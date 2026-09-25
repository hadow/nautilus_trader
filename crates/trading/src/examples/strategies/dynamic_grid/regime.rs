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

//! 仅使用已完成 K 线的因果市场状态识别，并保留有界历史以支持精确恢复。

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

/// 网格策略使用的市场状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketRegime {
    /// 方向性较弱，且波动率处于允许范围。
    Range,
    /// 方向强度和正斜率均达到上升趋势阈值。
    TrendUp,
    /// 方向强度和负斜率均达到下降趋势阈值。
    TrendDown,
    /// ATR、布林带宽度或已实现波动率超过上限。
    HighVolatility,
    /// 指标已预热，但 ATR/价格低于允许入场的下限。
    LowVolatility,
    /// 指标预热中、状态无法明确分类，或风险条件禁止交易。
    #[default]
    Disabled,
}

/// 已完成的 OHLC 观测；时间戳表示 K 线收盘而非开盘时刻。
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Observation {
    /// 收盘时间戳，单位为纳秒。
    pub ts_ns: u64,
    /// 已观测最高价。
    pub high: f64,
    /// 已观测最低价。
    pub low: f64,
    /// 已观测收盘价。
    pub close: f64,
}

/// 完全基于已完成观测生成的市场状态快照。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegimeSnapshot {
    /// 最近一根已完成 K 线的时间戳。
    pub ts_ns: u64,
    /// 所有指标是否均已完成预热。
    pub initialized: bool,
    /// 当前市场状态分类。
    pub regime: MarketRegime,
    /// 首个决定分类的条件；旧检查点缺失时保持 None，不虚构历史归因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 以价格单位表示的 ATR。
    pub atr: f64,
    /// 取值范围为 0–100 的 ADX。
    pub adx: f64,
    /// 布林带宽度，即 `(上轨 - 下轨) / 中轨`。
    pub bollinger_width: f64,
    /// 斜率窗口内，移动平均每根 K 线的比例变化。
    pub ma_slope: f64,
    /// 从有界已完成 K 线窗口重放得到的移动平均值。
    #[serde(default)]
    pub moving_average: f64,
    /// 当前已完成收盘价相对移动平均的偏离比例。
    #[serde(default)]
    pub price_ma_distance: f64,
    /// 已完成 K 线对数收益率的标准差，不做年化。
    pub realized_volatility: f64,
}

/// 通过有界历史重放，使恢复后的指标值与未中断运行保持一致。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegimeDetector {
    observations: VecDeque<Observation>,
    /// 最近一次因果信号快照。
    pub snapshot: RegimeSnapshot,
}

impl RegimeDetector {
    /// 恢复前使用持久化的已完成 K 线历史重新计算指标。
    ///
    /// # Errors
    ///
    /// 观测无效、历史超出上限，或快照与输入历史不一致时拒绝恢复。
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
                && actual.regime == expected.regime
                && actual
                    .reason
                    .as_ref()
                    .is_none_or(|reason| Some(reason) == expected.reason.as_ref()),
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
            // JSON 可能使浮点指标输入产生一个 ULP 的舍入误差；价格和订单金额仍使用 Decimal。
            anyhow::ensure!(
                a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-10 * b.abs().max(1.0),
                "Recovered indicator differs from completed observations"
            );
        }
        Ok(())
    }

    /// 对未完成订单和新订单意图应用同一套趋势限制。
    #[must_use]
    pub fn permits_order(&self, config: &GridConfig, buy: bool, level: i32) -> bool {
        self.snapshot.regime.permits_order(config, buy, level)
    }

    /// 只接受时间戳严格递增的已完成 K 线。
    ///
    /// # Errors
    ///
    /// OHLC 非有限、价格关系无效或观测乱序时返回错误。
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

        // ponytail: 有界 O(window) 重放无需修改共享指标类型即可保证重启一致性；
        // 只有性能采样证明这里成为瓶颈时，才增加可序列化的指标检查点。
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
            reason: None,
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
        // 判定与归因共用同一入口，不在报告层另写一套近似分类器
        let (regime, reason) = classify(config, &snapshot, bar.close, bb.lower, bb.upper);
        snapshot.regime = regime;
        snapshot.reason = Some(reason.to_string());
        self.snapshot = snapshot;
        Ok(&self.snapshot)
    }

    /// 最近一次信号对应的实际趋势策略。
    #[must_use]
    pub fn policy(&self, config: &GridConfig) -> TrendPolicy {
        self.snapshot.regime.policy(config)
    }
}

impl MarketRegime {
    pub(super) fn policy(self, config: &GridConfig) -> TrendPolicy {
        match self {
            Self::TrendUp => config.trend_up_policy,
            Self::TrendDown => config.trend_down_policy,
            Self::Range => TrendPolicy::Continue,
            _ => TrendPolicy::Disable,
        }
    }

    pub(super) fn permits_order(self, config: &GridConfig, buy: bool, level: i32) -> bool {
        let policy = self.policy(config);
        if buy && policy == TrendPolicy::Disable {
            return false;
        }
        let up = self == Self::TrendUp;
        let down = self == Self::TrendDown;
        if up && !buy && policy == TrendPolicy::LongOnly {
            return false;
        }
        // 原始分类与确认状态复用相同订单政策，避免只修新买单而遗漏撤单/覆盖卖出。
        if policy == TrendPolicy::ReduceGrid && ((up && !buy) || (down && buy)) {
            return Decimal::from(level.unsigned_abs())
                <= (Decimal::from(config.grid_levels as u64) * config.trend_level_fraction).ceil();
        }
        true
    }
}

// 保持原判定优先级；Disabled 只是未分类，不应在研究中全部当作“危险行情”
fn classify(
    config: &GridConfig,
    signal: &RegimeSnapshot,
    close: f64,
    lower: f64,
    upper: f64,
) -> (MarketRegime, &'static str) {
    if !signal.initialized {
        return (MarketRegime::Disabled, "WARMUP");
    }
    if config.enable_volatility_filter {
        if signal.atr / close > config.atr_pct_max {
            return (MarketRegime::HighVolatility, "HIGH_ATR");
        }
        if signal.realized_volatility > config.realized_volatility_max {
            return (MarketRegime::HighVolatility, "HIGH_REALIZED_VOLATILITY");
        }
        if signal.bollinger_width > config.bollinger_width_max {
            return (MarketRegime::HighVolatility, "WIDE_BOLLINGER_BANDS");
        }
        if signal.atr / close < config.atr_pct_min {
            return (MarketRegime::LowVolatility, "LOW_ATR");
        }
    }
    if !config.enable_trend_filter {
        return (MarketRegime::Range, "TREND_FILTER_DISABLED");
    }
    if signal.adx >= config.adx_trend_min {
        let up = signal.ma_slope > config.ma_slope_threshold;
        let down = signal.ma_slope < -config.ma_slope_threshold;
        if !up && !down {
            return (MarketRegime::Disabled, "TREND_SLOPE_TOO_SMALL");
        }
        let confirmed = if up {
            signal.price_ma_distance > config.price_ma_confirmation_pct
        } else {
            signal.price_ma_distance < -config.price_ma_confirmation_pct
        };
        if config.require_price_ma_confirmation && !confirmed {
            return (MarketRegime::Disabled, "PRICE_MA_CONFLICT");
        }
        return if up {
            (MarketRegime::TrendUp, "TREND_UP")
        } else {
            (MarketRegime::TrendDown, "TREND_DOWN")
        };
    }
    if signal.adx.is_nan() || signal.adx > config.adx_range_max {
        return (MarketRegime::Disabled, "ADX_TRANSITION");
    }
    if signal.ma_slope.is_nan() || signal.ma_slope.abs() > config.ma_slope_threshold {
        return (MarketRegime::Disabled, "RANGE_SLOPE_TOO_LARGE");
    }
    if !(lower..=upper).contains(&close) {
        return (MarketRegime::Disabled, "OUTSIDE_BOLLINGER_BANDS");
    }
    (MarketRegime::Range, "RANGE")
}

// 使用完整纳秒年龄，避免整秒截断多放行近一秒；未来时间戳也不能作为当前信号
pub(super) fn is_fresh(ts_ns: u64, now: u64, max_age_secs: u64) -> bool {
    now.checked_sub(ts_ns)
        .is_some_and(|age| Duration::from_nanos(age) <= Duration::from_secs(max_age_secs))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(20.0, 0.001, 100.0, MarketRegime::Range, "RANGE")]
    #[case(21.0, 0.0, 100.0, MarketRegime::Disabled, "ADX_TRANSITION")]
    #[case(24.99, 0.002, 100.0, MarketRegime::Disabled, "ADX_TRANSITION")]
    #[case(25.0, 0.001, 100.0, MarketRegime::Disabled, "TREND_SLOPE_TOO_SMALL")]
    #[case(30.0, -0.0002, 100.0, MarketRegime::Disabled, "TREND_SLOPE_TOO_SMALL")]
    #[case(25.0, 0.0011, 100.0, MarketRegime::TrendUp, "TREND_UP")]
    #[case(25.0, -0.0011, 100.0, MarketRegime::TrendDown, "TREND_DOWN")]
    #[case(20.0, 0.0011, 100.0, MarketRegime::Disabled, "RANGE_SLOPE_TOO_LARGE")]
    #[case(20.0, 0.0, 98.0, MarketRegime::Disabled, "OUTSIDE_BOLLINGER_BANDS")]
    fn regime_research_classification_boundaries(
        #[case] adx: f64,
        #[case] slope: f64,
        #[case] close: f64,
        #[case] expected: MarketRegime,
        #[case] reason: &str,
    ) {
        let signal = RegimeSnapshot {
            initialized: true,
            atr: 0.2,
            adx,
            ma_slope: slope,
            ..Default::default()
        };
        assert_eq!(
            classify(&GridConfig::default(), &signal, close, 99.0, 101.0),
            (expected, reason)
        );
    }

    #[rstest]
    fn regime_research_volatility_and_price_confirmation_remain_independent() {
        let config = GridConfig {
            require_price_ma_confirmation: true,
            ..Default::default()
        };
        let mut signal = RegimeSnapshot {
            initialized: true,
            atr: 0.2,
            adx: 30.0,
            ma_slope: -0.002,
            price_ma_distance: 0.001,
            ..Default::default()
        };
        assert_eq!(
            classify(&config, &signal, 100.0, 99.0, 101.0),
            (MarketRegime::Disabled, "PRICE_MA_CONFLICT")
        );
        signal.atr = 6.0;
        assert_eq!(
            classify(&config, &signal, 100.0, 99.0, 101.0),
            (MarketRegime::HighVolatility, "HIGH_ATR")
        );
        signal.initialized = false;
        assert_eq!(
            classify(&config, &signal, 100.0, 99.0, 101.0),
            (MarketRegime::Disabled, "WARMUP")
        );
    }

    #[rstest]
    fn regime_research_diagnostics_preserve_recovery_and_do_not_change_decisions() {
        use rust_decimal_macros::dec;

        use super::super::diagnostics::GridDiagnostics;

        let config = GridConfig::default();
        let mut detector = RegimeDetector::default();
        let mut diagnostics = GridDiagnostics::default();
        for ts_ns in 1..=200 {
            detector
                .update(
                    &config,
                    Observation {
                        ts_ns,
                        high: 100.1,
                        low: 99.9,
                        close: 100.0,
                    },
                )
                .unwrap();
            let before = serde_json::to_value(&detector).unwrap();
            diagnostics.observe_spacing(&config, &detector, None, dec!(100), None);
            assert_eq!(before, serde_json::to_value(&detector).unwrap());
        }
        let recorded = diagnostics.spacing.last().unwrap().regime.as_ref().unwrap();
        assert_eq!(recorded.ts_ns, detector.snapshot.ts_ns);
        assert_eq!(recorded.reason.as_deref(), Some("RANGE"));
        let mut legacy = serde_json::to_value(&detector).unwrap();
        legacy["snapshot"].as_object_mut().unwrap().remove("reason");
        let mut restored: RegimeDetector = serde_json::from_value(legacy).unwrap();
        restored.validate(&config).unwrap();
        let next = Observation {
            ts_ns: 201,
            high: 100.2,
            low: 100.0,
            close: 100.1,
        };
        detector.update(&config, next).unwrap();
        restored.update(&config, next).unwrap();
        assert_eq!(
            serde_json::to_value(&detector).unwrap(),
            serde_json::to_value(&restored).unwrap()
        );
        restored.snapshot.reason = Some("FORGED".to_string());
        assert!(restored.validate(&config).is_err());
    }
}
