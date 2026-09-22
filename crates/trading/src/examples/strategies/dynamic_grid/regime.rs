// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Causal completed-bar regime detection with a bounded, recoverable observation window.

use std::{collections::VecDeque, time::Duration};

use nautilus_indicators::{
    average::{MovingAverageFactory, MovingAverageType, rma::WilderMovingAverage},
    indicator::MovingAverage,
    momentum::{bb::BollingerBands, dm::DirectionalMovement},
    volatility::atr::AverageTrueRange,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::config::{GridConfig, RegimeAverage, TrendPolicy};

/// Market state used by the grid policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketRegime {
    /// Low directional strength and bounded volatility.
    Range,
    /// Strong positive slope.
    TrendUp,
    /// Strong negative slope.
    TrendDown,
    /// ATR, bandwidth or realized volatility exceeds its limit.
    HighVolatility,
    /// Indicators are warm, but ATR/price is below the configured entry threshold.
    LowVolatility,
    /// Warm-up, ambiguous regime or a risk restriction.
    #[default]
    Disabled,
}

/// Completed OHLC observation. Timestamps identify bar close, not bar open.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Observation {
    /// Close timestamp in nanoseconds.
    pub ts_ns: u64,
    /// Observed high.
    pub high: f64,
    /// Observed low.
    pub low: f64,
    /// Observed close.
    pub close: f64,
}

/// Signals based only on completed observations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegimeSnapshot {
    /// Most recent completed timestamp.
    pub ts_ns: u64,
    /// Whether every indicator is warm.
    pub initialized: bool,
    /// Current classification.
    pub regime: MarketRegime,
    /// ATR in price units.
    pub atr: f64,
    /// ADX on a 0..100 scale.
    pub adx: f64,
    /// (Upper - lower) / middle Bollinger bandwidth.
    pub bollinger_width: f64,
    /// MA fractional change per bar over the slope window.
    pub ma_slope: f64,
    /// Selected native moving average, replayed from the bounded completed-bar window.
    #[serde(default)]
    pub moving_average: f64,
    /// Current completed close / moving average minus one.
    #[serde(default)]
    pub price_ma_distance: f64,
    /// Standard deviation of completed-bar log returns, not annualized.
    pub realized_volatility: f64,
}

/// Bounded replay makes recovered indicator values identical to uninterrupted values.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegimeDetector {
    observations: VecDeque<Observation>,
    /// Latest causal signal snapshot.
    pub snapshot: RegimeSnapshot,
}

impl RegimeDetector {
    /// Recomputes persisted indicators from their completed-bar history before recovery.
    ///
    /// # Errors
    ///
    /// Rejects invalid observations, excessive history or a snapshot inconsistent with its inputs.
    pub(super) fn validate(&self, config: &GridConfig) -> anyhow::Result<()> {
        let capacity = 5 * (config.atr_period + config.adx_period)
            + config.ma_period
            + config.slope_period
            + config.volatility_period;
        anyhow::ensure!(
            self.observations.len() <= capacity,
            "Oversized recovered indicator history"
        );
        let mut replay = Self::default();
        for observation in &self.observations {
            replay.update(config, *observation)?;
        }
        let actual = &self.snapshot;
        let expected = &replay.snapshot;
        anyhow::ensure!(
            actual.ts_ns == expected.ts_ns
                && actual.initialized == expected.initialized
                && actual.regime == expected.regime,
            "Recovered regime metadata differs from completed observations"
        );
        for (a, b) in [
            (actual.atr, expected.atr),
            (actual.adx, expected.adx),
            (actual.bollinger_width, expected.bollinger_width),
            (actual.ma_slope, expected.ma_slope),
            (actual.moving_average, expected.moving_average),
            (actual.price_ma_distance, expected.price_ma_distance),
            (actual.realized_volatility, expected.realized_volatility),
        ] {
            // JSON may round floating-point indicator inputs by one ULP; prices/orders stay Decimal.
            anyhow::ensure!(
                a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-10 * b.abs().max(1.0),
                "Recovered indicator differs from completed observations"
            );
        }
        Ok(())
    }

    /// Applies the same trend restriction to outstanding orders and new intents.
    #[must_use]
    pub fn permits_order(&self, config: &GridConfig, buy: bool, level: i32) -> bool {
        let policy = self.policy(config);
        if buy && policy == TrendPolicy::Disable {
            return false;
        }
        let up = self.snapshot.regime == MarketRegime::TrendUp;
        let down = self.snapshot.regime == MarketRegime::TrendDown;
        if up && !buy && policy == TrendPolicy::LongOnly {
            return false;
        }
        // ReduceGrid 限制的是逆趋势方向：上涨时减少卖出层，下跌时减少买入层
        if policy == TrendPolicy::ReduceGrid && ((up && !buy) || (down && buy)) {
            let levels = config.grid_levels;
            return Decimal::from(level.unsigned_abs())
                <= (Decimal::from(levels as u64) * config.trend_level_fraction).ceil();
        }
        true
    }

    /// Updates on a strictly later completed bar.
    ///
    /// # Errors
    ///
    /// Returns an error on nonfinite/invalid OHLC or out-of-order observations.
    pub fn update(
        &mut self,
        config: &GridConfig,
        bar: Observation,
    ) -> anyhow::Result<&RegimeSnapshot> {
        anyhow::ensure!(
            [bar.high, bar.low, bar.close]
                .iter()
                .all(|x| x.is_finite() && *x > 0.0)
                && bar.low <= bar.close
                && bar.close <= bar.high,
            "Invalid completed bar"
        );
        anyhow::ensure!(
            self.observations
                .back()
                .is_none_or(|last| bar.ts_ns > last.ts_ns),
            "Bars must have strictly increasing close timestamps"
        );
        let capacity = 5 * (config.atr_period + config.adx_period)
            + config.ma_period
            + config.slope_period
            + config.volatility_period;
        self.observations.push_back(bar);
        if self.observations.len() > capacity {
            self.observations.pop_front();
        }

        // ponytail: bounded O(window) replay preserves exact restart parity without changing
        // shared indicator types; add serializable indicator checkpoints if profiling requires it.
        let mut atr = AverageTrueRange::new(
            config.atr_period,
            Some(MovingAverageType::Wilder),
            None,
            None,
        );
        let mut dm = DirectionalMovement::new(config.atr_period, Some(MovingAverageType::Wilder));
        let mut adx = WilderMovingAverage::new(config.adx_period, None);
        let mut ma = MovingAverageFactory::create(
            match config.regime_average {
                RegimeAverage::Simple => MovingAverageType::Simple,
                RegimeAverage::Exponential => MovingAverageType::Exponential,
            },
            config.ma_period,
        );
        let mut bb = BollingerBands::new(config.ma_period, config.bollinger_k, None);
        let mut averages = VecDeque::new();
        let mut returns = VecDeque::new();
        let mut previous: Option<f64> = None;
        for observation in &self.observations {
            atr.update_raw(observation.high, observation.low, observation.close);
            dm.update_raw(observation.high, observation.low);
            if dm.initialized {
                let sum = dm.pos + dm.neg;
                adx.update_raw(if sum > 0.0 {
                    100.0 * (dm.pos - dm.neg).abs() / sum
                } else {
                    0.0
                });
            }
            ma.update_raw(observation.close);
            bb.update_raw(observation.high, observation.low, observation.close);
            if ma.initialized() {
                averages.push_back(ma.value());
                if averages.len() > config.slope_period + 1 {
                    averages.pop_front();
                }
            }
            if let Some(previous) = previous {
                returns.push_back((observation.close / previous).ln());
                if returns.len() > config.volatility_period {
                    returns.pop_front();
                }
            }
            previous = Some(observation.close);
        }
        // ATR 暖机不等于可入场，ADX、均线斜率和收益率窗口必须同时就绪
        let initialized = atr.initialized
            && adx.initialized
            && bb.initialized
            && averages.len() == config.slope_period + 1
            && returns.len() == config.volatility_period;
        let slope = averages.front().map_or(0.0, |old| {
            (ma.value() / old - 1.0) / config.slope_period as f64
        });
        let mean = returns.iter().sum::<f64>() / returns.len().max(1) as f64;
        let volatility = (returns.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
            / returns.len().max(1) as f64)
            .sqrt();
        let width = if bb.middle > 0.0 {
            (bb.upper - bb.lower) / bb.middle
        } else {
            0.0
        };
        let mut snapshot = RegimeSnapshot {
            ts_ns: bar.ts_ns,
            initialized,
            regime: MarketRegime::Disabled,
            atr: atr.value,
            adx: adx.value,
            bollinger_width: width,
            ma_slope: slope,
            moving_average: ma.value(),
            price_ma_distance: if ma.value() > 0.0 {
                bar.close / ma.value() - 1.0
            } else {
                0.0
            },
            realized_volatility: volatility,
        };
        // 波动率先于趋势过滤，无法明确归类的行情保持 Disabled，不能当作震荡市
        if initialized {
            snapshot.regime = if config.enable_volatility_filter
                && (atr.value / bar.close > config.atr_pct_max
                    || volatility > config.realized_volatility_max
                    || width > config.bollinger_width_max)
            {
                MarketRegime::HighVolatility
            } else if config.enable_volatility_filter && atr.value / bar.close < config.atr_pct_min
            {
                MarketRegime::LowVolatility
            } else if !config.enable_trend_filter {
                MarketRegime::Range
            } else if adx.value >= config.adx_trend_min
                && slope > config.ma_slope_threshold
                && (!config.require_price_ma_confirmation
                    || snapshot.price_ma_distance > config.price_ma_confirmation_pct)
            {
                MarketRegime::TrendUp
            } else if adx.value >= config.adx_trend_min
                && slope < -config.ma_slope_threshold
                && (!config.require_price_ma_confirmation
                    || snapshot.price_ma_distance < -config.price_ma_confirmation_pct)
            {
                MarketRegime::TrendDown
            } else if adx.value <= config.adx_range_max
                && slope.abs() <= config.ma_slope_threshold
                && (bb.lower..=bb.upper).contains(&bar.close)
            {
                MarketRegime::Range
            } else {
                MarketRegime::Disabled
            };
        }
        self.snapshot = snapshot;
        Ok(&self.snapshot)
    }

    /// Effective directional policy for the most recent signal.
    #[must_use]
    pub fn policy(&self, config: &GridConfig) -> TrendPolicy {
        match self.snapshot.regime {
            MarketRegime::TrendUp => config.trend_up_policy,
            MarketRegime::TrendDown => config.trend_down_policy,
            MarketRegime::Range => TrendPolicy::Continue,
            _ => TrendPolicy::Disable,
        }
    }
}

// 使用完整纳秒年龄，避免整秒截断多放行近一秒；未来时间戳也不能作为当前信号
pub(super) fn is_fresh(ts_ns: u64, now: u64, max_age_secs: u64) -> bool {
    now.checked_sub(ts_ns)
        .is_some_and(|age| Duration::from_nanos(age) <= Duration::from_secs(max_age_secs))
}
