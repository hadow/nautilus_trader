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

//! Configuration for the momentum pullback strategy.

use std::collections::HashMap;

use nautilus_model::{
    data::{BarSpecification, bar::BAR_SPEC_1_DAY_LAST},
    identifiers::InstrumentId,
    types::Quantity,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::model::{EntryConfirmationMode, EntryOrderType, ScoreWeights, TrailingStopMode};
use crate::strategy::StrategyConfig;

/// Configuration for [`MomentumPullbackStrategy`](super::MomentumPullbackStrategy).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MomentumPullbackConfig {
    pub base: StrategyConfig,
    pub universe: Vec<InstrumentId>,
    pub market_regime_instrument_id: InstrumentId,
    pub secondary_market_instrument_id: InstrumentId,
    pub relative_strength_instrument_id: InstrumentId,
    pub bar_specification: BarSpecification,
    pub bars_are_final: bool,
    pub historical_warmup_bars: usize,
    pub timezone: String,
    pub momentum_lookback_short: usize,
    pub momentum_lookback_medium: usize,
    pub min_return_short: f64,
    pub min_return_medium: f64,
    pub rs_lookback_short: usize,
    pub rs_lookback_medium: usize,
    pub min_rs_short: f64,
    pub min_rs_medium: f64,
    pub sma_fast: usize,
    pub sma_medium: usize,
    pub sma_slow: usize,
    pub regime_sma_medium: usize,
    pub regime_sma_slow: usize,
    pub pullback_lookback: usize,
    pub pullback_min_pct: f64,
    pub pullback_shallow_max_pct: f64,
    pub pullback_normal_max_pct: f64,
    pub pullback_max_pct: f64,
    pub min_pullback_quality_score: f64,
    pub maximum_pullback_volume_ratio: f64,
    pub maximum_pullback_volatility_ratio: f64,
    pub entry_volume_lookback: usize,
    pub entry_volume_multiplier: f64,
    pub entry_confirmation_mode: EntryConfirmationMode,
    pub entry_order_type: EntryOrderType,
    pub entry_limit_offset_atr: f64,
    pub max_entry_extension_atr: f64,
    pub atr_period: usize,
    pub atr_stop_multiple: Decimal,
    pub stop_buffer_atr: Decimal,
    pub max_stop_distance_pct: Decimal,
    pub risk_per_trade: Decimal,
    pub neutral_position_multiplier: Decimal,
    pub deep_pullback_position_multiplier: Decimal,
    pub maximum_position_notional_pct: Decimal,
    pub max_positions: usize,
    pub max_sector_exposure: Decimal,
    pub max_correlated_positions: usize,
    pub sector_by_instrument: HashMap<InstrumentId, String>,
    pub correlation_group_by_instrument: HashMap<InstrumentId, String>,
    pub enable_market_cap_filter: bool,
    pub minimum_market_cap: Decimal,
    pub market_cap_by_instrument: HashMap<InstrumentId, Decimal>,
    pub enable_earnings_filter: bool,
    pub earnings_blackout_days: u16,
    pub days_to_earnings_by_instrument: HashMap<InstrumentId, u16>,
    pub minimum_price: Decimal,
    pub minimum_average_dollar_volume: Decimal,
    pub score_weights: ScoreWeights,
    pub minimum_momentum_score: f64,
    pub maximum_atr_pct: f64,
    pub trailing_stop_mode: TrailingStopMode,
    pub chandelier_atr_multiple: f64,
    pub trailing_swing_lookback: usize,
    pub partial_exit_r: f64,
    pub partial_exit_fraction: Decimal,
    pub time_stop_days: usize,
    pub time_stop_min_r: f64,
    pub maximum_holding_days: usize,
    pub protective_stop_uses_market_if_touched: bool,
    pub commission_rate: Decimal,
    pub exchange_rate: Decimal,
    pub hard_quantity_limit: Option<Decimal>,
    pub default_lot_size: Quantity,
}

impl Default for MomentumPullbackConfig {
    fn default() -> Self {
        Self {
            base: StrategyConfig::default(),
            universe: Vec::new(),
            market_regime_instrument_id: InstrumentId::from("SPY.SIM"),
            secondary_market_instrument_id: InstrumentId::from("QQQ.SIM"),
            relative_strength_instrument_id: InstrumentId::from("SPY.SIM"),
            bar_specification: BAR_SPEC_1_DAY_LAST,
            bars_are_final: true,
            historical_warmup_bars: 260,
            timezone: "America/New_York".to_string(),
            momentum_lookback_short: 20,
            momentum_lookback_medium: 60,
            min_return_short: 0.05,
            min_return_medium: 0.10,
            rs_lookback_short: 20,
            rs_lookback_medium: 60,
            min_rs_short: 0.03,
            min_rs_medium: 0.05,
            sma_fast: 20,
            sma_medium: 50,
            sma_slow: 200,
            regime_sma_medium: 50,
            regime_sma_slow: 200,
            pullback_lookback: 20,
            pullback_min_pct: 0.03,
            pullback_shallow_max_pct: 0.04,
            pullback_normal_max_pct: 0.08,
            pullback_max_pct: 0.10,
            min_pullback_quality_score: 60.0,
            maximum_pullback_volume_ratio: 0.80,
            maximum_pullback_volatility_ratio: 1.50,
            entry_volume_lookback: 20,
            entry_volume_multiplier: 1.20,
            entry_confirmation_mode: EntryConfirmationMode::Standard,
            entry_order_type: EntryOrderType::Market,
            entry_limit_offset_atr: 0.25,
            max_entry_extension_atr: 2.0,
            atr_period: 14,
            atr_stop_multiple: Decimal::new(15, 1),
            stop_buffer_atr: Decimal::new(1, 1),
            max_stop_distance_pct: Decimal::new(8, 2),
            risk_per_trade: Decimal::new(5, 3),
            neutral_position_multiplier: Decimal::new(5, 1),
            deep_pullback_position_multiplier: Decimal::new(5, 1),
            maximum_position_notional_pct: Decimal::new(20, 2),
            max_positions: 10,
            max_sector_exposure: Decimal::new(30, 2),
            max_correlated_positions: 2,
            sector_by_instrument: HashMap::new(),
            correlation_group_by_instrument: HashMap::new(),
            enable_market_cap_filter: false,
            minimum_market_cap: Decimal::from(5_000_000_000_u64),
            market_cap_by_instrument: HashMap::new(),
            enable_earnings_filter: false,
            earnings_blackout_days: 2,
            days_to_earnings_by_instrument: HashMap::new(),
            minimum_price: Decimal::from(10),
            minimum_average_dollar_volume: Decimal::from(30_000_000),
            score_weights: ScoreWeights::default(),
            minimum_momentum_score: 70.0,
            maximum_atr_pct: 0.08,
            trailing_stop_mode: TrailingStopMode::AtrChandelier,
            chandelier_atr_multiple: 3.0,
            trailing_swing_lookback: 5,
            partial_exit_r: 2.0,
            partial_exit_fraction: Decimal::new(5, 1),
            time_stop_days: 5,
            time_stop_min_r: 0.5,
            maximum_holding_days: 30,
            protective_stop_uses_market_if_touched: false,
            commission_rate: Decimal::ZERO,
            exchange_rate: Decimal::ONE,
            hard_quantity_limit: None,
            default_lot_size: Quantity::from(1),
        }
    }
}

impl MomentumPullbackConfig {
    /// Validates strategy parameters and configured metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when a parameter would make the strategy unsafe or ambiguous.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.universe.is_empty(),
            "momentum pullback universe must not be empty"
        );
        anyhow::ensure!(
            self.sma_fast < self.sma_medium && self.sma_medium < self.sma_slow,
            "momentum pullback SMA periods must satisfy fast < medium < slow",
        );
        anyhow::ensure!(
            self.regime_sma_medium > 0 && self.regime_sma_medium < self.regime_sma_slow,
            "market regime SMA periods must be positive and ordered",
        );
        anyhow::ensure!(
            self.momentum_lookback_short > 0
                && self.momentum_lookback_short <= self.momentum_lookback_medium
                && self.rs_lookback_short > 0
                && self.rs_lookback_short <= self.rs_lookback_medium
                && self.entry_volume_lookback > 0
                && self.atr_period > 0
                && self.pullback_lookback >= 3
                && self.trailing_swing_lookback > 0,
            "strategy lookback periods must be positive and ordered",
        );
        anyhow::ensure!(
            self.pullback_min_pct > 0.0
                && self.pullback_min_pct <= self.pullback_shallow_max_pct
                && self.pullback_shallow_max_pct < self.pullback_normal_max_pct
                && self.pullback_normal_max_pct <= self.pullback_max_pct,
            "momentum pullback depth thresholds must be positive and ordered",
        );
        anyhow::ensure!(
            self.risk_per_trade > Decimal::ZERO && self.risk_per_trade <= Decimal::ONE,
            "risk per trade must be in (0, 1]",
        );
        for (name, multiplier) in [
            (
                "neutral_position_multiplier",
                self.neutral_position_multiplier,
            ),
            (
                "deep_pullback_position_multiplier",
                self.deep_pullback_position_multiplier,
            ),
            (
                "maximum_position_notional_pct",
                self.maximum_position_notional_pct,
            ),
            ("max_sector_exposure", self.max_sector_exposure),
        ] {
            anyhow::ensure!(
                multiplier > Decimal::ZERO && multiplier <= Decimal::ONE,
                "{name} must be in (0, 1]",
            );
        }
        anyhow::ensure!(
            self.max_stop_distance_pct > Decimal::ZERO
                && self.max_stop_distance_pct <= Decimal::ONE,
            "maximum stop distance must be in (0, 1]",
        );
        anyhow::ensure!(
            self.partial_exit_fraction > Decimal::ZERO && self.partial_exit_fraction < Decimal::ONE,
            "partial exit fraction must be in (0, 1)",
        );
        anyhow::ensure!(self.max_positions > 0, "maximum positions must be positive");
        anyhow::ensure!(
            self.maximum_pullback_volume_ratio > 0.0 && self.maximum_pullback_volume_ratio < 1.0,
            "maximum pullback volume ratio must be in (0, 1)",
        );
        anyhow::ensure!(
            self.maximum_pullback_volatility_ratio > 1.0,
            "maximum pullback volatility ratio must exceed 1",
        );
        anyhow::ensure!(
            [
                self.entry_volume_multiplier,
                self.max_entry_extension_atr,
                self.maximum_atr_pct,
                self.chandelier_atr_multiple,
                self.partial_exit_r,
            ]
            .into_iter()
            .all(|value| value.is_finite() && value > 0.0)
                && self.entry_limit_offset_atr.is_finite()
                && self.entry_limit_offset_atr >= 0.0,
            "entry, volatility, and exit multipliers must be finite and positive",
        );
        anyhow::ensure!(
            self.maximum_holding_days >= self.time_stop_days,
            "maximum holding days must not be shorter than the time stop",
        );
        anyhow::ensure!(self.time_stop_days > 0, "time stop days must be positive");
        anyhow::ensure!(
            self.max_correlated_positions > 0,
            "maximum correlated positions must be positive",
        );
        anyhow::ensure!(
            (self.score_weights.total() - 100.0).abs() < 1e-9,
            "momentum score weights must sum to 100",
        );
        anyhow::ensure!(
            [
                self.score_weights.relative_strength,
                self.score_weights.momentum_short,
                self.score_weights.momentum_medium,
                self.score_weights.trend,
                self.score_weights.volume,
                self.score_weights.volatility,
            ]
            .into_iter()
            .all(|weight| weight.is_finite() && weight >= 0.0),
            "momentum score weights must be finite and non-negative",
        );
        anyhow::ensure!(
            self.minimum_momentum_score.is_finite()
                && (0.0..=100.0).contains(&self.minimum_momentum_score),
            "minimum momentum score must be in [0, 100]",
        );
        anyhow::ensure!(
            self.atr_stop_multiple > Decimal::ZERO
                && self.stop_buffer_atr >= Decimal::ZERO
                && self.exchange_rate > Decimal::ZERO
                && self.commission_rate >= Decimal::ZERO
                && self.default_lot_size.as_decimal() > Decimal::ZERO,
            "ATR risk, exchange rate, commission, and lot-size values are invalid",
        );
        anyhow::ensure!(
            self.minimum_price > Decimal::ZERO
                && self.minimum_average_dollar_volume >= Decimal::ZERO
                && (!self.enable_market_cap_filter || self.minimum_market_cap > Decimal::ZERO),
            "universe filter thresholds are invalid",
        );
        Ok(())
    }
}
