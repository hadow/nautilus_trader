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

//! 在因果特征上识别 Spring、SOS、LPS 及其空头对应结构。
//!
//! 每个 Setup 只在确认 Bar 收盘时生成。Spring Test、LPS 和 LPS Confirmation 都由显式
//! 有限状态推进，不会回写历史标签，也不会通过未来 Bar 重定义此前的 Trading Range。

use nautilus_core::UnixNanos;
use nautilus_model::enums::OrderSide;

use super::feature::{DirectionalBias, TrendRegime, VolatilityRegime, WyckoffFeatures};

/// 消融实验按顺序逐层增加过滤条件
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StudyLayer {
    Base,
    Volume,
    Vwap,
    Regime,
    RelativeVolume,
    OpeningRange,
}

impl StudyLayer {
    pub(crate) const ALL: [Self; 6] = [
        Self::Base,
        Self::Volume,
        Self::Vwap,
        Self::Regime,
        Self::RelativeVolume,
        Self::OpeningRange,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Base => "base_wyckoff",
            Self::Volume => "plus_volume",
            Self::Vwap => "plus_vwap",
            Self::Regime => "plus_regime",
            Self::RelativeVolume => "plus_rvol",
            Self::OpeningRange => "plus_opening_range",
        }
    }
}

/// Prompt 要求比较的五种入场确认级别
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EntryVariant {
    SpringImmediate,
    SpringTest,
    SosBreakout,
    SosLps,
    LpsConfirmation,
}

impl EntryVariant {
    pub(crate) const ALL: [Self; 5] = [
        Self::SpringImmediate,
        Self::SpringTest,
        Self::SosBreakout,
        Self::SosLps,
        Self::LpsConfirmation,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SpringImmediate => "spring_immediate",
            Self::SpringTest => "spring_test",
            Self::SosBreakout => "sos_breakout",
            Self::SosLps => "sos_lps",
            Self::LpsConfirmation => "lps_confirmation",
        }
    }
}

/// 一个可在下一根 Bar 执行的机械化信号
#[derive(Clone, Copy, Debug)]
pub(crate) struct WyckoffSignal {
    pub(crate) side: OrderSide,
    pub(crate) variant: EntryVariant,
    pub(crate) stop_anchor: f64,
    pub(crate) atr: f64,
    pub(crate) regime: TrendRegime,
    pub(crate) volatility: VolatilityRegime,
    pub(crate) minute: u16,
    pub(crate) ts_event: UnixNanos,
    pub(crate) evidence_count: u8,
}

/// 研究阶段固定且不通过 OOS 调整的结构阈值
#[derive(Clone, Copy, Debug)]
pub(crate) struct WyckoffRules {
    pub(crate) minimum_sweep_atr: f64,
    pub(crate) maximum_sweep_atr: f64,
    pub(crate) breakout_atr: f64,
    pub(crate) spring_test_bars: u8,
    pub(crate) lps_bars: u8,
    pub(crate) confirmation_bars: u8,
}

impl Default for WyckoffRules {
    fn default() -> Self {
        Self {
            minimum_sweep_atr: 0.05,
            maximum_sweep_atr: 0.50,
            breakout_atr: 0.10,
            spring_test_bars: 4,
            lps_bars: 6,
            confirmation_bars: 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingSpring {
    side: OrderSide,
    level: f64,
    extreme: f64,
    volume: f64,
    age: u8,
}

#[derive(Clone, Copy, Debug)]
struct PendingSos {
    side: OrderSide,
    level: f64,
    volume: f64,
    age: u8,
}

#[derive(Clone, Copy, Debug)]
struct PendingLps {
    side: OrderSide,
    extreme: f64,
    confirmation_level: f64,
    age: u8,
}

/// 维护 Wyckoff Setup 的最小有限状态机
#[derive(Debug)]
pub(crate) struct WyckoffStructureDetector {
    rules: WyckoffRules,
    layer: StudyLayer,
    pending_spring: Option<PendingSpring>,
    pending_sos: Option<PendingSos>,
    pending_lps: Option<PendingLps>,
}

impl WyckoffStructureDetector {
    pub(crate) fn new(rules: WyckoffRules, layer: StudyLayer) -> Self {
        Self {
            rules,
            layer,
            pending_spring: None,
            pending_sos: None,
            pending_lps: None,
        }
    }

    /// 处理一根已完成 Bar，并返回本 Bar 收盘后才成立的候选信号
    pub(crate) fn on_bar(&mut self, features: WyckoffFeatures) -> Vec<WyckoffSignal> {
        let mut signals = Vec::with_capacity(2);
        self.advance_lps_confirmation(features, &mut signals);
        self.advance_sos(features, &mut signals);
        self.advance_spring(features, &mut signals);
        self.detect_new_setups(features, &mut signals);
        signals
    }

    /// 日内结构不能跨交易日延续。
    pub(crate) fn reset_session(&mut self) {
        self.pending_spring = None;
        self.pending_sos = None;
        self.pending_lps = None;
    }

    fn advance_spring(&mut self, features: WyckoffFeatures, signals: &mut Vec<WyckoffSignal>) {
        let Some(mut pending) = self.pending_spring.take() else {
            return;
        };
        pending.age += 1;
        let bar = features.completed.bar;
        let volume_contracts = bar.volume.as_f64() <= pending.volume * 0.80;
        let confirmed = match pending.side {
            OrderSide::Buy => {
                bar.low.as_f64() <= pending.level + features.atr * 0.25
                    && bar.low.as_f64() > pending.extreme
                    && bar.close.as_f64() > pending.level
                    && features.close_location >= 0.55
            }
            OrderSide::Sell => {
                bar.high.as_f64() >= pending.level - features.atr * 0.25
                    && bar.high.as_f64() < pending.extreme
                    && bar.close.as_f64() < pending.level
                    && features.close_location <= 0.45
            }
            OrderSide::NoOrderSide => false,
        } && (self.layer < StudyLayer::Volume || volume_contracts)
            && (self.layer < StudyLayer::RelativeVolume
                || features
                    .relative_volume
                    .is_some_and(|relative_volume| relative_volume <= 1.0))
            && self.common_filters(pending.side, features, false, None);
        if confirmed {
            signals.push(self.signal(
                pending.side,
                EntryVariant::SpringTest,
                features,
                pending.extreme,
                5,
            ));
        } else if pending.age < self.rules.spring_test_bars {
            self.pending_spring = Some(pending);
        }
    }

    fn advance_sos(&mut self, features: WyckoffFeatures, signals: &mut Vec<WyckoffSignal>) {
        let Some(mut pending) = self.pending_sos.take() else {
            return;
        };
        pending.age += 1;
        let bar = features.completed.bar;
        let volume_contracts = bar.volume.as_f64() <= pending.volume * 0.80;
        let lps = match pending.side {
            OrderSide::Buy => {
                bar.low.as_f64() <= pending.level + features.atr * 0.40
                    && bar.close.as_f64() >= pending.level
                    && features.close_location >= 0.55
            }
            OrderSide::Sell => {
                bar.high.as_f64() >= pending.level - features.atr * 0.40
                    && bar.close.as_f64() <= pending.level
                    && features.close_location <= 0.45
            }
            OrderSide::NoOrderSide => false,
        } && features.spread_atr <= 1.0
            && (self.layer < StudyLayer::Volume || volume_contracts)
            && (self.layer < StudyLayer::RelativeVolume
                || features
                    .relative_volume
                    .is_some_and(|relative_volume| relative_volume <= 1.0))
            && self.common_filters(pending.side, features, true, None);
        if lps {
            let extreme = match pending.side {
                OrderSide::Buy => bar.low.as_f64(),
                OrderSide::Sell => bar.high.as_f64(),
                OrderSide::NoOrderSide => return,
            };
            signals.push(self.signal(pending.side, EntryVariant::SosLps, features, extreme, 5));
            self.pending_lps = Some(PendingLps {
                side: pending.side,
                extreme,
                confirmation_level: match pending.side {
                    OrderSide::Buy => bar.high.as_f64(),
                    OrderSide::Sell => bar.low.as_f64(),
                    OrderSide::NoOrderSide => unreachable!(),
                },
                age: 0,
            });
        } else if pending.age < self.rules.lps_bars {
            self.pending_sos = Some(pending);
        }
    }

    fn advance_lps_confirmation(
        &mut self,
        features: WyckoffFeatures,
        signals: &mut Vec<WyckoffSignal>,
    ) {
        let Some(mut pending) = self.pending_lps.take() else {
            return;
        };
        pending.age += 1;
        let close = features.completed.bar.close.as_f64();
        let confirmed = match pending.side {
            OrderSide::Buy => {
                close > pending.confirmation_level
                    && features.body_atr >= 0.25
                    && features.close_location >= 0.60
            }
            OrderSide::Sell => {
                close < pending.confirmation_level
                    && features.body_atr >= 0.25
                    && features.close_location <= 0.40
            }
            OrderSide::NoOrderSide => false,
        } && self.common_filters(pending.side, features, true, Some(1.0));
        if confirmed {
            signals.push(self.signal(
                pending.side,
                EntryVariant::LpsConfirmation,
                features,
                pending.extreme,
                5,
            ));
        } else if pending.age < self.rules.confirmation_bars {
            self.pending_lps = Some(pending);
        }
    }

    fn detect_new_setups(&mut self, features: WyckoffFeatures, signals: &mut Vec<WyckoffSignal>) {
        let Some(range) = features.trading_range.filter(|range| range.is_clear()) else {
            return;
        };
        let bar = features.completed.bar;
        let low = bar.low.as_f64();
        let high = bar.high.as_f64();
        let close = bar.close.as_f64();
        let volume = bar.volume.as_f64();
        let long_sweep = (range.low - low) / features.atr;
        let short_sweep = (high - range.high) / features.atr;
        let spring_long = low < range.low
            && close > range.low
            && (self.rules.minimum_sweep_atr..=self.rules.maximum_sweep_atr).contains(&long_sweep)
            && features.close_location >= 0.65
            && (self.layer < StudyLayer::Volume || features.bullish_absorption)
            && self.common_filters(OrderSide::Buy, features, false, Some(1.25));
        let spring_short = high > range.high
            && close < range.high
            && (self.rules.minimum_sweep_atr..=self.rules.maximum_sweep_atr).contains(&short_sweep)
            && features.close_location <= 0.35
            && (self.layer < StudyLayer::Volume || features.bearish_absorption)
            && self.common_filters(OrderSide::Sell, features, false, Some(1.25));
        if spring_long || spring_short {
            let side = if spring_long {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            };
            let level = if spring_long { range.low } else { range.high };
            let extreme = if spring_long { low } else { high };
            let evidence_count = 3
                + u8::from(self.layer >= StudyLayer::Volume)
                + u8::from(self.layer >= StudyLayer::OpeningRange);
            signals.push(self.signal(
                side,
                EntryVariant::SpringImmediate,
                features,
                extreme,
                evidence_count,
            ));
            self.pending_spring = Some(PendingSpring {
                side,
                level,
                extreme,
                volume,
                age: 0,
            });
        }

        let sos_long = (close - range.high) / features.atr >= self.rules.breakout_atr
            && features.spread_atr >= 1.0
            && features.body_atr >= 0.60
            && features.close_location >= 0.70
            && (self.layer < StudyLayer::Volume
                || features.effort_result.is_some_and(|value| value >= 0.80))
            && self.common_filters(OrderSide::Buy, features, true, Some(1.40));
        let sos_short = (range.low - close) / features.atr >= self.rules.breakout_atr
            && features.spread_atr >= 1.0
            && features.body_atr >= 0.60
            && features.close_location <= 0.30
            && (self.layer < StudyLayer::Volume
                || features.effort_result.is_some_and(|value| value >= 0.80))
            && self.common_filters(OrderSide::Sell, features, true, Some(1.40));
        if sos_long || sos_short {
            let side = if sos_long {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            };
            let level = if sos_long { range.high } else { range.low };
            let stop_anchor = match side {
                OrderSide::Buy => low.min(level - features.atr * 0.25),
                OrderSide::Sell => high.max(level + features.atr * 0.25),
                OrderSide::NoOrderSide => unreachable!(),
            };
            signals.push(self.signal(side, EntryVariant::SosBreakout, features, stop_anchor, 4));
            self.pending_sos = Some(PendingSos {
                side,
                level,
                volume,
                age: 0,
            });
        }
    }

    fn common_filters(
        &self,
        side: OrderSide,
        features: WyckoffFeatures,
        continuation: bool,
        minimum_relative_volume: Option<f64>,
    ) -> bool {
        let close = features.completed.bar.close.as_f64();
        if self.layer >= StudyLayer::Vwap {
            let tolerance = if continuation {
                0.0
            } else {
                features.atr * 0.25
            };
            let vwap_ok = match side {
                OrderSide::Buy => close >= features.vwap - tolerance,
                OrderSide::Sell => close <= features.vwap + tolerance,
                OrderSide::NoOrderSide => false,
            };
            if !vwap_ok {
                return false;
            }
        }
        if self.layer >= StudyLayer::Regime {
            if features.regime.volatility == VolatilityRegime::Low {
                return false;
            }
            let regime_ok = if continuation {
                matches!(features.regime.trend, TrendRegime::Trending)
                    && matches!(
                        (side, features.regime.bias),
                        (OrderSide::Buy, DirectionalBias::Bullish)
                            | (OrderSide::Sell, DirectionalBias::Bearish)
                    )
            } else {
                !matches!(
                    (side, features.regime.trend, features.regime.bias),
                    (
                        OrderSide::Buy,
                        TrendRegime::Trending,
                        DirectionalBias::Bearish
                    ) | (
                        OrderSide::Sell,
                        TrendRegime::Trending,
                        DirectionalBias::Bullish
                    )
                )
            };
            if !regime_ok {
                return false;
            }
        }
        if self.layer >= StudyLayer::RelativeVolume
            && let Some(minimum) = minimum_relative_volume
            && !features
                .relative_volume
                .is_some_and(|relative_volume| relative_volume >= minimum)
        {
            return false;
        }
        if self.layer >= StudyLayer::OpeningRange {
            let Some(midpoint) = features.opening_range_midpoint() else {
                return false;
            };
            let opening_range_ok = match side {
                OrderSide::Buy => close >= midpoint,
                OrderSide::Sell => close <= midpoint,
                OrderSide::NoOrderSide => false,
            };
            if !opening_range_ok {
                return false;
            }
        }
        true
    }

    fn signal(
        &self,
        side: OrderSide,
        variant: EntryVariant,
        features: WyckoffFeatures,
        stop_anchor: f64,
        evidence_count: u8,
    ) -> WyckoffSignal {
        WyckoffSignal {
            side,
            variant,
            stop_anchor,
            atr: features.atr,
            regime: features.regime.trend,
            volatility: features.regime.volatility,
            minute: features.completed.minute,
            ts_event: features.completed.bar.ts_event,
            evidence_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::Date;
    use nautilus_model::{
        data::{Bar, BarType},
        types::{Price, Quantity},
    };
    use rstest::rstest;

    use super::*;
    use crate::wyckoff::feature::{CompletedBar, MarketRegime, TradingRange};

    fn features(high: &str, low: &str, close: &str, close_location: f64) -> WyckoffFeatures {
        let bar = Bar::new(
            BarType::from("AAPL.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            Price::from("100.0"),
            Price::from(high),
            Price::from(low),
            Price::from(close),
            Quantity::from(200),
            UnixNanos::from(1),
            UnixNanos::from(2),
        );
        WyckoffFeatures {
            completed: CompletedBar {
                bar,
                date: Date::constant(2026, 8, 3),
                minute: 11 * 60,
            },
            atr: 2.0,
            relative_volume: Some(2.0),
            spread_atr: (bar.high.as_f64() - bar.low.as_f64()) / 2.0,
            body_atr: f64::abs(bar.close.as_f64() - bar.open.as_f64()) / 2.0,
            close_location,
            effort_result: Some(1.0),
            bullish_absorption: true,
            bearish_absorption: true,
            vwap: 100.0,
            trading_range: Some(TradingRange {
                high: 105.0,
                low: 95.0,
                width_atr: 5.0,
                upper_touches: 2,
                lower_touches: 2,
            }),
            opening_range_high: Some(104.0),
            opening_range_low: Some(96.0),
            regime: MarketRegime {
                trend: TrendRegime::Ranging,
                volatility: VolatilityRegime::Normal,
                bias: DirectionalBias::Neutral,
                efficiency_ratio: 0.1,
                displacement_atr: 0.0,
            },
        }
    }

    #[rstest]
    fn spring_is_confirmed_only_after_the_reclaiming_close() {
        let mut detector = WyckoffStructureDetector::new(WyckoffRules::default(), StudyLayer::Base);

        let signals = detector.on_bar(features("101.0", "94.5", "96.0", 0.75));

        assert!(signals.iter().any(|signal| {
            signal.side == OrderSide::Buy && signal.variant == EntryVariant::SpringImmediate
        }));
    }

    #[rstest]
    fn lps_cannot_occur_on_the_sos_bar() {
        let mut detector = WyckoffStructureDetector::new(WyckoffRules::default(), StudyLayer::Base);
        let mut sos = features("108.0", "100.0", "107.0", 0.875);
        sos.body_atr = 3.5;
        sos.spread_atr = 4.0;

        let signals = detector.on_bar(sos);

        assert!(
            signals
                .iter()
                .any(|signal| signal.variant == EntryVariant::SosBreakout)
        );
        assert!(
            !signals
                .iter()
                .any(|signal| signal.variant == EntryVariant::SosLps)
        );
    }
}
