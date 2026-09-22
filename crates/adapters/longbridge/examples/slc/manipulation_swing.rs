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

//! Multi-timeframe liquidity-sweep and IFVG swing signal model.
//!
//! Completed daily bars establish directional bias, four-hour bars confirm liquidity pivots,
//! one-hour bars form sweep and opposing FVG setups, and fifteen-minute closes trigger entries.
//! This module emits signals only; the shared runtime owns orders, risk, and exits.

use std::collections::VecDeque;

use nautilus_core::UnixNanos;
use nautilus_model::{data::Bar, enums::OrderSide, types::Price};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::{Signal, SignalLevel, TradeDirection, Trend, directional_close_location};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManipulationSwingSettings {
    daily_bias_bars: usize,
    liquidity_swing_bars: usize,
    liquidity_level_ttl_bars: usize,
    max_liquidity_levels_per_side: usize,
    fvg_search_bars: usize,
    inverse_fvg_ttl_bars: usize,
    minimum_sweep_atr: f64,
    minimum_fvg_atr: f64,
    maximum_fvg_atr: f64,
    maximum_inversion_atr: f64,
    minimum_holding_sessions: u8,
    maximum_holding_sessions: u8,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ManipulationSwingRules {
    daily_bias_bars: usize,
    liquidity_swing_bars: usize,
    liquidity_level_ttl_bars: usize,
    max_liquidity_levels_per_side: usize,
    fvg_search_bars: usize,
    inverse_fvg_ttl_bars: usize,
    minimum_sweep_atr: f64,
    minimum_fvg_atr: f64,
    maximum_fvg_atr: f64,
    maximum_inversion_atr: f64,
    pub minimum_holding_sessions: u8,
    pub maximum_holding_sessions: u8,
}

impl ManipulationSwingRules {
    pub(super) fn from_settings(settings: &ManipulationSwingSettings) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (3..=100).contains(&settings.daily_bias_bars),
            "manipulation_swing.daily_bias_bars must be between 3 and 100",
        );
        anyhow::ensure!(
            (1..=12).contains(&settings.liquidity_swing_bars),
            "manipulation_swing.liquidity_swing_bars must be between 1 and 12",
        );
        anyhow::ensure!(
            (1..=126).contains(&settings.liquidity_level_ttl_bars),
            "manipulation_swing.liquidity_level_ttl_bars must be between 1 and 126",
        );
        anyhow::ensure!(
            (1..=32).contains(&settings.max_liquidity_levels_per_side),
            "manipulation_swing.max_liquidity_levels_per_side must be between 1 and 32",
        );
        anyhow::ensure!(
            (1..=12).contains(&settings.fvg_search_bars),
            "manipulation_swing.fvg_search_bars must be between 1 and 12",
        );
        anyhow::ensure!(
            (1..=52).contains(&settings.inverse_fvg_ttl_bars),
            "manipulation_swing.inverse_fvg_ttl_bars must be between 1 and 52",
        );
        anyhow::ensure!(
            settings.minimum_sweep_atr.is_finite()
                && (0.0..=4.0).contains(&settings.minimum_sweep_atr),
            "manipulation_swing.minimum_sweep_atr must be finite and between 0 and 4",
        );
        anyhow::ensure!(
            settings.minimum_fvg_atr.is_finite() && (0.0..=4.0).contains(&settings.minimum_fvg_atr),
            "manipulation_swing.minimum_fvg_atr must be finite and between 0 and 4",
        );
        anyhow::ensure!(
            settings.maximum_fvg_atr.is_finite()
                && (settings.minimum_fvg_atr..=4.0).contains(&settings.maximum_fvg_atr),
            "manipulation_swing.maximum_fvg_atr must be finite, at least minimum_fvg_atr, and at most 4",
        );
        anyhow::ensure!(
            settings.maximum_inversion_atr.is_finite()
                && (0.0..=4.0).contains(&settings.maximum_inversion_atr),
            "manipulation_swing.maximum_inversion_atr must be finite and between 0 and 4",
        );
        anyhow::ensure!(
            settings.minimum_holding_sessions >= 1
                && settings.minimum_holding_sessions <= settings.maximum_holding_sessions
                && settings.maximum_holding_sessions <= 10,
            "manipulation_swing holding sessions must satisfy 1 <= minimum <= maximum <= 10",
        );
        Ok(Self {
            daily_bias_bars: settings.daily_bias_bars,
            liquidity_swing_bars: settings.liquidity_swing_bars,
            liquidity_level_ttl_bars: settings.liquidity_level_ttl_bars,
            max_liquidity_levels_per_side: settings.max_liquidity_levels_per_side,
            fvg_search_bars: settings.fvg_search_bars,
            inverse_fvg_ttl_bars: settings.inverse_fvg_ttl_bars,
            minimum_sweep_atr: settings.minimum_sweep_atr,
            minimum_fvg_atr: settings.minimum_fvg_atr,
            maximum_fvg_atr: settings.maximum_fvg_atr,
            maximum_inversion_atr: settings.maximum_inversion_atr,
            minimum_holding_sessions: settings.minimum_holding_sessions,
            maximum_holding_sessions: settings.maximum_holding_sessions,
        })
    }

    pub(super) fn required_daily_bars(self) -> usize {
        self.daily_bias_bars
    }

    pub(super) fn required_liquidity_bars(self) -> usize {
        self.liquidity_swing_bars * 2 + 1
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ManipulationSwingFunnel {
    pub entry_bars: u64,
    pub daily_directional_bars: u64,
    pub liquidity_levels: u64,
    pub liquidity_sweeps: u64,
    pub fair_value_gaps: u64,
    pub inverse_fair_value_gaps: u64,
    pub inversion_context_rejections: u64,
    pub inversion_distance_rejections: u64,
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
    available_at: UnixNanos,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SwingSetup {
    side: OrderSide,
    liquidity_level_age_bars: usize,
    sweep_atr: f64,
    hourly_age_bars: usize,
    confirmation_age_bars: usize,
    gap: Option<FairValueGap>,
}

#[derive(Debug)]
pub(super) struct ManipulationSwingState {
    rules: ManipulationSwingRules,
    daily_closes: VecDeque<Price>,
    four_hour_bars: VecDeque<Bar>,
    low_liquidity: VecDeque<LiquidityLevel>,
    high_liquidity: VecDeque<LiquidityLevel>,
    recent_hourly_bars: VecDeque<Bar>,
    recent_entry_bars: VecDeque<Bar>,
    long_setup: Option<SwingSetup>,
    short_setup: Option<SwingSetup>,
    pub funnel: ManipulationSwingFunnel,
}

impl ManipulationSwingState {
    pub(super) fn new(rules: ManipulationSwingRules) -> Self {
        Self {
            rules,
            daily_closes: VecDeque::with_capacity(rules.daily_bias_bars),
            four_hour_bars: VecDeque::with_capacity(rules.required_liquidity_bars()),
            low_liquidity: VecDeque::with_capacity(rules.max_liquidity_levels_per_side),
            high_liquidity: VecDeque::with_capacity(rules.max_liquidity_levels_per_side),
            recent_hourly_bars: VecDeque::with_capacity(2),
            recent_entry_bars: VecDeque::with_capacity(8),
            long_setup: None,
            short_setup: None,
            funnel: ManipulationSwingFunnel::default(),
        }
    }

    pub(super) fn process_daily_bar(&mut self, bar: Bar) {
        self.daily_closes.push_back(bar.close);
        if self.daily_closes.len() > self.rules.daily_bias_bars {
            self.daily_closes.pop_front();
        }
        self.funnel.daily_directional_bars += u64::from(self.daily_bias() != Trend::Neutral);
    }

    pub(super) fn daily_bias(&self) -> Trend {
        if self.daily_closes.len() < self.rules.daily_bias_bars {
            return Trend::Neutral;
        }
        let first = self
            .daily_closes
            .front()
            .expect("daily closes are not empty");
        let last = self
            .daily_closes
            .back()
            .expect("daily closes are not empty");
        let count = u32::try_from(self.daily_closes.len()).expect("daily bias is bounded at 100");
        let mean = self
            .daily_closes
            .iter()
            .map(|price| price.as_f64())
            .sum::<f64>()
            / f64::from(count);
        if last > first && last.as_f64() > mean {
            Trend::Up
        } else if last < first && last.as_f64() < mean {
            Trend::Down
        } else {
            Trend::Neutral
        }
    }

    pub(super) fn process_liquidity_bar(&mut self, bar: Bar) {
        age_levels(&mut self.low_liquidity, self.rules.liquidity_level_ttl_bars);
        age_levels(
            &mut self.high_liquidity,
            self.rules.liquidity_level_ttl_bars,
        );
        self.four_hour_bars.push_back(bar);
        if self.four_hour_bars.len() < self.rules.required_liquidity_bars() {
            return;
        }
        if self.four_hour_bars.len() > self.rules.required_liquidity_bars() {
            self.four_hour_bars.pop_front();
        }

        let candidate = self.four_hour_bars[self.rules.liquidity_swing_bars];
        let is_swing_low = self
            .four_hour_bars
            .iter()
            .enumerate()
            .all(|(index, other)| {
                index == self.rules.liquidity_swing_bars || candidate.low < other.low
            });
        let is_swing_high = self
            .four_hour_bars
            .iter()
            .enumerate()
            .all(|(index, other)| {
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

    pub(super) fn process_setup_bar(
        &mut self,
        bar: Bar,
        atr: f64,
        trade_direction: TradeDirection,
    ) {
        if self.recent_hourly_bars.back().is_some_and(|previous| {
            bar.ts_event
                .as_u64()
                .saturating_sub(previous.ts_event.as_u64())
                != 60 * 60 * 1_000_000_000
        }) {
            self.recent_hourly_bars.clear();
        }
        age_setup(&mut self.long_setup, self.rules.fvg_search_bars);
        age_setup(&mut self.short_setup, self.rules.fvg_search_bars);
        if atr > 0.0 {
            self.detect_sweeps(bar, atr, trade_direction);
            self.detect_fair_value_gaps(bar, atr);
        }
        self.recent_hourly_bars.push_back(bar);
        if self.recent_hourly_bars.len() > 2 {
            self.recent_hourly_bars.pop_front();
        }
    }

    pub(super) fn process_entry_bar(
        &mut self,
        bar: Bar,
        recent_range_lookback_bars: usize,
        allow_signal: bool,
        trade_direction: TradeDirection,
    ) -> Option<Signal> {
        self.funnel.entry_bars += 1;
        if self.recent_entry_bars.back().is_some_and(|previous| {
            bar.ts_event
                .as_u64()
                .saturating_sub(previous.ts_event.as_u64())
                != 15 * 60 * 1_000_000_000
        }) {
            self.recent_entry_bars.clear();
        }
        let recent_range =
            recent_bar_range(&self.recent_entry_bars, bar, recent_range_lookback_bars);
        let bias = self.daily_bias();
        let long_signal = observe_inversion(
            &mut self.long_setup,
            bar,
            recent_range,
            allow_signal && bias == Trend::Up,
            trade_direction,
            self.rules,
            &mut self.funnel,
        );
        let short_signal = observe_inversion(
            &mut self.short_setup,
            bar,
            recent_range,
            allow_signal && long_signal.is_none() && bias == Trend::Down,
            trade_direction,
            self.rules,
            &mut self.funnel,
        );
        self.recent_entry_bars.push_back(bar);
        while self.recent_entry_bars.len() > recent_range_lookback_bars.max(1) {
            self.recent_entry_bars.pop_front();
        }
        let signal = long_signal.or(short_signal);
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
            "entry_bars={}, daily_directional_bars={}, liquidity_levels={}, liquidity_sweeps={}, fair_value_gaps={}, inverse_fair_value_gaps={}, inversion_context_rejections={}, inversion_distance_rejections={}, signals={}",
            self.funnel.entry_bars,
            self.funnel.daily_directional_bars,
            self.funnel.liquidity_levels,
            self.funnel.liquidity_sweeps,
            self.funnel.fair_value_gaps,
            self.funnel.inverse_fair_value_gaps,
            self.funnel.inversion_context_rejections,
            self.funnel.inversion_distance_rejections,
            self.funnel.signals,
        )
    }

    fn detect_sweeps(&mut self, bar: Bar, atr: f64, trade_direction: TradeDirection) {
        let bias = self.daily_bias();
        if bias == Trend::Up
            && trade_direction.allows(OrderSide::Buy)
            && let Some(level) = take_swept_level(
                &mut self.low_liquidity,
                bar,
                OrderSide::Buy,
                atr,
                self.rules.minimum_sweep_atr,
            )
        {
            self.long_setup = Some(SwingSetup {
                side: OrderSide::Buy,
                liquidity_level_age_bars: level.age_bars,
                sweep_atr: atr,
                hourly_age_bars: 0,
                confirmation_age_bars: 0,
                gap: None,
            });
            self.funnel.liquidity_sweeps += 1;
        }
        if bias == Trend::Down
            && trade_direction.allows(OrderSide::Sell)
            && let Some(level) = take_swept_level(
                &mut self.high_liquidity,
                bar,
                OrderSide::Sell,
                atr,
                self.rules.minimum_sweep_atr,
            )
        {
            self.short_setup = Some(SwingSetup {
                side: OrderSide::Sell,
                liquidity_level_age_bars: level.age_bars,
                sweep_atr: atr,
                hourly_age_bars: 0,
                confirmation_age_bars: 0,
                gap: None,
            });
            self.funnel.liquidity_sweeps += 1;
        }
        self.low_liquidity.retain(|level| bar.low >= level.price);
        self.high_liquidity.retain(|level| bar.high <= level.price);
    }

    fn detect_fair_value_gaps(&mut self, bar: Bar, atr: f64) {
        if self.recent_hourly_bars.len() < 2 {
            return;
        }
        let first = self.recent_hourly_bars[0];
        for setup in [&mut self.long_setup, &mut self.short_setup] {
            let Some(setup) = setup.as_mut().filter(|setup| setup.gap.is_none()) else {
                continue;
            };
            if let Some((low, high)) = fair_value_gap(
                first,
                bar,
                setup.side,
                atr,
                self.rules.minimum_fvg_atr,
                self.rules.maximum_fvg_atr,
            ) {
                let Some(available_at) = bar.ts_event.checked_add(60_u64 * 60 * 1_000_000_000)
                else {
                    continue;
                };
                setup.gap = Some(FairValueGap {
                    low,
                    high,
                    available_at,
                });
                setup.confirmation_age_bars = 0;
                self.funnel.fair_value_gaps += 1;
            }
        }
    }
}

fn age_setup(setup: &mut Option<SwingSetup>, ttl_bars: usize) {
    let Some(active) = setup.as_mut() else {
        return;
    };
    if active.gap.is_none() {
        active.hourly_age_bars += 1;
        if active.hourly_age_bars > ttl_bars {
            *setup = None;
        }
    }
}

fn observe_inversion(
    setup: &mut Option<SwingSetup>,
    bar: Bar,
    recent_range: Option<Decimal>,
    allow_signal: bool,
    trade_direction: TradeDirection,
    rules: ManipulationSwingRules,
    funnel: &mut ManipulationSwingFunnel,
) -> Option<Signal> {
    let active = setup.as_mut()?;
    let gap = active.gap?;
    if bar.ts_event < gap.available_at {
        return None;
    }
    active.confirmation_age_bars += 1;
    if active.confirmation_age_bars > rules.inverse_fvg_ttl_bars {
        *setup = None;
        return None;
    }
    let inverted = match active.side {
        OrderSide::Buy => bar.close > gap.high,
        OrderSide::Sell => bar.close < gap.low,
        OrderSide::NoOrderSide => false,
    };
    if !inverted {
        return None;
    }
    funnel.inverse_fair_value_gaps += 1;
    let active = setup.take().expect("active setup exists");
    if !allow_signal || !trade_direction.allows(active.side) || active.sweep_atr <= 0.0 {
        funnel.inversion_context_rejections += 1;
        return None;
    }
    let distance_atr = match active.side {
        OrderSide::Buy => (bar.close.as_f64() - gap.high.as_f64()) / active.sweep_atr,
        OrderSide::Sell => (gap.low.as_f64() - bar.close.as_f64()) / active.sweep_atr,
        OrderSide::NoOrderSide => return None,
    };
    if distance_atr > rules.maximum_inversion_atr {
        funnel.inversion_distance_rejections += 1;
        return None;
    }
    let gap_width_atr = (gap.high.as_f64() - gap.low.as_f64()) / active.sweep_atr;
    Some(Signal {
        side: active.side,
        level: SignalLevel::Fresh,
        entry: bar.close,
        zone_low: gap.low,
        zone_high: gap.high,
        risk_atr: active.sweep_atr,
        recent_range,
        level_age_bars: u64::try_from(active.liquidity_level_age_bars).unwrap_or(u64::MAX),
        confirmation_bars: u64::try_from(active.confirmation_age_bars).unwrap_or(u64::MAX),
        confirmation_close_location: directional_close_location(bar, active.side),
        distance_atr,
        zone_width_atr: gap_width_atr,
        displacement_strength_atr: gap_width_atr,
        ts_event: bar.ts_event,
    })
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

fn recent_bar_range(bars: &VecDeque<Bar>, current: Bar, lookback: usize) -> Option<Decimal> {
    if lookback == 0 || bars.len() + 1 < lookback {
        return None;
    }
    let (high, low) = bars
        .iter()
        .rev()
        .take(lookback - 1)
        .fold((current.high, current.low), |(high, low), bar| {
            (high.max(bar.high), low.min(bar.low))
        });
    let range = high.as_decimal() - low.as_decimal();
    (range > Decimal::ZERO).then_some(range)
}

#[cfg(test)]
mod tests {
    use nautilus_model::{data::BarType, types::Quantity};
    use rstest::rstest;

    use super::*;

    fn rules() -> ManipulationSwingRules {
        ManipulationSwingRules {
            daily_bias_bars: 3,
            liquidity_swing_bars: 1,
            liquidity_level_ttl_bars: 12,
            max_liquidity_levels_per_side: 4,
            fvg_search_bars: 3,
            inverse_fvg_ttl_bars: 8,
            minimum_sweep_atr: 0.1,
            minimum_fvg_atr: 0.1,
            maximum_fvg_atr: 1.0,
            maximum_inversion_atr: 0.5,
            minimum_holding_sessions: 2,
            maximum_holding_sessions: 5,
        }
    }

    fn bar(period: &str, ts: u64, open: f64, high: f64, low: f64, close: f64) -> Bar {
        Bar::new(
            BarType::from(format!("QQQ.US.LONGBRIDGE-{period}-LAST-EXTERNAL").as_str()),
            Price::new(open, 2),
            Price::new(high, 2),
            Price::new(low, 2),
            Price::new(close, 2),
            Quantity::from(100),
            UnixNanos::from(ts),
            UnixNanos::from(ts),
        )
    }

    #[rstest]
    fn daily_bias_requires_price_and_average_alignment() {
        let mut state = ManipulationSwingState::new(rules());
        state.process_daily_bar(bar("1-DAY", 1, 99.0, 101.0, 98.0, 100.0));
        state.process_daily_bar(bar("1-DAY", 2, 100.0, 102.0, 99.0, 101.0));
        assert_eq!(state.daily_bias(), Trend::Neutral);

        state.process_daily_bar(bar("1-DAY", 3, 101.0, 106.0, 100.0, 105.0));

        assert_eq!(state.daily_bias(), Trend::Up);
    }

    #[rstest]
    fn daily_and_four_hour_context_authorize_one_hour_setup_and_fifteen_minute_entry() {
        let hour = 60 * 60 * 1_000_000_000;
        let fifteen_minutes = 15_u64 * 60 * 1_000_000_000;
        let mut state = ManipulationSwingState::new(rules());
        for (ts, close) in [(1, 100.0), (2, 101.0), (3, 105.0)] {
            state.process_daily_bar(bar("1-DAY", ts, close, close + 1.0, close - 1.0, close));
        }
        state.process_liquidity_bar(bar("4-HOUR", 10, 104.0, 108.0, 100.0, 105.0));
        state.process_liquidity_bar(bar("4-HOUR", 11, 100.0, 106.0, 95.0, 101.0));
        state.process_liquidity_bar(bar("4-HOUR", 12, 102.0, 109.0, 99.0, 107.0));

        state.process_setup_bar(
            bar("1-HOUR", 20 * hour, 97.0, 98.0, 94.0, 96.0),
            2.0,
            TradeDirection::Both,
        );
        state.process_setup_bar(
            bar("1-HOUR", 21 * hour, 96.0, 97.0, 94.5, 95.0),
            2.0,
            TradeDirection::Both,
        );
        state.process_setup_bar(
            bar("1-HOUR", 22 * hour, 93.0, 93.5, 91.0, 92.0),
            2.0,
            TradeDirection::Both,
        );

        assert!(
            state
                .process_entry_bar(
                    bar(
                        "15-MINUTE",
                        22 * hour + fifteen_minutes,
                        93.0,
                        95.0,
                        92.5,
                        94.5,
                    ),
                    1,
                    true,
                    TradeDirection::Both,
                )
                .is_none(),
            "the final 15-minute bar inside an unfinished 1-hour setup must not confirm it",
        );

        let signal = state.process_entry_bar(
            bar("15-MINUTE", 23 * hour, 93.0, 95.0, 92.5, 94.5),
            1,
            true,
            TradeDirection::Both,
        );

        assert_eq!(signal.map(|signal| signal.side), Some(OrderSide::Buy));
        assert_eq!(state.funnel.inverse_fair_value_gaps, 1);
        assert_eq!(state.funnel.signals, 1);
        assert_eq!(state.funnel.signals, 1);
    }
}
