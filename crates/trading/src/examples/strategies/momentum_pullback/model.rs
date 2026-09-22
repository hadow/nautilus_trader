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

//! Pure factor, signal, risk, and exit models.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use nautilus_core::UnixNanos;
use nautilus_model::{
    identifiers::InstrumentId,
    instruments::{Instrument, InstrumentAny},
    types::{Money, Price, Quantity},
};
use nautilus_risk::sizing::calculate_fixed_risk_position_size;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::config::MomentumPullbackConfig;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketRegime {
    Bull,
    #[default]
    Neutral,
    Bear,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PullbackType {
    Shallow,
    #[default]
    Normal,
    Deep,
    Breakdown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryConfirmationMode {
    Relaxed,
    #[default]
    Standard,
    Strict,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryOrderType {
    #[default]
    Market,
    Limit,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrailingStopMode {
    #[default]
    AtrChandelier,
    MaFast,
    SwingLow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SetupState {
    #[default]
    Watchlist,
    Qualified,
    Pullback,
    Ready,
    EntryTriggered,
    Long,
    ExitPending,
    Closed,
    Invalidated,
}

impl SetupState {
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Watchlist, Self::Qualified)
                | (
                    Self::Qualified,
                    Self::Pullback | Self::Watchlist | Self::Invalidated
                )
                | (
                    Self::Pullback,
                    Self::Ready | Self::Invalidated | Self::Watchlist
                )
                | (
                    Self::Ready,
                    Self::EntryTriggered | Self::Pullback | Self::Invalidated
                )
                | (Self::EntryTriggered, Self::Long | Self::Invalidated)
                | (Self::Long, Self::ExitPending)
                | (Self::ExitPending, Self::Closed | Self::Long)
                | (Self::Closed | Self::Invalidated, Self::Watchlist)
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScoreWeights {
    pub relative_strength: f64,
    pub momentum_short: f64,
    pub momentum_medium: f64,
    pub trend: f64,
    pub volume: f64,
    pub volatility: f64,
}

impl ScoreWeights {
    #[must_use]
    pub const fn total(self) -> f64 {
        self.relative_strength
            + self.momentum_short
            + self.momentum_medium
            + self.trend
            + self.volume
            + self.volatility
    }
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            relative_strength: 30.0,
            momentum_short: 20.0,
            momentum_medium: 20.0,
            trend: 15.0,
            volume: 10.0,
            volatility: 5.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MomentumFactors {
    pub relative_strength: f64,
    pub momentum_short: f64,
    pub momentum_medium: f64,
    pub trend: f64,
    pub volume: f64,
    pub volatility: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PullbackFactors {
    pub pullback_pct: f64,
    pub volume_ratio: f64,
    pub above_sma_fast: bool,
    pub above_sma_medium: bool,
    pub volatility_ratio: f64,
    pub structure_intact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct EntryFactors {
    pub close: f64,
    pub previous_close: f64,
    pub previous_high: f64,
    pub pullback_swing_high: f64,
    pub sma_fast: f64,
    pub atr: f64,
    pub volume_ratio: f64,
    pub minimum_volume_ratio: f64,
    pub maximum_extension_atr: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EntryGateStatus {
    pub trend_supported: bool,
    pub volume_confirmed: bool,
    pub breakout_confirmed: bool,
    pub extension_acceptable: bool,
}

impl EntryGateStatus {
    #[must_use]
    pub(super) const fn confirmed(self) -> bool {
        self.trend_supported
            && self.volume_confirmed
            && self.breakout_confirmed
            && self.extension_acceptable
    }

    #[must_use]
    pub(super) const fn failed_count(self) -> u8 {
        (!self.trend_supported) as u8
            + (!self.volume_confirmed) as u8
            + (!self.breakout_confirmed) as u8
            + (!self.extension_acceptable) as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExitReason {
    TrailingStop,
    TimeStop,
    MaximumHoldingPeriod,
    RiskFailure,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ExitDecision {
    Partial { fraction: f64 },
    Full { reason: ExitReason },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ExitFactors {
    pub current_price: f64,
    pub initial_risk_per_share: f64,
    pub entry_price: f64,
    pub highest_high: f64,
    pub atr: f64,
    pub sma_fast: f64,
    pub swing_low: f64,
    pub bars_held: usize,
    pub partial_taken: bool,
    pub partial_exit_r: f64,
    pub partial_exit_fraction: f64,
    pub time_stop_days: usize,
    pub time_stop_min_r: f64,
    pub maximum_holding_days: usize,
    pub trailing_mode: TrailingStopMode,
    pub chandelier_multiple: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MomentumSnapshot {
    pub timestamp: UnixNanos,
    pub return_short: f64,
    pub return_medium: f64,
    pub relative_strength_short: f64,
    pub relative_strength_medium: f64,
    pub sma_fast: f64,
    pub sma_medium: f64,
    pub sma_slow: f64,
    pub average_dollar_volume: Decimal,
    pub atr: f64,
    pub score: f64,
    pub trend_aligned: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullbackSnapshot {
    pub timestamp: UnixNanos,
    pub pullback_type: PullbackType,
    pub pullback_pct: f64,
    pub quality_score: f64,
    pub volume_ratio: f64,
    pub volatility_ratio: f64,
    pub recent_high: Price,
    pub pullback_swing_high: Price,
    pub pullback_low: Price,
    pub structure_intact: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RiskSnapshot {
    pub entry_price: Price,
    pub stop_price: Price,
    pub risk_per_share: Decimal,
    pub risk_amount: Decimal,
    pub position_size: Quantity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EntrySignal {
    pub symbol: InstrumentId,
    pub timestamp: UnixNanos,
    pub momentum_score: f64,
    pub relative_strength_short: f64,
    pub relative_strength_medium: f64,
    pub pullback_pct: f64,
    pub pullback_quality: f64,
    pub entry_price: Price,
    pub stop_price: Price,
    pub risk_per_share: Decimal,
    pub position_size: Quantity,
    pub market_regime: MarketRegime,
    pub signal_reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TradeRecord {
    pub symbol: InstrumentId,
    pub opened_at: UnixNanos,
    pub closed_at: UnixNanos,
    pub average_entry_price: f64,
    pub average_exit_price: f64,
    pub realized_return: f64,
    pub realized_pnl: Option<Money>,
    pub peak_quantity: Quantity,
    pub holding_days: usize,
    pub entry_regime: MarketRegime,
}

#[derive(Debug, Default)]
pub struct MomentumPullbackReport {
    pub signals: Vec<EntrySignal>,
    pub trades: Vec<TradeRecord>,
    pub event_counts: BTreeMap<String, u64>,
}

pub type SharedMomentumPullbackReport = Arc<Mutex<MomentumPullbackReport>>;

#[must_use]
pub(super) fn percent_return(current: f64, previous: f64) -> Option<f64> {
    (current.is_finite() && previous.is_finite() && previous > 0.0)
        .then_some(current / previous - 1.0)
}

#[must_use]
pub(super) fn relative_strength(stock_return: f64, market_return: f64) -> f64 {
    stock_return - market_return
}

#[must_use]
pub(super) const fn available_position_slots(
    maximum: usize,
    open: usize,
    inflight: usize,
) -> usize {
    maximum.saturating_sub(open.saturating_add(inflight))
}

#[must_use]
pub(super) fn trend_aligned(close: f64, fast: f64, medium: f64, slow: f64) -> bool {
    close > fast && fast > medium && medium > slow
}

#[must_use]
pub(super) fn momentum_score(factors: MomentumFactors, weights: ScoreWeights) -> f64 {
    let component = |value: f64, weight: f64| value.clamp(0.0, 1.0) * weight;
    (component(factors.relative_strength, weights.relative_strength)
        + component(factors.momentum_short, weights.momentum_short)
        + component(factors.momentum_medium, weights.momentum_medium)
        + component(factors.trend, weights.trend)
        + component(factors.volume, weights.volume)
        + component(factors.volatility, weights.volatility))
    .clamp(0.0, 100.0)
}

#[must_use]
pub(super) fn classify_pullback(
    pullback_pct: f64,
    structure_intact: bool,
    config: &MomentumPullbackConfig,
) -> PullbackType {
    if !structure_intact
        || pullback_pct < config.pullback_min_pct
        || pullback_pct > config.pullback_max_pct
    {
        PullbackType::Breakdown
    } else if pullback_pct <= config.pullback_shallow_max_pct {
        PullbackType::Shallow
    } else if pullback_pct <= config.pullback_normal_max_pct {
        PullbackType::Normal
    } else {
        PullbackType::Deep
    }
}

#[must_use]
pub(super) fn pullback_quality_score(
    factors: PullbackFactors,
    config: &MomentumPullbackConfig,
) -> f64 {
    let ideal_depth = (config.pullback_min_pct + config.pullback_normal_max_pct) / 2.0;
    let depth_tolerance = (ideal_depth - config.pullback_min_pct)
        .max(config.pullback_max_pct - ideal_depth)
        .max(f64::EPSILON);
    let depth =
        (1.0 - (factors.pullback_pct - ideal_depth).abs() / depth_tolerance).clamp(0.0, 1.0);
    let volume = ((1.0 - factors.volume_ratio)
        / (1.0 - config.maximum_pullback_volume_ratio).max(f64::EPSILON))
    .clamp(0.0, 1.0);
    let support = if factors.above_sma_fast {
        1.0
    } else if factors.above_sma_medium {
        0.5
    } else {
        0.0
    };
    let volatility = ((config.maximum_pullback_volatility_ratio - factors.volatility_ratio)
        / (config.maximum_pullback_volatility_ratio - 1.0).max(f64::EPSILON))
    .clamp(0.0, 1.0);
    let structure = f64::from(factors.structure_intact);

    (depth * 35.0 + volume * 25.0 + support * 20.0 + volatility * 10.0 + structure * 10.0)
        .clamp(0.0, 100.0)
}

#[must_use]
pub(super) fn is_overextended(close: f64, sma_fast: f64, atr: f64, maximum_atr: f64) -> bool {
    atr <= 0.0 || close - sma_fast > maximum_atr * atr
}

#[must_use]
pub(super) fn entry_gate_status(
    mode: EntryConfirmationMode,
    factors: EntryFactors,
) -> EntryGateStatus {
    let breakout_confirmed = match mode {
        EntryConfirmationMode::Relaxed => factors.close > factors.previous_close,
        EntryConfirmationMode::Standard => factors.close > factors.previous_high,
        EntryConfirmationMode::Strict => factors.close > factors.pullback_swing_high,
    };
    EntryGateStatus {
        trend_supported: factors.close > factors.sma_fast,
        volume_confirmed: factors.volume_ratio >= factors.minimum_volume_ratio,
        breakout_confirmed,
        extension_acceptable: !is_overextended(
            factors.close,
            factors.sma_fast,
            factors.atr,
            factors.maximum_extension_atr,
        ),
    }
}

#[must_use]
pub(super) fn classify_market_regime(close: f64, sma_medium: f64, sma_slow: f64) -> MarketRegime {
    if close > sma_slow && sma_medium > sma_slow {
        MarketRegime::Bull
    } else if close < sma_slow && sma_medium < sma_slow {
        MarketRegime::Bear
    } else {
        MarketRegime::Neutral
    }
}

#[must_use]
pub(super) fn atr_stop_price(
    entry: Decimal,
    atr: Decimal,
    atr_multiple: Decimal,
    pullback_low: Decimal,
    buffer: Decimal,
    maximum_distance_pct: Decimal,
) -> Option<Decimal> {
    let stop = (entry - atr * atr_multiple).min(pullback_low - buffer);
    let distance = entry - stop;
    (stop > Decimal::ZERO && distance > Decimal::ZERO && distance / entry <= maximum_distance_pct)
        .then_some(stop)
}

/// Builds a fixed-risk position plan with Nautilus instrument constraints.
///
/// # Errors
///
/// Returns an error when inputs are invalid or exact sizing cannot be represented.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the reused fixed-risk sizing API"
)]
pub(super) fn build_risk_snapshot(
    instrument: &InstrumentAny,
    entry: Price,
    stop: Price,
    equity: Money,
    risk_fraction: Decimal,
    regime_multiplier: Decimal,
    commission_rate: Decimal,
    exchange_rate: Decimal,
    hard_quantity_limit: Option<Decimal>,
    default_lot_size: Quantity,
) -> anyhow::Result<RiskSnapshot> {
    anyhow::ensure!(
        regime_multiplier > Decimal::ZERO && regime_multiplier <= Decimal::ONE,
        "regime position multiplier must be in (0, 1]",
    );
    let adjusted_risk = risk_fraction
        .checked_mul(regime_multiplier)
        .ok_or_else(|| anyhow::anyhow!("risk fraction overflow"))?;
    let lot_size = instrument
        .lot_size()
        .map_or(default_lot_size.as_decimal(), |quantity| {
            quantity.as_decimal()
        });
    let position_size = calculate_fixed_risk_position_size(
        instrument,
        entry,
        stop,
        equity,
        adjusted_risk,
        commission_rate,
        exchange_rate,
        hard_quantity_limit,
        lot_size,
        1,
    )?;
    let risk_per_share = entry
        .as_decimal()
        .checked_sub(stop.as_decimal())
        .filter(|value| *value > Decimal::ZERO)
        .ok_or_else(|| anyhow::anyhow!("long entry price must be above stop price"))?;
    let risk_amount = equity
        .as_decimal()
        .checked_mul(adjusted_risk)
        .ok_or_else(|| anyhow::anyhow!("risk amount overflow"))?;

    Ok(RiskSnapshot {
        entry_price: entry,
        stop_price: stop,
        risk_per_share,
        risk_amount,
        position_size,
    })
}

#[must_use]
pub(super) fn trailing_stop_price(
    mode: TrailingStopMode,
    highest_high: f64,
    atr: f64,
    chandelier_multiple: f64,
    sma_fast: f64,
    swing_low: f64,
) -> f64 {
    match mode {
        TrailingStopMode::AtrChandelier => highest_high - chandelier_multiple * atr,
        TrailingStopMode::MaFast => sma_fast,
        TrailingStopMode::SwingLow => swing_low,
    }
}

#[must_use]
pub(super) fn evaluate_exit(factors: ExitFactors) -> Option<ExitDecision> {
    let current_r = (factors.current_price - factors.entry_price) / factors.initial_risk_per_share;
    if factors.bars_held >= factors.maximum_holding_days {
        return Some(ExitDecision::Full {
            reason: ExitReason::MaximumHoldingPeriod,
        });
    }
    if !factors.partial_taken && current_r >= factors.partial_exit_r {
        return Some(ExitDecision::Partial {
            fraction: factors.partial_exit_fraction,
        });
    }
    if factors.bars_held >= factors.time_stop_days && current_r < factors.time_stop_min_r {
        return Some(ExitDecision::Full {
            reason: ExitReason::TimeStop,
        });
    }
    let trailing = trailing_stop_price(
        factors.trailing_mode,
        factors.highest_high,
        factors.atr,
        factors.chandelier_multiple,
        factors.sma_fast,
        factors.swing_low,
    );
    (factors.current_price < trailing).then_some(ExitDecision::Full {
        reason: ExitReason::TrailingStop,
    })
}
