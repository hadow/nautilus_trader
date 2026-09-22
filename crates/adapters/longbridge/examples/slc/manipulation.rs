// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Liquidity-sweep and inverse fair-value-gap intraday reversal signal model.
//!
//! Completed one-hour bars identify causal swing liquidity. Five-minute bars must sweep a level,
//! close back through it, form an opposing three-candle fair value gap nearby, and later close
//! through the far side of that gap. The module only emits immutable signals; the shared SLC
//! runtime owns sizing, orders, exits, account risk, and both live and backtest data paths.

use std::collections::VecDeque;

use nautilus_core::UnixNanos;
use nautilus_model::{data::Bar, enums::OrderSide, types::Price};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::{Signal, SignalLevel, TradeDirection, directional_close_location};

/// User-facing parameters for the manipulation signal model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManipulationSettings {
    liquidity_swing_bars: usize,
    liquidity_level_ttl_bars: usize,
    max_liquidity_levels_per_side: usize,
    fvg_search_bars: usize,
    inverse_fvg_ttl_bars: usize,
    minimum_sweep_atr: f64,
    minimum_fvg_atr: f64,
    maximum_fvg_atr: f64,
    maximum_inversion_atr: f64,
}

/// Validated rules consumed by each symbol's signal state.
#[derive(Clone, Copy, Debug)]
pub(super) struct ManipulationRules {
    liquidity_swing_bars: usize,
    liquidity_level_ttl_bars: usize,
    max_liquidity_levels_per_side: usize,
    fvg_search_bars: usize,
    inverse_fvg_ttl_bars: usize,
    minimum_sweep_atr: f64,
    minimum_fvg_atr: f64,
    maximum_fvg_atr: f64,
    maximum_inversion_atr: f64,
}

impl ManipulationRules {
    pub(super) fn from_settings(settings: &ManipulationSettings) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=12).contains(&settings.liquidity_swing_bars),
            "manipulation.liquidity_swing_bars must be between 1 and 12",
        );
        anyhow::ensure!(
            (1..=504).contains(&settings.liquidity_level_ttl_bars),
            "manipulation.liquidity_level_ttl_bars must be between 1 and 504",
        );
        anyhow::ensure!(
            (1..=32).contains(&settings.max_liquidity_levels_per_side),
            "manipulation.max_liquidity_levels_per_side must be between 1 and 32",
        );
        anyhow::ensure!(
            (1..=24).contains(&settings.fvg_search_bars),
            "manipulation.fvg_search_bars must be between 1 and 24",
        );
        anyhow::ensure!(
            (1..=48).contains(&settings.inverse_fvg_ttl_bars),
            "manipulation.inverse_fvg_ttl_bars must be between 1 and 48",
        );
        anyhow::ensure!(
            settings.minimum_sweep_atr.is_finite()
                && (0.0..=4.0).contains(&settings.minimum_sweep_atr),
            "manipulation.minimum_sweep_atr must be finite and between 0 and 4",
        );
        anyhow::ensure!(
            settings.minimum_fvg_atr.is_finite() && (0.0..=4.0).contains(&settings.minimum_fvg_atr),
            "manipulation.minimum_fvg_atr must be finite and between 0 and 4",
        );
        anyhow::ensure!(
            settings.maximum_fvg_atr.is_finite()
                && (settings.minimum_fvg_atr..=4.0).contains(&settings.maximum_fvg_atr),
            "manipulation.maximum_fvg_atr must be finite, at least minimum_fvg_atr, and at most 4",
        );
        anyhow::ensure!(
            settings.maximum_inversion_atr.is_finite()
                && (0.0..=4.0).contains(&settings.maximum_inversion_atr),
            "manipulation.maximum_inversion_atr must be finite and between 0 and 4",
        );
        Ok(Self {
            liquidity_swing_bars: settings.liquidity_swing_bars,
            liquidity_level_ttl_bars: settings.liquidity_level_ttl_bars,
            max_liquidity_levels_per_side: settings.max_liquidity_levels_per_side,
            fvg_search_bars: settings.fvg_search_bars,
            inverse_fvg_ttl_bars: settings.inverse_fvg_ttl_bars,
            minimum_sweep_atr: settings.minimum_sweep_atr,
            minimum_fvg_atr: settings.minimum_fvg_atr,
            maximum_fvg_atr: settings.maximum_fvg_atr,
            maximum_inversion_atr: settings.maximum_inversion_atr,
        })
    }

    pub(super) fn required_liquidity_bars(self) -> usize {
        self.liquidity_swing_bars * 2 + 1
    }
}

/// Per-symbol funnel for auditing each causal setup stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ManipulationFunnel {
    pub liquidity_levels: u64,
    pub liquidity_sweeps: u64,
    pub fair_value_gaps: u64,
    pub inverse_fair_value_gaps: u64,
    pub signals: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiquidityLevel {
    price: Price,
    age_bars: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FairValueGap {
    low: Price,
    high: Price,
    formed_at_setup_age: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ManipulationSetup {
    side: OrderSide,
    liquidity_level_age_bars: usize,
    sweep_atr: f64,
    age_bars: usize,
    gap: Option<FairValueGap>,
    ts_event: UnixNanos,
}

/// Independent higher-timeframe liquidity and five-minute IFVG state for one instrument.
#[derive(Debug)]
pub(super) struct ManipulationState {
    rules: ManipulationRules,
    hourly_bars: VecDeque<Bar>,
    low_liquidity: VecDeque<LiquidityLevel>,
    high_liquidity: VecDeque<LiquidityLevel>,
    recent_five_minute_bars: VecDeque<Bar>,
    long_setup: Option<ManipulationSetup>,
    short_setup: Option<ManipulationSetup>,
    pub funnel: ManipulationFunnel,
}

impl ManipulationState {
    pub(super) fn new(rules: ManipulationRules) -> Self {
        Self {
            rules,
            hourly_bars: VecDeque::with_capacity(rules.required_liquidity_bars()),
            low_liquidity: VecDeque::with_capacity(rules.max_liquidity_levels_per_side),
            high_liquidity: VecDeque::with_capacity(rules.max_liquidity_levels_per_side),
            recent_five_minute_bars: VecDeque::with_capacity(2),
            long_setup: None,
            short_setup: None,
            funnel: ManipulationFunnel::default(),
        }
    }

    /// Confirms an hourly swing only after the configured right-side bars have completed.
    pub(super) fn process_liquidity_bar(&mut self, bar: Bar) {
        age_levels(&mut self.low_liquidity, self.rules.liquidity_level_ttl_bars);
        age_levels(
            &mut self.high_liquidity,
            self.rules.liquidity_level_ttl_bars,
        );
        self.hourly_bars.push_back(bar);
        if self.hourly_bars.len() < self.rules.required_liquidity_bars() {
            return;
        }
        if self.hourly_bars.len() > self.rules.required_liquidity_bars() {
            self.hourly_bars.pop_front();
        }

        let candidate = self.hourly_bars[self.rules.liquidity_swing_bars];
        let is_swing_low = self.hourly_bars.iter().enumerate().all(|(index, other)| {
            index == self.rules.liquidity_swing_bars || candidate.low < other.low
        });
        let is_swing_high = self.hourly_bars.iter().enumerate().all(|(index, other)| {
            index == self.rules.liquidity_swing_bars || candidate.high > other.high
        });
        if is_swing_low {
            push_level(
                &mut self.low_liquidity,
                candidate.low,
                self.rules.max_liquidity_levels_per_side,
            );
            self.funnel.liquidity_levels += 1;
        }
        if is_swing_high {
            push_level(
                &mut self.high_liquidity,
                candidate.high,
                self.rules.max_liquidity_levels_per_side,
            );
            self.funnel.liquidity_levels += 1;
        }
    }

    /// Consumes one completed five-minute bar and emits at most one IFVG entry signal.
    pub(super) fn process(
        &mut self,
        bar: Bar,
        atr: f64,
        recent_range: Option<Decimal>,
        allow_signal: bool,
        trade_direction: TradeDirection,
        has_gap: bool,
    ) -> Option<Signal> {
        if has_gap {
            self.recent_five_minute_bars.clear();
            self.long_setup = None;
            self.short_setup = None;
        }

        let (long_setup, long_signal, long_inversion) = observe_setup(
            self.long_setup.take(),
            bar,
            atr,
            recent_range,
            allow_signal,
            trade_direction,
            self.rules,
        );
        self.long_setup = long_setup;
        let (short_setup, short_signal, short_inversion) = observe_setup(
            self.short_setup.take(),
            bar,
            atr,
            recent_range,
            allow_signal && long_signal.is_none(),
            trade_direction,
            self.rules,
        );
        self.short_setup = short_setup;
        self.funnel.inverse_fair_value_gaps +=
            u64::from(long_inversion) + u64::from(short_inversion);

        let signal = long_signal.or(short_signal);
        if signal.is_none() && atr > 0.0 {
            self.detect_sweeps(bar, atr, trade_direction);
            self.detect_fair_value_gaps(bar, atr);
        }
        self.recent_five_minute_bars.push_back(bar);
        if self.recent_five_minute_bars.len() > 2 {
            self.recent_five_minute_bars.pop_front();
        }
        self.funnel.signals += u64::from(signal.is_some());
        signal
    }

    pub(super) fn opposing_level(&self, side: OrderSide, entry: Price) -> Option<Price> {
        match side {
            OrderSide::Buy => self
                .high_liquidity
                .iter()
                .map(|level| level.price)
                .filter(|price| *price > entry)
                .min(),
            OrderSide::Sell => self
                .low_liquidity
                .iter()
                .map(|level| level.price)
                .filter(|price| *price < entry)
                .max(),
            OrderSide::NoOrderSide => None,
        }
    }

    pub(super) fn funnel_summary(&self) -> String {
        format!(
            "liquidity_levels={}, liquidity_sweeps={}, fair_value_gaps={}, inverse_fair_value_gaps={}, signals={}",
            self.funnel.liquidity_levels,
            self.funnel.liquidity_sweeps,
            self.funnel.fair_value_gaps,
            self.funnel.inverse_fair_value_gaps,
            self.funnel.signals,
        )
    }

    fn detect_sweeps(&mut self, bar: Bar, atr: f64, trade_direction: TradeDirection) {
        if trade_direction.allows(OrderSide::Buy)
            && let Some(level) = take_swept_level(
                &mut self.low_liquidity,
                bar,
                OrderSide::Buy,
                atr,
                self.rules.minimum_sweep_atr,
            )
        {
            self.long_setup = Some(ManipulationSetup {
                side: OrderSide::Buy,
                liquidity_level_age_bars: level.age_bars,
                sweep_atr: atr,
                age_bars: 0,
                gap: None,
                ts_event: bar.ts_event,
            });
            self.funnel.liquidity_sweeps += 1;
        }
        if trade_direction.allows(OrderSide::Sell)
            && let Some(level) = take_swept_level(
                &mut self.high_liquidity,
                bar,
                OrderSide::Sell,
                atr,
                self.rules.minimum_sweep_atr,
            )
        {
            self.short_setup = Some(ManipulationSetup {
                side: OrderSide::Sell,
                liquidity_level_age_bars: level.age_bars,
                sweep_atr: atr,
                age_bars: 0,
                gap: None,
                ts_event: bar.ts_event,
            });
            self.funnel.liquidity_sweeps += 1;
        }
        self.low_liquidity.retain(|level| bar.low >= level.price);
        self.high_liquidity.retain(|level| bar.high <= level.price);
    }

    fn detect_fair_value_gaps(&mut self, bar: Bar, atr: f64) {
        if self.recent_five_minute_bars.len() < 2 {
            return;
        }
        let first = self.recent_five_minute_bars[0];
        if let Some(setup) = self.long_setup.as_mut().filter(|setup| setup.gap.is_none())
            && let Some((low, high)) = fair_value_gap(
                first,
                bar,
                OrderSide::Buy,
                atr,
                self.rules.minimum_fvg_atr,
                self.rules.maximum_fvg_atr,
            )
        {
            setup.gap = Some(FairValueGap {
                low,
                high,
                formed_at_setup_age: setup.age_bars,
            });
            self.funnel.fair_value_gaps += 1;
        }
        if let Some(setup) = self
            .short_setup
            .as_mut()
            .filter(|setup| setup.gap.is_none())
            && let Some((low, high)) = fair_value_gap(
                first,
                bar,
                OrderSide::Sell,
                atr,
                self.rules.minimum_fvg_atr,
                self.rules.maximum_fvg_atr,
            )
        {
            setup.gap = Some(FairValueGap {
                low,
                high,
                formed_at_setup_age: setup.age_bars,
            });
            self.funnel.fair_value_gaps += 1;
        }
    }
}

fn age_levels(levels: &mut VecDeque<LiquidityLevel>, ttl_bars: usize) {
    for level in levels.iter_mut() {
        level.age_bars += 1;
    }
    levels.retain(|level| level.age_bars <= ttl_bars);
}

fn push_level(levels: &mut VecDeque<LiquidityLevel>, price: Price, maximum: usize) {
    levels.retain(|level| level.price != price);
    if levels.len() == maximum {
        levels.pop_front();
    }
    levels.push_back(LiquidityLevel { price, age_bars: 0 });
}

fn take_swept_level(
    levels: &mut VecDeque<LiquidityLevel>,
    bar: Bar,
    side: OrderSide,
    atr: f64,
    minimum_sweep_atr: f64,
) -> Option<LiquidityLevel> {
    // ponytail: one-bar recovery is deterministic; add bounded recovery state if testing supports it
    let selected = levels
        .iter()
        .enumerate()
        .filter(|(_, level)| match side {
            OrderSide::Buy => {
                bar.low < level.price
                    && bar.close > level.price
                    && (level.price.as_f64() - bar.low.as_f64()) / atr >= minimum_sweep_atr
            }
            OrderSide::Sell => {
                bar.high > level.price
                    && bar.close < level.price
                    && (bar.high.as_f64() - level.price.as_f64()) / atr >= minimum_sweep_atr
            }
            OrderSide::NoOrderSide => false,
        })
        .map(|(index, level)| (index, level.price))
        .reduce(|left, right| match side {
            OrderSide::Buy if right.1 > left.1 => right,
            OrderSide::Sell if right.1 < left.1 => right,
            _ => left,
        });
    selected.and_then(|(index, _)| levels.remove(index))
}

fn fair_value_gap(
    first: Bar,
    third: Bar,
    side: OrderSide,
    atr: f64,
    minimum_fvg_atr: f64,
    maximum_fvg_atr: f64,
) -> Option<(Price, Price)> {
    let (low, high) = match side {
        OrderSide::Buy if third.high < first.low => (third.high, first.low),
        OrderSide::Sell if third.low > first.high => (first.high, third.low),
        _ => return None,
    };
    let width_atr = (high.as_f64() - low.as_f64()) / atr;
    (minimum_fvg_atr..=maximum_fvg_atr)
        .contains(&width_atr)
        .then_some((low, high))
}

fn observe_setup(
    setup: Option<ManipulationSetup>,
    bar: Bar,
    atr: f64,
    recent_range: Option<Decimal>,
    allow_signal: bool,
    trade_direction: TradeDirection,
    rules: ManipulationRules,
) -> (Option<ManipulationSetup>, Option<Signal>, bool) {
    let Some(mut setup) = setup else {
        return (None, None, false);
    };
    if bar.ts_event <= setup.ts_event {
        return (Some(setup), None, false);
    }
    setup.age_bars += 1;
    let Some(gap) = setup.gap else {
        return if setup.age_bars > rules.fvg_search_bars {
            (None, None, false)
        } else {
            (Some(setup), None, false)
        };
    };
    if setup.age_bars.saturating_sub(gap.formed_at_setup_age) > rules.inverse_fvg_ttl_bars {
        return (None, None, false);
    }
    let inverted = match setup.side {
        OrderSide::Buy => bar.close > gap.high,
        OrderSide::Sell => bar.close < gap.low,
        OrderSide::NoOrderSide => false,
    };
    if !inverted {
        return (Some(setup), None, false);
    }
    if !allow_signal || !trade_direction.allows(setup.side) || atr <= 0.0 {
        return (None, None, true);
    }

    let risk_atr = setup.sweep_atr.max(atr);
    let distance_atr = match setup.side {
        OrderSide::Buy => (bar.close.as_f64() - gap.high.as_f64()) / risk_atr,
        OrderSide::Sell => (gap.low.as_f64() - bar.close.as_f64()) / risk_atr,
        OrderSide::NoOrderSide => return (None, None, true),
    };
    if distance_atr > rules.maximum_inversion_atr {
        return (None, None, true);
    }
    let gap_width_atr = (gap.high.as_f64() - gap.low.as_f64()) / risk_atr;
    (
        None,
        Some(Signal {
            side: setup.side,
            level: SignalLevel::Fresh,
            entry: bar.close,
            zone_low: gap.low,
            zone_high: gap.high,
            risk_atr,
            recent_range,
            level_age_bars: u64::try_from(setup.liquidity_level_age_bars).unwrap_or(u64::MAX),
            confirmation_bars: u64::try_from(setup.age_bars).unwrap_or(u64::MAX),
            confirmation_close_location: directional_close_location(bar, setup.side),
            distance_atr,
            zone_width_atr: gap_width_atr,
            displacement_strength_atr: gap_width_atr,
            ts_event: bar.ts_event,
        }),
        true,
    )
}

#[cfg(test)]
mod tests {
    use nautilus_model::{data::BarType, types::Quantity};
    use rstest::rstest;

    use super::*;

    fn rules() -> ManipulationRules {
        ManipulationRules {
            liquidity_swing_bars: 2,
            liquidity_level_ttl_bars: 48,
            max_liquidity_levels_per_side: 8,
            fvg_search_bars: 3,
            inverse_fvg_ttl_bars: 3,
            minimum_sweep_atr: 0.0,
            minimum_fvg_atr: 0.0,
            maximum_fvg_atr: 0.4,
            maximum_inversion_atr: 4.0,
        }
    }

    fn bar(open: &str, high: &str, low: &str, close: &str, ts: u64) -> Bar {
        let price = |value: &str| {
            Price::from_decimal_dp(value.parse::<Decimal>().expect("valid test price"), 2)
                .expect("two-decimal test price")
        };
        Bar::new(
            BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            price(open),
            price(high),
            price(low),
            price(close),
            Quantity::from(100),
            UnixNanos::from(ts),
            UnixNanos::from(ts),
        )
    }

    fn process(state: &mut ManipulationState, bar: Bar) -> Option<Signal> {
        state.process(
            bar,
            1.0,
            Some(Decimal::ONE),
            true,
            TradeDirection::Both,
            false,
        )
    }

    #[rstest]
    fn hourly_swing_is_confirmed_only_after_right_side_bars_complete() {
        let mut state = ManipulationState::new(rules());
        let bars = [
            bar("105", "110", "100", "106", 1),
            bar("105", "109", "99", "104", 2),
            bar("100", "108", "95", "103", 3),
            bar("103", "109", "98", "106", 4),
            bar("106", "110", "99", "108", 5),
        ];

        for value in &bars[..4] {
            state.process_liquidity_bar(*value);
        }
        assert!(state.low_liquidity.is_empty());

        state.process_liquidity_bar(bars[4]);
        assert_eq!(
            state.low_liquidity.front().map(|level| level.price),
            Some(Price::from("95"))
        );
    }

    #[rstest]
    fn low_sweep_then_bearish_gap_inversion_emits_buy_signal() {
        let mut state = ManipulationState::new(rules());
        state.low_liquidity.push_back(LiquidityLevel {
            price: Price::from("100"),
            age_bars: 4,
        });

        assert!(process(&mut state, bar("102", "103", "101", "102", 1)).is_none());
        assert!(process(&mut state, bar("102", "102.2", "99.5", "100.5", 2)).is_none());
        assert!(process(&mut state, bar("100.4", "100.8", "99.8", "100", 3)).is_none());
        let signal = process(&mut state, bar("100.2", "101.4", "100", "101.2", 4))
            .expect("expected bullish inverse fair value gap signal");

        assert_eq!(signal.side, OrderSide::Buy);
        assert_eq!(signal.entry, Price::from("101.2"));
        assert_eq!(signal.zone_low, Price::from("100.8"));
        assert_eq!(signal.zone_high, Price::from("101"));
        assert_eq!(state.funnel.liquidity_sweeps, 1);
        assert_eq!(state.funnel.fair_value_gaps, 1);
        assert_eq!(state.funnel.inverse_fair_value_gaps, 1);
    }

    #[rstest]
    fn high_sweep_then_bullish_gap_inversion_emits_sell_signal() {
        let mut state = ManipulationState::new(rules());
        state.high_liquidity.push_back(LiquidityLevel {
            price: Price::from("100"),
            age_bars: 3,
        });

        assert!(process(&mut state, bar("98", "99", "97.5", "98.5", 1)).is_none());
        assert!(process(&mut state, bar("99", "100.5", "98.8", "99.5", 2)).is_none());
        assert!(process(&mut state, bar("99.7", "100", "99.2", "99.8", 3)).is_none());
        let signal = process(&mut state, bar("99.6", "99.7", "98.6", "98.8", 4))
            .expect("expected bearish inverse fair value gap signal");

        assert_eq!(signal.side, OrderSide::Sell);
        assert_eq!(signal.entry, Price::from("98.8"));
        assert_eq!(signal.zone_low, Price::from("99"));
        assert_eq!(signal.zone_high, Price::from("99.2"));
        assert_eq!(state.funnel.liquidity_sweeps, 1);
        assert_eq!(state.funnel.fair_value_gaps, 1);
        assert_eq!(state.funnel.inverse_fair_value_gaps, 1);
    }

    #[rstest]
    fn fair_value_gap_without_prior_liquidity_sweep_is_ignored() {
        let mut state = ManipulationState::new(rules());

        assert!(process(&mut state, bar("102", "103", "101", "102", 1)).is_none());
        assert!(process(&mut state, bar("102", "102.2", "99.5", "100.5", 2)).is_none());
        assert!(process(&mut state, bar("100.4", "100.8", "99.8", "100", 3)).is_none());
        assert!(process(&mut state, bar("100.2", "101.4", "100", "101.2", 4)).is_none());
        assert_eq!(state.funnel.signals, 0);
    }

    #[rstest]
    fn extended_inversion_is_rejected() {
        let mut rules = rules();
        rules.maximum_inversion_atr = 0.1;
        let mut state = ManipulationState::new(rules);
        state.low_liquidity.push_back(LiquidityLevel {
            price: Price::from("100"),
            age_bars: 4,
        });

        assert!(process(&mut state, bar("102", "103", "101", "102", 1)).is_none());
        assert!(process(&mut state, bar("102", "102.2", "99.5", "100.5", 2)).is_none());
        assert!(process(&mut state, bar("100.4", "100.8", "99.8", "100", 3)).is_none());
        assert!(process(&mut state, bar("100.2", "101.4", "100", "101.2", 4)).is_none());
        assert_eq!(state.funnel.inverse_fair_value_gaps, 1);
        assert_eq!(state.funnel.signals, 0);
    }

    #[rstest]
    fn fair_value_gap_width_must_stay_within_configured_range() {
        let first = bar("102", "103", "101", "102", 1);
        let third = bar("100.4", "100.8", "99.8", "100", 3);

        assert!(fair_value_gap(first, third, OrderSide::Buy, 1.0, 0.05, 0.25).is_some());
        assert!(fair_value_gap(first, third, OrderSide::Buy, 1.0, 0.05, 0.15).is_none());
        assert!(fair_value_gap(first, third, OrderSide::Buy, 1.0, 0.25, 0.4).is_none());
    }
}
