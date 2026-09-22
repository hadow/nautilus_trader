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
//  See the License for the specific language governing permissions and limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Event-driven momentum pullback strategy.

use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use ahash::AHashSet;
use jiff::tz::TimeZone;
use nautilus_common::actor::DataActor;
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_indicators::{
    average::{MovingAverageType, sma::SimpleMovingAverage},
    indicator::{Indicator, MovingAverage},
    volatility::atr::AverageTrueRange,
};
use nautilus_model::{
    accounts::Account,
    data::{Bar, BarType, QuoteTick},
    enums::{AggregationSource, BarAggregation, OrderSide, PositionSide, TimeInForce, TriggerType},
    events::{
        OrderCanceled, OrderDenied, OrderExpired, OrderFilled, OrderRejected, PositionClosed,
    },
    identifiers::{ClientOrderId, InstrumentId, PositionId, StrategyId},
    instruments::{Instrument, InstrumentAny},
    orders::Order,
    types::{Money, Price, Quantity},
};
use rust_decimal::{
    Decimal,
    prelude::{FromPrimitive, ToPrimitive},
};
use ustr::Ustr;

use super::{
    EntryOrderType, EntrySignal, MarketRegime, MomentumPullbackConfig, MomentumPullbackReport,
    MomentumSnapshot, PullbackSnapshot, PullbackType, RiskSnapshot, SetupState,
    SharedMomentumPullbackReport, TradeRecord,
    model::{
        EntryFactors, ExitDecision, ExitFactors, ExitReason, MomentumFactors, PullbackFactors,
        atr_stop_price, available_position_slots, build_risk_snapshot, classify_market_regime,
        classify_pullback, entry_gate_status, evaluate_exit, momentum_score, percent_return,
        pullback_quality_score, relative_strength, trailing_stop_price, trend_aligned,
    },
};
use crate::{
    nautilus_strategy,
    strategy::{Strategy, StrategyCore},
};

const EVENT_SCAN: &str = "SCAN";
const EVENT_QUALIFY: &str = "QUALIFY";
const EVENT_PULLBACK: &str = "PULLBACK_DETECTED";
const EVENT_READY: &str = "SETUP_READY";
const EVENT_TRIGGER: &str = "ENTRY_TRIGGER";
const EVENT_SUBMITTED: &str = "ORDER_SUBMITTED";
const EVENT_FILLED: &str = "ORDER_FILLED";
const EVENT_STOP: &str = "STOP_UPDATED";
const EVENT_PARTIAL: &str = "PARTIAL_EXIT";
const EVENT_EXIT: &str = "EXIT";
const EVENT_INVALIDATED: &str = "INVALIDATED";
const EVENT_ENTRY_REJECTED: &str = "ENTRY_REJECTED";
const COUNT_READY_EVALUATED: &str = "READY_EVALUATED";
const COUNT_READY_REGIME_BLOCKED: &str = "READY_REGIME_BLOCKED";
const COUNT_READY_VOLUME_FAILED: &str = "READY_VOLUME_FAILED";
const COUNT_READY_BREAKOUT_FAILED: &str = "READY_BREAKOUT_FAILED";
const COUNT_READY_EXTENSION_FAILED: &str = "READY_EXTENSION_FAILED";
const COUNT_READY_TREND_FAILED: &str = "READY_TREND_FAILED";
const COUNT_READY_SOLE_VOLUME_FAILED: &str = "READY_SOLE_VOLUME_FAILED";
const COUNT_READY_SOLE_BREAKOUT_FAILED: &str = "READY_SOLE_BREAKOUT_FAILED";
const COUNT_READY_SOLE_EXTENSION_FAILED: &str = "READY_SOLE_EXTENSION_FAILED";
const COUNT_READY_SOLE_TREND_FAILED: &str = "READY_SOLE_TREND_FAILED";
const COUNT_NEXT_SESSION_EVALUATED: &str = "NEXT_SESSION_EVALUATED";
const COUNT_NEXT_SESSION_SUPPORT_FAILED: &str = "NEXT_SESSION_SUPPORT_FAILED";
const COUNT_NEXT_SESSION_EXTENSION_FAILED: &str = "NEXT_SESSION_EXTENSION_FAILED";

#[derive(Clone, Debug)]
struct PendingEntry {
    signal_timestamp: UnixNanos,
    momentum: MomentumSnapshot,
    pullback: PullbackSnapshot,
    market_regime: MarketRegime,
    sma_fast: f64,
    atr: f64,
}

#[derive(Clone, Debug)]
struct SubmittedEntry {
    signal: EntrySignal,
    atr: Decimal,
    pullback_low: Price,
}

#[derive(Clone, Copy, Debug)]
struct PendingExit {
    decision: ExitDecision,
    signal_timestamp: UnixNanos,
    execution_started: bool,
    exit_order_id: Option<ClientOrderId>,
}

#[derive(Debug)]
struct ActiveTrade {
    entry_timestamp: UnixNanos,
    average_entry: Decimal,
    quantity: Decimal,
    initial_stop: Price,
    current_stop: Price,
    initial_risk_per_share: Decimal,
    highest_high: f64,
    bars_held: usize,
    partial_taken: bool,
    entry_regime: MarketRegime,
    protective_orders: HashMap<ClientOrderId, Quantity>,
}

#[derive(Debug)]
struct SymbolState {
    bars: VecDeque<Bar>,
    working_bar: Option<Bar>,
    sma_fast: SimpleMovingAverage,
    sma_medium: SimpleMovingAverage,
    sma_slow: SimpleMovingAverage,
    regime_sma_medium: SimpleMovingAverage,
    regime_sma_slow: SimpleMovingAverage,
    atr: AverageTrueRange,
    setup_state: SetupState,
    momentum: Option<MomentumSnapshot>,
    pullback: Option<PullbackSnapshot>,
    pending_entry: Option<PendingEntry>,
    submitted_entry: Option<SubmittedEntry>,
    entry_order_id: Option<ClientOrderId>,
    active_trade: Option<ActiveTrade>,
    pending_exit: Option<PendingExit>,
    awaiting_protective_cancels: AHashSet<ClientOrderId>,
    last_evaluated: Option<UnixNanos>,
    last_final_bar_ts: Option<UnixNanos>,
    history_capacity: usize,
}

impl SymbolState {
    fn new(config: &MomentumPullbackConfig) -> Self {
        let history_capacity = config
            .sma_slow
            .max(config.regime_sma_slow)
            .max(config.momentum_lookback_medium)
            .max(config.rs_lookback_medium)
            .max(config.pullback_lookback)
            + 2;
        Self {
            bars: VecDeque::with_capacity(history_capacity),
            working_bar: None,
            sma_fast: SimpleMovingAverage::new(config.sma_fast, None),
            sma_medium: SimpleMovingAverage::new(config.sma_medium, None),
            sma_slow: SimpleMovingAverage::new(config.sma_slow, None),
            regime_sma_medium: SimpleMovingAverage::new(config.regime_sma_medium, None),
            regime_sma_slow: SimpleMovingAverage::new(config.regime_sma_slow, None),
            atr: AverageTrueRange::new(
                config.atr_period,
                Some(MovingAverageType::Wilder),
                Some(true),
                None,
            ),
            setup_state: SetupState::Watchlist,
            momentum: None,
            pullback: None,
            pending_entry: None,
            submitted_entry: None,
            entry_order_id: None,
            active_trade: None,
            pending_exit: None,
            awaiting_protective_cancels: AHashSet::new(),
            last_evaluated: None,
            last_final_bar_ts: None,
            history_capacity,
        }
    }

    fn push_final_bar(&mut self, bar: Bar) {
        self.sma_fast.handle_bar(&bar);
        self.sma_medium.handle_bar(&bar);
        self.sma_slow.handle_bar(&bar);
        self.regime_sma_medium.handle_bar(&bar);
        self.regime_sma_slow.handle_bar(&bar);
        self.atr.handle_bar(&bar);
        if self.bars.len() == self.history_capacity {
            self.bars.pop_front();
        }
        self.bars.push_back(bar);
    }

    fn return_over(&self, lookback: usize) -> Option<f64> {
        let current = self.bars.back()?.close.as_f64();
        let previous = self
            .bars
            .get(self.bars.len().checked_sub(lookback + 1)?)?
            .close
            .as_f64();
        percent_return(current, previous)
    }
}

/// Venue-neutral long-only momentum pullback strategy.
pub struct MomentumPullbackStrategy {
    pub(super) core: StrategyCore,
    pub(super) config: MomentumPullbackConfig,
    states: HashMap<InstrumentId, SymbolState>,
    timezone: TimeZone,
    report: SharedMomentumPullbackReport,
}

impl MomentumPullbackStrategy {
    /// Creates a strategy after validating its configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration is invalid.
    pub fn new(config: MomentumPullbackConfig) -> anyhow::Result<Self> {
        Self::with_report(
            config,
            Arc::new(Mutex::new(MomentumPullbackReport::default())),
        )
    }

    /// Creates a strategy and writes signals and completed trades to `report`.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration is invalid.
    pub fn with_report(
        config: MomentumPullbackConfig,
        report: SharedMomentumPullbackReport,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            config.bar_specification.aggregation == BarAggregation::Day,
            "momentum pullback V1 requires daily bars",
        );
        let timezone = get_timezone(&config.timezone)?;
        let mut ids = config.universe.clone();
        ids.extend([
            config.market_regime_instrument_id,
            config.secondary_market_instrument_id,
            config.relative_strength_instrument_id,
        ]);
        ids.sort_unstable();
        ids.dedup();
        let states = ids
            .into_iter()
            .map(|id| (id, SymbolState::new(&config)))
            .collect();

        Ok(Self {
            core: StrategyCore::new(config.base.clone()),
            config,
            states,
            timezone,
            report,
        })
    }

    #[must_use]
    pub fn report_handle(&self) -> SharedMomentumPullbackReport {
        Arc::clone(&self.report)
    }

    fn bar_type(&self, instrument_id: InstrumentId) -> BarType {
        BarType::new(
            instrument_id,
            self.config.bar_specification,
            AggregationSource::External,
        )
    }

    fn event(
        &self,
        event: &str,
        instrument_id: InstrumentId,
        ts: UnixNanos,
        reason: &str,
        metrics: &str,
    ) {
        let strategy_id = self
            .strategy_id()
            .unwrap_or_else(|| StrategyId::from("MOMENTUM-PULLBACK-UNREGISTERED"));
        log::info!(
            "event={event} timestamp={ts} symbol={instrument_id} strategy_id={strategy_id} reason={reason:?} {metrics}",
        );
        self.increment_count(event);
    }

    fn increment_count(&self, name: &str) {
        if let Ok(mut report) = self.report.lock() {
            *report.event_counts.entry(name.to_string()).or_default() += 1;
        }
    }

    fn set_state(&mut self, instrument_id: InstrumentId, next: SetupState) {
        let Some(state) = self.states.get_mut(&instrument_id) else {
            return;
        };
        if state.setup_state == next {
            return;
        }
        debug_assert!(state.setup_state.can_transition_to(next));
        state.setup_state = next;
    }

    fn accept_bar(&mut self, bar: Bar) -> Option<Bar> {
        let state = self.states.get_mut(&bar.bar_type.instrument_id())?;
        if self.config.bars_are_final {
            return Some(bar);
        }
        match state.working_bar {
            None => {
                state.working_bar = Some(bar);
                None
            }
            Some(current) if current.ts_event == bar.ts_event => {
                state.working_bar = Some(bar);
                None
            }
            Some(current) if current.ts_event < bar.ts_event => {
                state.working_bar = Some(bar);
                Some(current)
            }
            Some(_) => {
                log::warn!(
                    "Discarding out-of-order daily bar for {} at {}",
                    bar.bar_type.instrument_id(),
                    bar.ts_event,
                );
                None
            }
        }
    }

    fn process_final_bar(&mut self, bar: Bar) -> anyhow::Result<()> {
        let instrument_id = bar.bar_type.instrument_id();
        if let Some(state) = self.states.get_mut(&instrument_id) {
            if state
                .last_final_bar_ts
                .is_some_and(|last| bar.ts_event <= last)
            {
                return Ok(());
            }
            if state
                .pending_entry
                .as_ref()
                .is_some_and(|pending| pending.signal_timestamp < bar.ts_event)
            {
                state.pending_entry = None;
                state.setup_state = SetupState::Invalidated;
            }
            state.push_final_bar(bar);
            state.last_final_bar_ts = Some(bar.ts_event);
        }

        if self.config.universe.contains(&instrument_id) {
            self.manage_position(instrument_id)?;
            self.evaluate_symbol(instrument_id)?;
        } else if instrument_id == self.config.market_regime_instrument_id
            || instrument_id == self.config.relative_strength_instrument_id
        {
            for id in self.config.universe.clone() {
                self.evaluate_symbol(id)?;
            }
        }
        Ok(())
    }

    fn market_regime(&self, ts: UnixNanos) -> Option<MarketRegime> {
        let market = self.states.get(&self.config.market_regime_instrument_id)?;
        (market.bars.back()?.ts_event == ts
            && market.regime_sma_medium.initialized()
            && market.regime_sma_slow.initialized())
        .then(|| {
            classify_market_regime(
                market.bars.back().expect("checked above").close.as_f64(),
                market.regime_sma_medium.value(),
                market.regime_sma_slow.value(),
            )
        })
    }

    fn momentum_snapshot(
        &self,
        instrument_id: InstrumentId,
        ts: UnixNanos,
    ) -> Option<(MomentumSnapshot, MarketRegime)> {
        let state = self.states.get(&instrument_id)?;
        let market = self
            .states
            .get(&self.config.relative_strength_instrument_id)?;
        if state.bars.back()?.ts_event != ts
            || market.bars.back()?.ts_event != ts
            || !state.sma_slow.initialized()
            || !state.atr.initialized()
        {
            return None;
        }

        let return_short = state.return_over(self.config.momentum_lookback_short)?;
        let return_medium = state.return_over(self.config.momentum_lookback_medium)?;
        let market_short = market.return_over(self.config.rs_lookback_short)?;
        let market_medium = market.return_over(self.config.rs_lookback_medium)?;
        let rs_short = relative_strength(return_short, market_short);
        let rs_medium = relative_strength(return_medium, market_medium);
        let close = state.bars.back()?.close.as_f64();
        let trend = trend_aligned(
            close,
            state.sma_fast.value(),
            state.sma_medium.value(),
            state.sma_slow.value(),
        );
        let volume_window = self.config.entry_volume_lookback.min(state.bars.len());
        let window_start = state.bars.len() - volume_window;
        let window: Vec<&Bar> = state.bars.iter().skip(window_start).collect();
        let average_dollar_volume = window
            .iter()
            .map(|bar| bar.close.as_decimal() * bar.volume.as_decimal())
            .sum::<Decimal>()
            / Decimal::from(u64::try_from(volume_window).ok()?);
        let average_volume =
            window.iter().map(|bar| bar.volume.as_f64()).sum::<f64>() / volume_window as f64;
        let up_volume = window
            .windows(2)
            .filter(|bars| bars[1].close > bars[0].close)
            .map(|bars| bars[1].volume.as_f64())
            .sum::<f64>();
        let up_days = window
            .windows(2)
            .filter(|bars| bars[1].close > bars[0].close)
            .count();
        let volume_strength = if up_days == 0 || average_volume <= 0.0 {
            0.0
        } else {
            (up_volume
                / up_days as f64
                / average_volume
                / self.config.entry_volume_multiplier.max(f64::EPSILON))
            .clamp(0.0, 1.0)
        };
        let normalize = |value: f64, minimum: f64| {
            if minimum <= 0.0 {
                f64::from(value > 0.0)
            } else {
                (value / minimum).clamp(0.0, 1.0)
            }
        };
        let atr_pct = state.atr.value / close;
        let score = momentum_score(
            MomentumFactors {
                relative_strength: (normalize(rs_short, self.config.min_rs_short)
                    + normalize(rs_medium, self.config.min_rs_medium))
                    / 2.0,
                momentum_short: normalize(return_short, self.config.min_return_short),
                momentum_medium: normalize(return_medium, self.config.min_return_medium),
                trend: f64::from(trend),
                volume: volume_strength,
                volatility: (1.0 - atr_pct / self.config.maximum_atr_pct).clamp(0.0, 1.0),
            },
            self.config.score_weights,
        );
        Some((
            MomentumSnapshot {
                timestamp: ts,
                return_short,
                return_medium,
                relative_strength_short: rs_short,
                relative_strength_medium: rs_medium,
                sma_fast: state.sma_fast.value(),
                sma_medium: state.sma_medium.value(),
                sma_slow: state.sma_slow.value(),
                average_dollar_volume,
                atr: state.atr.value,
                score,
                trend_aligned: trend,
            },
            self.market_regime(ts)?,
        ))
    }

    fn qualifies(&self, instrument_id: InstrumentId, snapshot: &MomentumSnapshot) -> bool {
        let Some(close) = self
            .states
            .get(&instrument_id)
            .and_then(|state| state.bars.back())
            .map(|bar| bar.close.as_decimal())
        else {
            return false;
        };
        if self.config.enable_market_cap_filter
            && self
                .config
                .market_cap_by_instrument
                .get(&instrument_id)
                .is_none_or(|cap| *cap < self.config.minimum_market_cap)
        {
            return false;
        }
        if self.config.enable_earnings_filter
            && self
                .config
                .days_to_earnings_by_instrument
                .get(&instrument_id)
                .is_none_or(|days| *days <= self.config.earnings_blackout_days)
        {
            return false;
        }
        close >= self.config.minimum_price
            && snapshot.average_dollar_volume >= self.config.minimum_average_dollar_volume
            && snapshot.return_short >= self.config.min_return_short
            && snapshot.return_medium >= self.config.min_return_medium
            && snapshot.relative_strength_short >= self.config.min_rs_short
            && snapshot.relative_strength_medium >= self.config.min_rs_medium
            && snapshot.trend_aligned
            && snapshot.atr / close.to_f64().unwrap_or(f64::INFINITY) <= self.config.maximum_atr_pct
            && snapshot.score >= self.config.minimum_momentum_score
    }

    fn pullback_snapshot(
        &self,
        instrument_id: InstrumentId,
        ts: UnixNanos,
    ) -> Option<PullbackSnapshot> {
        let state = self.states.get(&instrument_id)?;
        let count = self.config.pullback_lookback.min(state.bars.len());
        let start = state.bars.len().checked_sub(count)?;
        let bars: Vec<&Bar> = state.bars.iter().skip(start).collect();
        let (high_index, recent_high) = bars.iter().enumerate().max_by_key(|(_, bar)| bar.high)?;
        if high_index + 1 >= bars.len() {
            return None;
        }
        let pullback_bars = &bars[high_index + 1..];
        let advance_start = high_index.saturating_sub(pullback_bars.len().max(5));
        let advance_bars = &bars[advance_start..=high_index];
        let average = |items: &[&Bar], value: fn(&Bar) -> f64| {
            items.iter().map(|bar| value(bar)).sum::<f64>() / items.len() as f64
        };
        let volume_ratio = average(pullback_bars, |bar| bar.volume.as_f64())
            / average(advance_bars, |bar| bar.volume.as_f64()).max(f64::EPSILON);
        let volatility_ratio = average(pullback_bars, |bar| bar.high.as_f64() - bar.low.as_f64())
            / average(advance_bars, |bar| bar.high.as_f64() - bar.low.as_f64()).max(f64::EPSILON);
        let close = bars.last()?.close.as_f64();
        let pullback_pct = (recent_high.high.as_f64() - close) / recent_high.high.as_f64();
        let consecutive_structure_breaks = pullback_bars
            .windows(2)
            .rev()
            .take(3)
            .take_while(|pair| pair[1].high < pair[0].high && pair[1].low < pair[0].low)
            .count();
        let structure_intact =
            close >= state.sma_medium.value() && consecutive_structure_breaks < 3;
        let pullback_type = classify_pullback(pullback_pct, structure_intact, &self.config);
        let quality_score = pullback_quality_score(
            PullbackFactors {
                pullback_pct,
                volume_ratio,
                above_sma_fast: close >= state.sma_fast.value(),
                above_sma_medium: close >= state.sma_medium.value(),
                volatility_ratio,
                structure_intact,
            },
            &self.config,
        );
        let swing_high = pullback_bars
            .iter()
            .take(pullback_bars.len().saturating_sub(1))
            .map(|bar| bar.high)
            .max()
            .unwrap_or(recent_high.high);
        Some(PullbackSnapshot {
            timestamp: ts,
            pullback_type,
            pullback_pct,
            quality_score,
            volume_ratio,
            volatility_ratio,
            recent_high: recent_high.high,
            pullback_swing_high: swing_high,
            pullback_low: pullback_bars.iter().map(|bar| bar.low).min()?,
            structure_intact,
        })
    }

    fn evaluate_symbol(&mut self, instrument_id: InstrumentId) -> anyhow::Result<()> {
        let Some(ts) = self
            .states
            .get(&instrument_id)
            .and_then(|state| state.bars.back())
            .map(|bar| bar.ts_event)
        else {
            return Ok(());
        };
        let state = self.states.get(&instrument_id).expect("checked above");
        if state.last_evaluated == Some(ts)
            || state.active_trade.is_some()
            || state.entry_order_id.is_some()
        {
            return Ok(());
        }
        let Some((momentum, regime)) = self.momentum_snapshot(instrument_id, ts) else {
            return Ok(());
        };
        if let Some(state) = self.states.get_mut(&instrument_id) {
            state.last_evaluated = Some(ts);
            state.momentum = Some(momentum.clone());
        }
        self.event(
            EVENT_SCAN,
            instrument_id,
            ts,
            "daily factors calculated",
            &format!(
                "momentum_score={:.2} rs_short_{}={:.4} rs_medium_{}={:.4} return_short_{}={:.4} return_medium_{}={:.4} regime={regime:?}",
                momentum.score,
                self.config.rs_lookback_short,
                momentum.relative_strength_short,
                self.config.rs_lookback_medium,
                momentum.relative_strength_medium,
                self.config.momentum_lookback_short,
                momentum.return_short,
                self.config.momentum_lookback_medium,
                momentum.return_medium,
            ),
        );

        if !self.qualifies(instrument_id, &momentum) {
            let prior = self.states[&instrument_id].setup_state;
            if matches!(prior, SetupState::Closed | SetupState::Invalidated) {
                self.set_state(instrument_id, SetupState::Watchlist);
            } else if prior != SetupState::Watchlist {
                self.set_state(instrument_id, SetupState::Invalidated);
                self.event(
                    EVENT_INVALIDATED,
                    instrument_id,
                    ts,
                    "momentum, liquidity, metadata, or trend qualification failed",
                    &format!("prior_state={prior:?} momentum_score={:.2}", momentum.score),
                );
                self.set_state(instrument_id, SetupState::Watchlist);
            }
            return Ok(());
        }

        if matches!(
            self.states[&instrument_id].setup_state,
            SetupState::Closed | SetupState::Invalidated
        ) {
            self.set_state(instrument_id, SetupState::Watchlist);
        }
        if self.states[&instrument_id].setup_state == SetupState::Watchlist {
            self.set_state(instrument_id, SetupState::Qualified);
            self.event(
                EVENT_QUALIFY,
                instrument_id,
                ts,
                "strength and trend gates passed",
                &format!("momentum_score={:.2}", momentum.score),
            );
        }

        if self.states[&instrument_id].setup_state == SetupState::Ready
            && self.try_entry_trigger(instrument_id, &momentum, regime)?
        {
            return Ok(());
        }

        let Some(pullback) = self.pullback_snapshot(instrument_id, ts) else {
            return Ok(());
        };
        if pullback.pullback_type == PullbackType::Breakdown {
            if matches!(
                self.states[&instrument_id].setup_state,
                SetupState::Pullback | SetupState::Ready
            ) {
                self.set_state(instrument_id, SetupState::Invalidated);
                self.event(
                    EVENT_INVALIDATED,
                    instrument_id,
                    ts,
                    "pullback structure broke",
                    &format!("pullback_pct={:.4}", pullback.pullback_pct),
                );
                self.set_state(instrument_id, SetupState::Watchlist);
            }
            return Ok(());
        }

        if self.states[&instrument_id].setup_state == SetupState::Qualified {
            self.set_state(instrument_id, SetupState::Pullback);
            self.event(
                EVENT_PULLBACK,
                instrument_id,
                ts,
                "valid pullback depth and structure",
                &format!(
                    "pullback_type={:?} pullback_pct={:.4} volume_ratio={:.3} volatility_ratio={:.3}",
                    pullback.pullback_type,
                    pullback.pullback_pct,
                    pullback.volume_ratio,
                    pullback.volatility_ratio,
                ),
            );
        }
        let ready = pullback.quality_score >= self.config.min_pullback_quality_score
            && pullback.volume_ratio <= self.config.maximum_pullback_volume_ratio
            && pullback.volatility_ratio <= self.config.maximum_pullback_volatility_ratio;
        if let Some(state) = self.states.get_mut(&instrument_id) {
            state.pullback = Some(pullback.clone());
        }
        if ready && self.states[&instrument_id].setup_state == SetupState::Pullback {
            self.set_state(instrument_id, SetupState::Ready);
            self.event(
                EVENT_READY,
                instrument_id,
                ts,
                "pullback quality gates passed",
                &format!("quality_score={:.2}", pullback.quality_score),
            );
        } else if !ready && self.states[&instrument_id].setup_state == SetupState::Ready {
            self.set_state(instrument_id, SetupState::Pullback);
        }
        Ok(())
    }

    fn try_entry_trigger(
        &mut self,
        instrument_id: InstrumentId,
        momentum: &MomentumSnapshot,
        market_regime: MarketRegime,
    ) -> anyhow::Result<bool> {
        if market_regime == MarketRegime::Bear {
            self.increment_count(COUNT_READY_REGIME_BLOCKED);
            return Ok(false);
        }
        let state = self.states.get(&instrument_id).expect("state exists");
        let Some(pullback) = state.pullback.clone() else {
            return Ok(false);
        };
        let Some(current) = state.bars.back().copied() else {
            return Ok(false);
        };
        let Some(previous) = state.bars.get(state.bars.len().saturating_sub(2)).copied() else {
            return Ok(false);
        };
        let volume_count = self
            .config
            .entry_volume_lookback
            .min(state.bars.len().saturating_sub(1));
        if volume_count == 0 {
            return Ok(false);
        }
        let average_volume = state
            .bars
            .iter()
            .rev()
            .skip(1)
            .take(volume_count)
            .map(|bar| bar.volume.as_f64())
            .sum::<f64>()
            / volume_count as f64;
        let volume_ratio = current.volume.as_f64() / average_volume.max(f64::EPSILON);
        let factors = EntryFactors {
            close: current.close.as_f64(),
            previous_close: previous.close.as_f64(),
            previous_high: previous.high.as_f64(),
            pullback_swing_high: pullback.pullback_swing_high.as_f64(),
            sma_fast: state.sma_fast.value(),
            atr: state.atr.value,
            volume_ratio,
            minimum_volume_ratio: self.config.entry_volume_multiplier,
            maximum_extension_atr: self.config.max_entry_extension_atr,
        };
        let status = entry_gate_status(self.config.entry_confirmation_mode, factors);
        self.increment_count(COUNT_READY_EVALUATED);
        for (failed, count) in [
            (!status.volume_confirmed, COUNT_READY_VOLUME_FAILED),
            (!status.breakout_confirmed, COUNT_READY_BREAKOUT_FAILED),
            (!status.extension_acceptable, COUNT_READY_EXTENSION_FAILED),
            (!status.trend_supported, COUNT_READY_TREND_FAILED),
        ] {
            if failed {
                self.increment_count(count);
            }
        }
        if status.failed_count() == 1 {
            let count = if !status.volume_confirmed {
                COUNT_READY_SOLE_VOLUME_FAILED
            } else if !status.breakout_confirmed {
                COUNT_READY_SOLE_BREAKOUT_FAILED
            } else if !status.extension_acceptable {
                COUNT_READY_SOLE_EXTENSION_FAILED
            } else {
                COUNT_READY_SOLE_TREND_FAILED
            };
            self.increment_count(count);
        }
        if !status.confirmed() {
            self.event(
                EVENT_ENTRY_REJECTED,
                instrument_id,
                current.ts_event,
                "one or more entry confirmation gates failed",
                &format!(
                    "gate_status={status:?} close={} previous_high={} volume_ratio={volume_ratio:.3} minimum_volume_ratio={:.3} atr={:.4} maximum_extension_atr={:.3}",
                    current.close,
                    previous.high,
                    self.config.entry_volume_multiplier,
                    state.atr.value,
                    self.config.max_entry_extension_atr,
                ),
            );
            return Ok(false);
        }
        let pending = PendingEntry {
            signal_timestamp: current.ts_event,
            momentum: momentum.clone(),
            pullback: pullback.clone(),
            market_regime,
            sma_fast: state.sma_fast.value(),
            atr: state.atr.value,
        };
        let atr = state.atr.value;
        if let Some(state) = self.states.get_mut(&instrument_id) {
            state.pending_entry = Some(pending);
        }
        self.set_state(instrument_id, SetupState::EntryTriggered);
        self.event(
            EVENT_TRIGGER,
            instrument_id,
            current.ts_event,
            "confirmed continuation after pullback",
            &format!(
                "mode={:?} close={} previous_high={} volume_ratio={volume_ratio:.3} atr={:.4}",
                self.config.entry_confirmation_mode, current.close, previous.high, atr,
            ),
        );
        Ok(true)
    }

    fn is_later_session(&self, signal: UnixNanos, execution: UnixNanos) -> bool {
        execution
            .to_datetime_utc()
            .to_zoned(self.timezone.clone())
            .date()
            > signal
                .to_datetime_utc()
                .to_zoned(self.timezone.clone())
                .date()
    }

    fn pending_entry_is_ranked(&self, instrument_id: InstrumentId) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let open_positions = self
            .cache()
            .positions_open(None, None, Some(&strategy_id), None, None)
            .len();
        let inflight_entries = self
            .states
            .values()
            .filter(|state| state.entry_order_id.is_some())
            .count();
        let available =
            available_position_slots(self.config.max_positions, open_positions, inflight_entries);
        if available == 0 {
            return false;
        }
        let mut candidates: Vec<_> = self
            .states
            .iter()
            .filter_map(|(id, state)| {
                state
                    .pending_entry
                    .as_ref()
                    .map(|pending| (*id, pending.momentum.score))
            })
            .collect();
        candidates.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        candidates
            .iter()
            .take(available)
            .any(|(id, _)| *id == instrument_id)
    }

    fn account_equity_and_free(
        &self,
        instrument: &InstrumentAny,
    ) -> anyhow::Result<(Money, Decimal)> {
        let currency = instrument.quote_currency();
        let venue = instrument.id().venue;
        let equity = self
            .portfolio()
            .equity(&venue, None)
            .get(&currency)
            .copied()
            .or_else(|| {
                self.cache()
                    .account_for_venue(&venue)
                    .and_then(|account| account.balance_total(Some(currency)))
            })
            .ok_or_else(|| anyhow::anyhow!("account equity unavailable for {venue}/{currency}"))?;
        let free = self
            .cache()
            .account_for_venue(&venue)
            .and_then(|account| account.balance_free(Some(currency)))
            .map_or(equity.as_decimal(), |money| money.as_decimal());
        Ok((equity, free))
    }

    fn metadata_risk_allows(
        &self,
        instrument_id: InstrumentId,
        proposed_notional: Decimal,
        equity: Decimal,
    ) -> bool {
        let positions = self.cache().positions_open(None, None, None, None, None);
        if let Some(group) = self
            .config
            .correlation_group_by_instrument
            .get(&instrument_id)
        {
            let correlated = positions
                .iter()
                .filter(|position| {
                    self.config
                        .correlation_group_by_instrument
                        .get(&position.instrument_id)
                        == Some(group)
                })
                .count()
                + self
                    .states
                    .iter()
                    .filter(|(_, state)| {
                        state.entry_order_id.is_some() && state.active_trade.is_none()
                    })
                    .filter(|(id, _)| {
                        self.config.correlation_group_by_instrument.get(id) == Some(group)
                    })
                    .count();
            if correlated >= self.config.max_correlated_positions {
                return false;
            }
        }
        if let Some(sector) = self.config.sector_by_instrument.get(&instrument_id) {
            let sector_notional = positions
                .iter()
                .filter(|position| {
                    self.config
                        .sector_by_instrument
                        .get(&position.instrument_id)
                        == Some(sector)
                })
                .filter_map(|position| {
                    self.cache()
                        .quote(&position.instrument_id)
                        .map(|quote| {
                            (quote.bid_price.as_decimal() + quote.ask_price.as_decimal())
                                / Decimal::TWO
                        })
                        .or_else(|| Decimal::from_f64(position.avg_px_open))
                        .map(|price| {
                            price
                                * position.quantity.as_decimal()
                                * position.multiplier.as_decimal()
                        })
                })
                .sum::<Decimal>()
                + self
                    .states
                    .iter()
                    .filter(|(id, state)| {
                        state.entry_order_id.is_some()
                            && state.active_trade.is_none()
                            && self.config.sector_by_instrument.get(id) == Some(sector)
                    })
                    .filter_map(|(id, state)| {
                        let signal = &state.submitted_entry.as_ref()?.signal;
                        let instrument = self.cache().instrument(id)?;
                        Some(
                            signal.entry_price.as_decimal()
                                * signal.position_size.as_decimal()
                                * instrument.multiplier().as_decimal(),
                        )
                    })
                    .sum::<Decimal>();
            if (sector_notional + proposed_notional) / equity > self.config.max_sector_exposure {
                return false;
            }
        }
        true
    }

    fn try_submit_entry(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let instrument_id = quote.instrument_id;
        let Some(pending) = self
            .states
            .get(&instrument_id)
            .and_then(|state| state.pending_entry.clone())
        else {
            return Ok(());
        };
        if !self.is_later_session(pending.signal_timestamp, quote.ts_event)
            || !self.pending_entry_is_ranked(instrument_id)
        {
            return Ok(());
        }
        let instrument = self.cache().try_instrument(&instrument_id)?.clone();
        let executable_ask = quote.ask_price;
        let support_failed = executable_ask.as_f64() <= pending.sma_fast
            || executable_ask <= pending.pullback.pullback_low;
        let extension_failed = executable_ask.as_f64() - pending.sma_fast
            > self.config.max_entry_extension_atr * pending.atr
            || executable_ask.as_f64() - pending.pullback.recent_high.as_f64()
                > self.config.max_entry_extension_atr * pending.atr;
        self.increment_count(COUNT_NEXT_SESSION_EVALUATED);
        if support_failed {
            self.increment_count(COUNT_NEXT_SESSION_SUPPORT_FAILED);
        }
        if extension_failed {
            self.increment_count(COUNT_NEXT_SESSION_EXTENSION_FAILED);
        }
        if support_failed || extension_failed {
            let reason = match (support_failed, extension_failed) {
                (true, true) => "next-session support failed and price overextended",
                (true, false) => "next-session support failed",
                (false, true) => "next-session price overextended",
                (false, false) => unreachable!(),
            };
            self.invalidate_pending(instrument_id, quote.ts_event, reason);
            return Ok(());
        }
        let atr = Decimal::from_f64(pending.atr)
            .ok_or_else(|| anyhow::anyhow!("ATR cannot be represented as Decimal"))?;
        let entry = match self.config.entry_order_type {
            EntryOrderType::Market => executable_ask,
            EntryOrderType::Limit => instrument.try_make_price_from_decimal(
                executable_ask.as_decimal()
                    - atr
                        * Decimal::from_f64(self.config.entry_limit_offset_atr)
                            .ok_or_else(|| anyhow::anyhow!("limit offset is not finite"))?,
            )?,
        };
        let Some(stop_raw) = atr_stop_price(
            entry.as_decimal(),
            atr,
            self.config.atr_stop_multiple,
            pending.pullback.pullback_low.as_decimal(),
            atr * self.config.stop_buffer_atr,
            self.config.max_stop_distance_pct,
        ) else {
            self.invalidate_pending(
                instrument_id,
                quote.ts_event,
                "stop exceeds maximum distance",
            );
            return Ok(());
        };
        let stop = instrument.try_make_price_from_decimal(stop_raw)?;
        let (equity, free) = self.account_equity_and_free(&instrument)?;
        let notional_cap = (equity.as_decimal() * self.config.maximum_position_notional_pct)
            .min(free)
            .max(Decimal::ZERO);
        let notional_quantity_cap =
            notional_cap / (entry.as_decimal() * instrument.multiplier().as_decimal());
        let hard_limit = self
            .config
            .hard_quantity_limit
            .map_or(notional_quantity_cap, |limit| {
                limit.min(notional_quantity_cap)
            });
        let regime_multiplier = match pending.market_regime {
            MarketRegime::Bull => Decimal::ONE,
            MarketRegime::Neutral => self.config.neutral_position_multiplier,
            MarketRegime::Bear => return Ok(()),
        } * if pending.pullback.pullback_type == PullbackType::Deep {
            self.config.deep_pullback_position_multiplier
        } else {
            Decimal::ONE
        };
        let risk = build_risk_snapshot(
            &instrument,
            entry,
            stop,
            equity,
            self.config.risk_per_trade,
            regime_multiplier,
            self.config.commission_rate,
            self.config.exchange_rate,
            Some(hard_limit),
            self.config.default_lot_size,
        )?;
        if risk.position_size.as_decimal() <= Decimal::ZERO {
            self.invalidate_pending(
                instrument_id,
                quote.ts_event,
                "position size rounded to zero",
            );
            return Ok(());
        }
        let proposed_notional = risk.position_size.as_decimal()
            * entry.as_decimal()
            * instrument.multiplier().as_decimal();
        if !self.metadata_risk_allows(instrument_id, proposed_notional, equity.as_decimal()) {
            self.invalidate_pending(
                instrument_id,
                quote.ts_event,
                "sector or correlation portfolio limit reached",
            );
            return Ok(());
        }
        let order = match self.config.entry_order_type {
            EntryOrderType::Market => self.order().market(
                instrument_id,
                OrderSide::Buy,
                risk.position_size,
                Some(TimeInForce::Day),
                Some(false),
                None,
                None,
                None,
                Some(vec![Ustr::from("MOMENTUM_PULLBACK_ENTRY")]),
                None,
            ),
            EntryOrderType::Limit => self.order().limit(
                instrument_id,
                OrderSide::Buy,
                risk.position_size,
                entry,
                Some(TimeInForce::Day),
                None,
                Some(false),
                Some(false),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(vec![Ustr::from("MOMENTUM_PULLBACK_ENTRY")]),
                None,
            ),
        };
        let signal = self.build_entry_signal(instrument_id, quote.ts_event, &pending, &risk);
        let client_order_id = order.client_order_id();
        self.submit_order(order, None, None, None)?;
        if let Some(state) = self.states.get_mut(&instrument_id) {
            state.pending_entry = None;
            state.entry_order_id = Some(client_order_id);
            state.submitted_entry = Some(SubmittedEntry {
                signal: signal.clone(),
                atr,
                pullback_low: pending.pullback.pullback_low,
            });
        }
        if let Ok(mut report) = self.report.lock() {
            report.signals.push(signal.clone());
        }
        self.event(
            EVENT_SUBMITTED,
            instrument_id,
            quote.ts_event,
            "ranked signal passed portfolio risk",
            &format!(
                "client_order_id={client_order_id} quantity={} entry={} stop={} risk_amount={} momentum_score={:.2}",
                risk.position_size, entry, stop, risk.risk_amount, pending.momentum.score,
            ),
        );
        Ok(())
    }

    fn build_entry_signal(
        &self,
        instrument_id: InstrumentId,
        timestamp: UnixNanos,
        pending: &PendingEntry,
        risk: &RiskSnapshot,
    ) -> EntrySignal {
        EntrySignal {
            symbol: instrument_id,
            timestamp,
            momentum_score: pending.momentum.score,
            relative_strength_short: pending.momentum.relative_strength_short,
            relative_strength_medium: pending.momentum.relative_strength_medium,
            pullback_pct: pending.pullback.pullback_pct,
            pullback_quality: pending.pullback.quality_score,
            entry_price: risk.entry_price,
            stop_price: risk.stop_price,
            risk_per_share: risk.risk_per_share,
            position_size: risk.position_size,
            market_regime: pending.market_regime,
            signal_reason: format!(
                "strength→trend→{:?} pullback→{:?} confirmation; score={:.1}, RS{}={:+.1}%, RS{}={:+.1}%, pullback={:.1}%, volume={:.2}x, ATR={:.2}",
                pending.pullback.pullback_type,
                self.config.entry_confirmation_mode,
                pending.momentum.score,
                self.config.rs_lookback_short,
                pending.momentum.relative_strength_short * 100.0,
                self.config.rs_lookback_medium,
                pending.momentum.relative_strength_medium * 100.0,
                pending.pullback.pullback_pct * 100.0,
                pending.pullback.volume_ratio,
                pending.atr,
            ),
        }
    }

    fn invalidate_pending(&mut self, instrument_id: InstrumentId, ts: UnixNanos, reason: &str) {
        if let Some(state) = self.states.get_mut(&instrument_id) {
            state.pending_entry = None;
        }
        self.set_state(instrument_id, SetupState::Invalidated);
        self.event(EVENT_INVALIDATED, instrument_id, ts, reason, "");
        self.set_state(instrument_id, SetupState::Watchlist);
    }

    fn submit_protective_stop(
        &mut self,
        instrument_id: InstrumentId,
        position_id: PositionId,
        quantity: Quantity,
        stop: Price,
    ) -> anyhow::Result<ClientOrderId> {
        let order = if self.config.protective_stop_uses_market_if_touched {
            self.order().market_if_touched(
                instrument_id,
                OrderSide::Sell,
                quantity,
                stop,
                Some(TriggerType::Default),
                Some(TimeInForce::Gtc),
                None,
                Some(false),
                Some(false),
                None,
                None,
                None,
                None,
                Some(vec![Ustr::from("MOMENTUM_PULLBACK_STOP")]),
                None,
            )
        } else {
            self.order().stop_market(
                instrument_id,
                OrderSide::Sell,
                quantity,
                stop,
                Some(TriggerType::Default),
                Some(TimeInForce::Gtc),
                None,
                Some(true),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(vec![Ustr::from("MOMENTUM_PULLBACK_STOP")]),
                None,
            )
        };
        let id = order.client_order_id();
        self.submit_order(order, Some(position_id), None, None)?;
        Ok(id)
    }

    fn handle_entry_fill(&mut self, event: &OrderFilled) -> anyhow::Result<()> {
        let instrument_id = event.instrument_id;
        let submitted = self.states[&instrument_id]
            .submitted_entry
            .clone()
            .ok_or_else(|| anyhow::anyhow!("entry fill has no submitted signal"))?;
        let position_id = event
            .position_id
            .ok_or_else(|| anyhow::anyhow!("entry fill has no position ID"))?;
        let instrument = self.cache().try_instrument(&instrument_id)?.clone();
        let stop_raw = atr_stop_price(
            event.last_px.as_decimal(),
            submitted.atr,
            self.config.atr_stop_multiple,
            submitted.pullback_low.as_decimal(),
            submitted.atr * self.config.stop_buffer_atr,
            self.config.max_stop_distance_pct,
        )
        .ok_or_else(|| anyhow::anyhow!("actual fill makes stop exceed maximum distance"))?;
        let stop = instrument.try_make_price_from_decimal(stop_raw)?;
        let protective_id =
            self.submit_protective_stop(instrument_id, position_id, event.last_qty, stop)?;
        let state = self.states.get_mut(&instrument_id).expect("state exists");
        let fill_qty = event.last_qty.as_decimal();
        if let Some(active) = state.active_trade.as_mut() {
            let notional =
                active.average_entry * active.quantity + event.last_px.as_decimal() * fill_qty;
            active.quantity += fill_qty;
            active.average_entry = notional / active.quantity;
            active.initial_risk_per_share = active.average_entry - active.initial_stop.as_decimal();
            active
                .protective_orders
                .insert(protective_id, event.last_qty);
        } else {
            state.active_trade = Some(ActiveTrade {
                entry_timestamp: event.ts_event,
                average_entry: event.last_px.as_decimal(),
                quantity: fill_qty,
                initial_stop: stop,
                current_stop: stop,
                initial_risk_per_share: event.last_px.as_decimal() - stop.as_decimal(),
                highest_high: event.last_px.as_f64(),
                bars_held: 0,
                partial_taken: false,
                entry_regime: submitted.signal.market_regime,
                protective_orders: HashMap::from([(protective_id, event.last_qty)]),
            });
        }
        state.setup_state = SetupState::Long;
        let order_closed = self
            .cache()
            .order(&event.client_order_id)
            .is_some_and(|order| order.is_closed());
        if order_closed {
            if let Some(state) = self.states.get_mut(&instrument_id) {
                state.entry_order_id = None;
                state.submitted_entry = None;
            }
        }
        self.event(
            EVENT_FILLED,
            instrument_id,
            event.ts_event,
            "entry filled and broker-side protection submitted",
            &format!(
                "fill_price={} fill_quantity={} stop={} protective_order_id={protective_id}",
                event.last_px, event.last_qty, stop,
            ),
        );
        Ok(())
    }

    fn manage_position(&mut self, instrument_id: InstrumentId) -> anyhow::Result<()> {
        let Some(bar) = self.states[&instrument_id].bars.back().copied() else {
            return Ok(());
        };
        let Some(active) = self.states[&instrument_id].active_trade.as_ref() else {
            return Ok(());
        };
        if bar.ts_event <= active.entry_timestamp
            || self.states[&instrument_id].pending_exit.is_some()
        {
            return Ok(());
        }
        let (decision, new_stop, stop_ids) = {
            let state = &self.states[&instrument_id];
            let active = state.active_trade.as_ref().expect("checked above");
            let highest_high = active.highest_high.max(bar.high.as_f64());
            let swing_low = state
                .bars
                .iter()
                .rev()
                .take(self.config.trailing_swing_lookback)
                .map(|bar| bar.low.as_f64())
                .fold(f64::INFINITY, f64::min);
            let factors = ExitFactors {
                current_price: bar.close.as_f64(),
                initial_risk_per_share: active.initial_risk_per_share.to_string().parse()?,
                entry_price: active.average_entry.to_string().parse()?,
                highest_high,
                atr: state.atr.value,
                sma_fast: state.sma_fast.value(),
                swing_low,
                bars_held: active.bars_held + 1,
                partial_taken: active.partial_taken,
                partial_exit_r: self.config.partial_exit_r,
                partial_exit_fraction: self.config.partial_exit_fraction.to_string().parse()?,
                time_stop_days: self.config.time_stop_days,
                time_stop_min_r: self.config.time_stop_min_r,
                maximum_holding_days: self.config.maximum_holding_days,
                trailing_mode: self.config.trailing_stop_mode,
                chandelier_multiple: self.config.chandelier_atr_multiple,
            };
            let raw_stop = trailing_stop_price(
                self.config.trailing_stop_mode,
                highest_high,
                state.atr.value,
                self.config.chandelier_atr_multiple,
                state.sma_fast.value(),
                swing_low,
            );
            let instrument = self.cache().try_instrument(&instrument_id)?.clone();
            let raw_stop = Decimal::from_f64(raw_stop)
                .ok_or_else(|| anyhow::anyhow!("trailing stop is not finite"))?;
            let normalized = instrument.try_make_price_from_decimal(raw_stop)?;
            (
                evaluate_exit(factors),
                normalized,
                active.protective_orders.keys().copied().collect::<Vec<_>>(),
            )
        };
        if let Some(active) = self
            .states
            .get_mut(&instrument_id)
            .and_then(|s| s.active_trade.as_mut())
        {
            active.highest_high = active.highest_high.max(bar.high.as_f64());
            active.bars_held += 1;
        }
        let current_stop = self.states[&instrument_id]
            .active_trade
            .as_ref()
            .expect("active trade exists")
            .current_stop;
        if new_stop > current_stop && new_stop < bar.close {
            for id in stop_ids {
                self.modify_order(id, None, None, Some(new_stop), None, None)?;
            }
            if let Some(active) = self
                .states
                .get_mut(&instrument_id)
                .and_then(|s| s.active_trade.as_mut())
            {
                active.current_stop = new_stop;
            }
            self.event(
                EVENT_STOP,
                instrument_id,
                bar.ts_event,
                "daily trailing stop ratcheted",
                &format!("old_stop={current_stop} new_stop={new_stop}"),
            );
        }
        if let Some(decision) = decision {
            if let Some(state) = self.states.get_mut(&instrument_id) {
                state.pending_exit = Some(PendingExit {
                    decision,
                    signal_timestamp: bar.ts_event,
                    execution_started: false,
                    exit_order_id: None,
                });
            }
        }
        Ok(())
    }

    fn try_start_exit(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let instrument_id = quote.instrument_id;
        let Some(exit) = self.states[&instrument_id].pending_exit else {
            return Ok(());
        };
        if exit.execution_started || !self.is_later_session(exit.signal_timestamp, quote.ts_event) {
            return Ok(());
        }
        let stop_ids: Vec<_> = self.states[&instrument_id]
            .active_trade
            .as_ref()
            .map(|active| active.protective_orders.keys().copied().collect())
            .unwrap_or_default();
        if let Some(state) = self.states.get_mut(&instrument_id) {
            state.setup_state = SetupState::ExitPending;
            state.awaiting_protective_cancels = stop_ids.iter().copied().collect();
            if let Some(exit) = state.pending_exit.as_mut() {
                exit.execution_started = true;
            }
        }
        for id in stop_ids {
            let open = self
                .cache()
                .order(&id)
                .is_some_and(|order| !order.is_closed());
            if open {
                self.cancel_order(id, None, None)?;
            } else if let Some(state) = self.states.get_mut(&instrument_id) {
                state.awaiting_protective_cancels.remove(&id);
            }
        }
        if self.states[&instrument_id]
            .awaiting_protective_cancels
            .is_empty()
        {
            self.submit_exit_order(instrument_id)?;
        }
        Ok(())
    }

    fn submit_exit_order(&mut self, instrument_id: InstrumentId) -> anyhow::Result<()> {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let Some(position) = self
            .cache()
            .positions_open(
                None,
                Some(&instrument_id),
                Some(&strategy_id),
                None,
                Some(PositionSide::Long),
            )
            .into_iter()
            .next()
        else {
            return Ok(());
        };
        let decision = self.states[&instrument_id]
            .pending_exit
            .expect("pending exit exists")
            .decision;
        let instrument = self.cache().try_instrument(&instrument_id)?.clone();
        let quantity = match decision {
            ExitDecision::Partial { fraction } => {
                let fraction = Decimal::from_f64(fraction)
                    .ok_or_else(|| anyhow::anyhow!("partial fraction is not finite"))?;
                let requested = instrument.try_make_qty_from_decimal(
                    position.quantity.as_decimal() * fraction,
                    Some(true),
                )?;
                if requested.as_decimal() <= Decimal::ZERO
                    || position.quantity.as_decimal() - requested.as_decimal()
                        < instrument
                            .min_quantity()
                            .map_or(Decimal::ONE, |qty| qty.as_decimal())
                {
                    position.quantity
                } else {
                    requested
                }
            }
            ExitDecision::Full { .. } => position.quantity,
        };
        let order = self.order().market(
            instrument_id,
            OrderSide::Sell,
            quantity,
            Some(TimeInForce::Day),
            Some(!self.config.protective_stop_uses_market_if_touched),
            None,
            None,
            None,
            Some(vec![Ustr::from("MOMENTUM_PULLBACK_EXIT")]),
            None,
        );
        let order_id = order.client_order_id();
        self.submit_order(order, Some(position.id), None, None)?;
        if let Some(exit) = self
            .states
            .get_mut(&instrument_id)
            .and_then(|state| state.pending_exit.as_mut())
        {
            exit.exit_order_id = Some(order_id);
        }
        self.event(
            EVENT_SUBMITTED,
            instrument_id,
            self.clock().timestamp_ns(),
            "exit submitted after protective stops canceled",
            &format!("client_order_id={order_id} quantity={quantity} decision={decision:?}"),
        );
        Ok(())
    }

    fn finish_partial_exit(
        &mut self,
        instrument_id: InstrumentId,
        ts: UnixNanos,
    ) -> anyhow::Result<()> {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let position = self
            .cache()
            .positions_open(
                None,
                Some(&instrument_id),
                Some(&strategy_id),
                None,
                Some(PositionSide::Long),
            )
            .into_iter()
            .next();
        let Some(position) = position else {
            return Ok(());
        };
        let stop = self.states[&instrument_id]
            .active_trade
            .as_ref()
            .expect("active trade exists")
            .current_stop;
        let protective =
            self.submit_protective_stop(instrument_id, position.id, position.quantity, stop)?;
        if let Some(state) = self.states.get_mut(&instrument_id) {
            let active = state.active_trade.as_mut().expect("active trade exists");
            active.quantity = position.quantity.as_decimal();
            active.partial_taken = true;
            active.protective_orders.clear();
            active
                .protective_orders
                .insert(protective, position.quantity);
            state.pending_exit = None;
            state.awaiting_protective_cancels.clear();
            state.setup_state = SetupState::Long;
        }
        self.event(
            EVENT_PARTIAL,
            instrument_id,
            ts,
            "2R partial exit completed; remainder re-protected",
            &format!(
                "remaining_quantity={} protective_order_id={protective}",
                position.quantity
            ),
        );
        Ok(())
    }

    fn handle_order_failure(
        &mut self,
        instrument_id: InstrumentId,
        order_id: ClientOrderId,
        reason: &str,
    ) {
        let is_entry = self
            .states
            .get(&instrument_id)
            .is_some_and(|state| state.entry_order_id == Some(order_id));
        if is_entry {
            if let Some(state) = self.states.get_mut(&instrument_id) {
                state.entry_order_id = None;
                state.submitted_entry = None;
            }
            let ts = self.clock().timestamp_ns();
            self.invalidate_pending(instrument_id, ts, reason);
            return;
        }
        let is_protective = self.states.get(&instrument_id).is_some_and(|state| {
            state
                .active_trade
                .as_ref()
                .is_some_and(|active| active.protective_orders.contains_key(&order_id))
        });
        if is_protective {
            log::error!(
                "Protective order {order_id} failed for {instrument_id}: {reason}; scheduling emergency exit",
            );
            if let Some(state) = self.states.get_mut(&instrument_id) {
                state.pending_exit = Some(PendingExit {
                    decision: ExitDecision::Full {
                        reason: ExitReason::RiskFailure,
                    },
                    signal_timestamp: UnixNanos::default(),
                    execution_started: false,
                    exit_order_id: None,
                });
            }
            return;
        }
        if let Some(exit) = self
            .states
            .get_mut(&instrument_id)
            .and_then(|state| state.pending_exit.as_mut())
            && exit.exit_order_id == Some(order_id)
        {
            exit.execution_started = false;
            exit.exit_order_id = None;
            log::error!(
                "Exit order {order_id} failed for {instrument_id}: {reason}; retrying on the next quote",
            );
        }
    }

    fn handle_fill(&mut self, event: &OrderFilled) {
        let instrument_id = event.instrument_id;
        if !self.states.contains_key(&instrument_id) {
            return;
        }
        if self.states[&instrument_id].entry_order_id == Some(event.client_order_id) {
            if let Err(error) = self.handle_entry_fill(event) {
                log::error!("Failed to protect entry fill for {instrument_id}: {error}");
                if let Some(state) = self.states.get_mut(&instrument_id) {
                    state.pending_exit = Some(PendingExit {
                        decision: ExitDecision::Full {
                            reason: ExitReason::RiskFailure,
                        },
                        signal_timestamp: UnixNanos::default(),
                        execution_started: false,
                        exit_order_id: None,
                    });
                }
            }
            return;
        }
        let is_protective = self.states[&instrument_id]
            .active_trade
            .as_ref()
            .is_some_and(|active| {
                active
                    .protective_orders
                    .contains_key(&event.client_order_id)
            });
        if is_protective {
            if let Some(active) = self
                .states
                .get_mut(&instrument_id)
                .and_then(|s| s.active_trade.as_mut())
            {
                active.protective_orders.remove(&event.client_order_id);
            }
            self.event(
                EVENT_EXIT,
                instrument_id,
                event.ts_event,
                "protective stop filled at available market price",
                &format!("fill_price={} quantity={}", event.last_px, event.last_qty),
            );
            return;
        }
        let exit = self.states[&instrument_id].pending_exit;
        if exit.is_some_and(|exit| exit.exit_order_id == Some(event.client_order_id)) {
            let closed = self
                .cache()
                .order(&event.client_order_id)
                .is_some_and(|order| order.is_closed());
            if closed
                && matches!(
                    exit.expect("checked").decision,
                    ExitDecision::Partial { .. }
                )
                && let Err(error) = self.finish_partial_exit(instrument_id, event.ts_event)
            {
                log::error!("Failed to re-protect partial exit for {instrument_id}: {error}");
            }
        }
    }

    fn handle_canceled(&mut self, event: &OrderCanceled) {
        let instrument_id = event.instrument_id;
        let Some(state) = self.states.get_mut(&instrument_id) else {
            return;
        };
        if state.entry_order_id == Some(event.client_order_id) {
            state.entry_order_id = None;
            state.submitted_entry = None;
            state.setup_state = SetupState::Watchlist;
            return;
        }
        if let Some(active) = state.active_trade.as_mut() {
            active.protective_orders.remove(&event.client_order_id);
        }
        state
            .awaiting_protective_cancels
            .remove(&event.client_order_id);
        if state.pending_exit.is_some()
            && state.awaiting_protective_cancels.is_empty()
            && state
                .pending_exit
                .is_some_and(|exit| exit.exit_order_id.is_none())
            && let Err(error) = self.submit_exit_order(instrument_id)
        {
            log::error!("Failed to submit exit for {instrument_id}: {error}");
        }
    }

    fn handle_position_closed(&mut self, event: PositionClosed) {
        let (entry_regime, holding_days) = self
            .states
            .get(&event.instrument_id)
            .and_then(|state| state.active_trade.as_ref())
            .map_or((MarketRegime::Neutral, 0), |active| {
                (active.entry_regime, active.bars_held)
            });
        let closed_at = event.ts_closed.unwrap_or(event.ts_event);
        if let Ok(mut report) = self.report.lock() {
            report.trades.push(TradeRecord {
                symbol: event.instrument_id,
                opened_at: event.ts_opened,
                closed_at,
                average_entry_price: event.avg_px_open,
                average_exit_price: event.avg_px_close.unwrap_or(event.last_px.as_f64()),
                realized_return: event.realized_return,
                realized_pnl: event.realized_pnl,
                peak_quantity: event.peak_quantity,
                holding_days,
                entry_regime,
            });
        }
        if let Some(state) = self.states.get_mut(&event.instrument_id) {
            state.active_trade = None;
            state.pending_exit = None;
            state.awaiting_protective_cancels.clear();
            state.setup_state = SetupState::Closed;
        }
        self.event(
            EVENT_EXIT,
            event.instrument_id,
            event.ts_event,
            "position closed",
            &format!(
                "average_entry={} average_exit={:?} realized_return={:.4} realized_pnl={:?} holding_days={holding_days}",
                event.avg_px_open, event.avg_px_close, event.realized_return, event.realized_pnl,
            ),
        );
    }
}

nautilus_strategy!(MomentumPullbackStrategy, {
    fn on_order_filled(&mut self, event: &OrderFilled) {
        self.handle_fill(event);
    }

    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        self.handle_canceled(event);
    }

    fn on_order_rejected(&mut self, event: OrderRejected) {
        self.handle_order_failure(
            event.instrument_id,
            event.client_order_id,
            event.reason.as_str(),
        );
    }

    fn on_order_denied(&mut self, event: OrderDenied) {
        self.handle_order_failure(
            event.instrument_id,
            event.client_order_id,
            event.reason.as_str(),
        );
    }

    fn on_order_expired(&mut self, event: OrderExpired) {
        self.handle_order_failure(event.instrument_id, event.client_order_id, "order expired");
    }

    fn on_position_closed(&mut self, event: PositionClosed) {
        self.handle_position_closed(event);
    }
});

impl DataActor for MomentumPullbackStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let mut ids: Vec<_> = self.states.keys().copied().collect();
        ids.sort_unstable();
        for instrument_id in ids {
            self.cache().try_instrument(&instrument_id)?;
            self.subscribe_bars(self.bar_type(instrument_id), None, None);
            if self.config.universe.contains(&instrument_id) {
                anyhow::ensure!(
                    self.cache()
                        .positions_open(
                            None,
                            Some(&instrument_id),
                            Some(&strategy_id),
                            None,
                            Some(PositionSide::Long),
                        )
                        .is_empty(),
                    "restart recovery for an existing {instrument_id} position is not implemented; refusing to run unprotected",
                );
                self.subscribe_quotes(instrument_id, None, None);
            }
            if !self.config.bars_are_final
                && let Some(limit) = NonZeroUsize::new(self.config.historical_warmup_bars)
            {
                self.request_bars(
                    self.bar_type(instrument_id),
                    None,
                    None,
                    Some(limit),
                    None,
                    None,
                )?;
            }
        }
        if self.config.enable_market_cap_filter {
            for id in &self.config.universe {
                if !self.config.market_cap_by_instrument.contains_key(id) {
                    log::warn!("Market cap data unavailable for {id}; entries will be rejected");
                }
            }
        } else {
            log::warn!(
                "Market cap data unavailable or filter disabled; market-cap screening is not applied"
            );
        }
        if self.config.enable_earnings_filter {
            for id in &self.config.universe {
                if !self.config.days_to_earnings_by_instrument.contains_key(id) {
                    log::warn!("Earnings calendar unavailable for {id}; entries will be rejected");
                }
            }
        } else {
            log::warn!(
                "Earnings calendar unavailable or filter disabled; earnings blackout is not applied"
            );
        }
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        for instrument_id in self.config.universe.clone() {
            self.unsubscribe_quotes(instrument_id, None, None);
        }
        let ids: Vec<_> = self.states.keys().copied().collect();
        for instrument_id in ids {
            self.unsubscribe_bars(self.bar_type(instrument_id), None, None);
        }
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if bar.bar_type.spec() != self.config.bar_specification
            || bar.bar_type.aggregation_source() != AggregationSource::External
            || !self.states.contains_key(&bar.bar_type.instrument_id())
        {
            return Ok(());
        }
        if let Some(final_bar) = self.accept_bar(*bar) {
            self.process_final_bar(final_bar)?;
        }
        Ok(())
    }

    fn on_historical_bars(&mut self, bars: &[Bar]) -> anyhow::Result<()> {
        let mut bars = bars
            .iter()
            .copied()
            .filter(|bar| {
                bar.bar_type.spec() == self.config.bar_specification
                    && bar.bar_type.aggregation_source() == AggregationSource::External
                    && self.states.contains_key(&bar.bar_type.instrument_id())
            })
            .collect::<Vec<_>>();
        bars.sort_unstable_by_key(|bar| bar.ts_event);
        let Some(last) = bars.last().copied() else {
            return Ok(());
        };
        let instrument_id = last.bar_type.instrument_id();
        let live_bar_is_newer = self.states[&instrument_id]
            .working_bar
            .is_some_and(|working| working.ts_event > last.ts_event);
        let completed = bars.len().saturating_sub(usize::from(!live_bar_is_newer));
        for bar in bars.iter().take(completed).copied() {
            self.process_final_bar(bar)?;
        }
        if completed < bars.len() {
            let state = self.states.get_mut(&instrument_id).expect("state exists");
            if state
                .working_bar
                .is_none_or(|working| working.ts_event < last.ts_event)
            {
                state.working_bar = Some(last);
            }
        }
        log::info!(
            "Loaded {completed} completed warmup bars for {instrument_id}; latest bar retained until completion",
        );
        Ok(())
    }

    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if !self.config.universe.contains(&quote.instrument_id) {
            return Ok(());
        }
        self.try_start_exit(quote)?;
        if self.states[&quote.instrument_id].pending_exit.is_none() {
            self.try_submit_entry(quote)?;
        }
        Ok(())
    }
}

impl Debug for MomentumPullbackStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(MomentumPullbackStrategy))
            .field("universe_size", &self.config.universe.len())
            .field("states", &self.states.len())
            .finish()
    }
}
