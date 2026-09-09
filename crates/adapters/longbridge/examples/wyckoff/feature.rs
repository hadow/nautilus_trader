// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! 把已完成 5 分钟 Bar 转换为因果、标准化的 Wyckoff 研究特征。
//!
//! Trading Range 永远由当前 Bar 之前的同日 Bar 构造；ATR、VWAP 和成交量只在当前 Bar
//! 收盘后更新。所有价格、成交量和波动阈值都采用比例或 ATR 标准化，避免写死美元值。

use std::collections::{HashMap, VecDeque};

use jiff::civil::Date;
use nautilus_indicators::{
    average::{MovingAverageType, vwap::VolumeWeightedAveragePrice},
    indicator::Indicator,
    volatility::atr::AverageTrueRange,
};
use nautilus_model::data::Bar;

const RTH_OPEN_MINUTE: u16 = 9 * 60 + 30;
const OPENING_RANGE_END_MINUTE: u16 = 10 * 60;

/// 一根已确认完成且附带纽约交易时段坐标的 Bar
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompletedBar {
    pub(crate) bar: Bar,
    pub(crate) date: Date,
    pub(crate) minute: u16,
}

/// 价格路径相对其净位移的状态
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrendRegime {
    Trending,
    Ranging,
    Transition,
}

/// 当前 ATR 百分比相对近期自身历史的位置
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VolatilityRegime {
    High,
    Normal,
    Low,
}

/// 一小时净位移与当日 VWAP 共同给出的方向偏置
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectionalBias {
    Bullish,
    Bearish,
    Neutral,
}

/// 只由当前及过去已完成 Bar 计算的市场状态
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MarketRegime {
    pub(crate) trend: TrendRegime,
    pub(crate) volatility: VolatilityRegime,
    pub(crate) bias: DirectionalBias,
    pub(crate) efficiency_ratio: f64,
    pub(crate) displacement_atr: f64,
}

/// 当前 Bar 之前的短期平衡区
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TradingRange {
    pub(crate) high: f64,
    pub(crate) low: f64,
    pub(crate) width_atr: f64,
    pub(crate) upper_touches: usize,
    pub(crate) lower_touches: usize,
}

impl TradingRange {
    /// 返回当前窗口是否同时形成可测试的上下边界
    pub(crate) fn is_clear(self) -> bool {
        (1.5..=5.0).contains(&self.width_atr) && self.upper_touches >= 2 && self.lower_touches >= 2
    }
}

/// 结构检测器消费的不可变特征快照
#[derive(Clone, Copy, Debug)]
pub(crate) struct WyckoffFeatures {
    pub(crate) completed: CompletedBar,
    pub(crate) atr: f64,
    pub(crate) relative_volume: Option<f64>,
    pub(crate) spread_atr: f64,
    pub(crate) body_atr: f64,
    pub(crate) close_location: f64,
    pub(crate) effort_result: Option<f64>,
    pub(crate) bullish_absorption: bool,
    pub(crate) bearish_absorption: bool,
    pub(crate) vwap: f64,
    pub(crate) trading_range: Option<TradingRange>,
    pub(crate) opening_range_high: Option<f64>,
    pub(crate) opening_range_low: Option<f64>,
    pub(crate) regime: MarketRegime,
}

impl WyckoffFeatures {
    /// 返回开盘区间中点，区间未完成时不提供值
    pub(crate) fn opening_range_midpoint(self) -> Option<f64> {
        Some((self.opening_range_high? + self.opening_range_low?) / 2.0)
    }
}

/// 使用价格效率与相对 ATR 判定市场环境
#[derive(Debug)]
pub(crate) struct MarketRegimeDetector {
    lookback: usize,
    atr_percent_history: VecDeque<f64>,
}

impl MarketRegimeDetector {
    pub(crate) fn new(lookback: usize) -> Self {
        Self {
            lookback,
            atr_percent_history: VecDeque::with_capacity(50),
        }
    }

    fn detect(&mut self, closes: &VecDeque<f64>, close: f64, atr: f64, vwap: f64) -> MarketRegime {
        let mut path = 0.0;
        let mut previous = None;
        let selected = closes
            .iter()
            .rev()
            .take(self.lookback.saturating_sub(1))
            .copied()
            .collect::<Vec<_>>();
        for value in selected.iter().rev().copied().chain(std::iter::once(close)) {
            if let Some(previous) = previous {
                path += f64::abs(value - previous);
            }
            previous = Some(value);
        }
        let start = selected.last().copied().unwrap_or(close);
        let displacement_atr = if atr > 0.0 {
            (close - start) / atr
        } else {
            0.0
        };
        let efficiency_ratio = if path > 0.0 {
            f64::abs(close - start) / path
        } else {
            0.0
        };
        let trend = if efficiency_ratio >= 0.35 {
            TrendRegime::Trending
        } else if efficiency_ratio <= 0.20 {
            TrendRegime::Ranging
        } else {
            TrendRegime::Transition
        };

        let atr_percent = if close > 0.0 { atr / close } else { 0.0 };
        let baseline = median(self.atr_percent_history.iter().copied());
        let volatility = match baseline {
            Some(baseline) if baseline > 0.0 && atr_percent >= baseline * 1.5 => {
                VolatilityRegime::High
            }
            Some(baseline) if baseline > 0.0 && atr_percent <= baseline * 0.7 => {
                VolatilityRegime::Low
            }
            _ => VolatilityRegime::Normal,
        };
        if atr_percent > 0.0 {
            push_bounded(&mut self.atr_percent_history, atr_percent, 50);
        }

        let bias = if displacement_atr >= 1.0 && close >= vwap {
            DirectionalBias::Bullish
        } else if displacement_atr <= -1.0 && close <= vwap {
            DirectionalBias::Bearish
        } else {
            DirectionalBias::Neutral
        };
        MarketRegime {
            trend,
            volatility,
            bias,
            efficiency_ratio,
            displacement_atr,
        }
    }
}

/// 维护 ATR、VWAP、分时 RVOL、开盘区间和短期 Trading Range
#[derive(Debug)]
pub(crate) struct WyckoffFeatureEngine {
    range_lookback: usize,
    atr: AverageTrueRange,
    vwap: VolumeWeightedAveragePrice,
    regime: MarketRegimeDetector,
    bars: VecDeque<Bar>,
    closes: VecDeque<f64>,
    volume_by_minute: HashMap<u16, VecDeque<f64>>,
    current_date: Option<Date>,
    opening_range_high: Option<f64>,
    opening_range_low: Option<f64>,
}

impl WyckoffFeatureEngine {
    pub(crate) fn new(atr_period: usize, range_lookback: usize, regime_lookback: usize) -> Self {
        Self {
            range_lookback,
            atr: AverageTrueRange::new(
                atr_period,
                Some(MovingAverageType::Wilder),
                Some(true),
                None,
            ),
            vwap: VolumeWeightedAveragePrice::new(),
            regime: MarketRegimeDetector::new(regime_lookback),
            bars: VecDeque::with_capacity(range_lookback),
            closes: VecDeque::with_capacity(regime_lookback),
            volume_by_minute: HashMap::new(),
            current_date: None,
            opening_range_high: None,
            opening_range_low: None,
        }
    }

    /// 消费一根已完成 Bar，并在更新滚动历史前固定其 Trading Range
    pub(crate) fn on_bar(&mut self, completed: CompletedBar) -> Option<WyckoffFeatures> {
        if self.current_date != Some(completed.date) {
            self.current_date = Some(completed.date);
            self.bars.clear();
            self.opening_range_high = None;
            self.opening_range_low = None;
        }

        let bar = completed.bar;
        let open = bar.open.as_f64();
        let high = bar.high.as_f64();
        let low = bar.low.as_f64();
        let close = bar.close.as_f64();
        let volume = bar.volume.as_f64();
        let prior_range = self.trading_range();

        self.atr.handle_bar(&bar);
        self.vwap.handle_bar(&bar);
        let atr = self.atr.value;
        let spread = high - low;
        let spread_atr = if atr > 0.0 { spread / atr } else { 0.0 };
        let body_atr = if atr > 0.0 {
            f64::abs(close - open) / atr
        } else {
            0.0
        };
        let close_location = if spread > 0.0 {
            (close - low) / spread
        } else {
            0.5
        };
        let relative_volume = self
            .volume_by_minute
            .get(&completed.minute)
            .filter(|history| history.len() >= 5)
            .and_then(|history| median(history.iter().copied()))
            .filter(|baseline| *baseline > 0.0)
            .map(|baseline| volume / baseline);
        let effort_result = relative_volume.map(|effort| effort / spread_atr.max(0.10));
        let bullish_absorption = relative_volume.is_some_and(|rvol| rvol >= 1.25)
            && spread_atr <= 0.80
            && close_location >= 0.65;
        let bearish_absorption = relative_volume.is_some_and(|rvol| rvol >= 1.25)
            && spread_atr <= 0.80
            && close_location <= 0.35;

        if (RTH_OPEN_MINUTE..OPENING_RANGE_END_MINUTE).contains(&completed.minute) {
            self.opening_range_high = Some(
                self.opening_range_high
                    .map_or(high, |current| current.max(high)),
            );
            self.opening_range_low = Some(
                self.opening_range_low
                    .map_or(low, |current| current.min(low)),
            );
        }
        let opening_range_complete = completed.minute >= OPENING_RANGE_END_MINUTE - 5;
        let regime = self
            .regime
            .detect(&self.closes, close, atr, self.vwap.value);
        let initialized = self.atr.initialized()
            && self.vwap.initialized()
            && self.closes.len() >= self.regime.lookback.saturating_sub(1);
        let snapshot = initialized.then_some(WyckoffFeatures {
            completed,
            atr,
            relative_volume,
            spread_atr,
            body_atr,
            close_location,
            effort_result,
            bullish_absorption,
            bearish_absorption,
            vwap: self.vwap.value,
            trading_range: prior_range,
            opening_range_high: opening_range_complete
                .then_some(self.opening_range_high)
                .flatten(),
            opening_range_low: opening_range_complete
                .then_some(self.opening_range_low)
                .flatten(),
            regime,
        });

        push_bounded(&mut self.bars, bar, self.range_lookback);
        push_bounded(&mut self.closes, close, self.regime.lookback);
        push_bounded(
            self.volume_by_minute.entry(completed.minute).or_default(),
            volume,
            20,
        );
        snapshot
    }

    fn trading_range(&self) -> Option<TradingRange> {
        if self.bars.len() < self.range_lookback || self.atr.value <= 0.0 {
            return None;
        }
        let high = self
            .bars
            .iter()
            .map(|bar| bar.high.as_f64())
            .fold(f64::NEG_INFINITY, f64::max);
        let low = self
            .bars
            .iter()
            .map(|bar| bar.low.as_f64())
            .fold(f64::INFINITY, f64::min);
        let tolerance = self.atr.value * 0.20;
        let upper_touches = self
            .bars
            .iter()
            .filter(|bar| bar.high.as_f64() >= high - tolerance)
            .count();
        let lower_touches = self
            .bars
            .iter()
            .filter(|bar| bar.low.as_f64() <= low + tolerance)
            .count();
        Some(TradingRange {
            high,
            low,
            width_atr: (high - low) / self.atr.value,
            upper_touches,
            lower_touches,
        })
    }
}

fn push_bounded<T>(values: &mut VecDeque<T>, value: T, limit: usize) {
    if values.len() == limit {
        values.pop_front();
    }
    values.push_back(value);
}

fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values = values.filter(|value| value.is_finite()).collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[middle - 1] + values[middle]) / 2.0)
    } else {
        Some(values[middle])
    }
}

#[cfg(test)]
mod tests {
    use nautilus_core::UnixNanos;
    use nautilus_model::{
        data::BarType,
        types::{Price, Quantity},
    };
    use rstest::rstest;

    use super::*;

    fn bar(high: &str, low: &str, close: &str, volume: u64, index: u64) -> Bar {
        Bar::new(
            BarType::from("AAPL.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            Price::from(close),
            Price::from(high),
            Price::from(low),
            Price::from(close),
            Quantity::from(volume),
            UnixNanos::from(index * 300_000_000_000),
            UnixNanos::from((index + 1) * 300_000_000_000),
        )
    }

    #[rstest]
    fn trading_range_excludes_the_current_bar() {
        let date = "2026-08-03".parse().unwrap();
        let mut engine = WyckoffFeatureEngine::new(2, 3, 2);
        for (index, (high, low)) in [("101", "99"), ("102", "99"), ("101", "98")]
            .into_iter()
            .enumerate()
        {
            engine.on_bar(CompletedBar {
                bar: bar(high, low, "100", 100, index as u64),
                date,
                minute: RTH_OPEN_MINUTE + index as u16 * 5,
            });
        }

        let features = engine
            .on_bar(CompletedBar {
                bar: bar("110", "90", "100", 100, 3),
                date,
                minute: RTH_OPEN_MINUTE + 15,
            })
            .unwrap();
        let range = features.trading_range.unwrap();

        assert_eq!(range.high, 102.0);
        assert_eq!(range.low, 98.0);
    }

    #[rstest]
    fn same_minute_rvol_uses_only_prior_sessions() {
        let mut engine = WyckoffFeatureEngine::new(2, 2, 2);
        for day in 1..=6 {
            let date = format!("2026-08-{day:02}").parse().unwrap();
            let features = engine.on_bar(CompletedBar {
                bar: bar("101", "99", "100", if day == 6 { 200 } else { 100 }, day),
                date,
                minute: RTH_OPEN_MINUTE,
            });
            if day == 6 {
                assert_eq!(features.unwrap().relative_volume, Some(2.0));
            }
        }
    }
}
