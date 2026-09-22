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

use nautilus_core::UnixNanos;
use nautilus_model::identifiers::InstrumentId;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::reference::{ModelConfig, Timing};
use crate::strategy::StrategyConfig;

/// One regular trading session and its ex-dividend adjustment.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntradayMomentumSession {
    pub open: UnixNanos,
    pub close: UnixNanos,
    #[serde(default)]
    pub dividend: Decimal,
}

/// Configuration for [`super::IntradayMomentumStrategy`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntradayMomentumConfig {
    pub base: StrategyConfig,
    pub instrument_id: InstrumentId,
    pub sessions: Vec<IntradayMomentumSession>,
    pub lookback_days: usize,
    pub volatility_multiplier: Decimal,
    pub relative_volume_threshold: Option<Decimal>,
    pub target_daily_volatility: f64,
    pub max_leverage: Decimal,
    pub capital_fraction: Decimal,
    pub decision_interval_minutes: u64,
    pub flatten_before_close_minutes: u64,
    /// Whether incoming bars already carry their completion timestamp.
    pub bars_are_final: bool,
    pub timing: Timing,
    pub directional_stops: bool,
    pub volume_lookback_days: usize,
    pub volatility_lookback_days: usize,
    /// Legacy OHLC research only; strict Notebook parity requires provider VWAP.
    pub allow_ohlc_vwap_approximation: bool,
    pub dry_run: bool,
    pub dry_run_equity: Decimal,
    pub max_position_notional: Decimal,
    pub max_order_notional: Decimal,
    pub max_orders_per_day: usize,
    /// New entry attempts per session; exits are never restricted by this limit.
    pub max_entries_per_day: Option<usize>,
    pub max_daily_loss: Decimal,
    /// Optional cash-risk cap at entry and an account-equity emergency loss trigger.
    /// Uses a dedicated strategy account; it is not a guaranteed broker stop fill.
    pub risk_per_trade: Option<Decimal>,
    pub stale_data_seconds: u64,
    pub order_timeout_seconds: u64,
    pub retain_features: bool,
    /// Waits for a quote after the signal; used for next-minute-open replay.
    pub defer_to_next_quote: bool,
}

impl Default for IntradayMomentumConfig {
    fn default() -> Self {
        Self {
            base: StrategyConfig::default(),
            instrument_id: InstrumentId::from("SPY.SIM"),
            sessions: Vec::new(),
            lookback_days: 14,
            volatility_multiplier: Decimal::ONE,
            relative_volume_threshold: Some(Decimal::ONE),
            target_daily_volatility: 0.03,
            max_leverage: Decimal::from(4),
            capital_fraction: Decimal::ONE,
            decision_interval_minutes: 30,
            flatten_before_close_minutes: 0,
            bars_are_final: true,
            timing: Timing::SessionClose,
            directional_stops: false,
            volume_lookback_days: 14,
            volatility_lookback_days: 14,
            allow_ohlc_vwap_approximation: false,
            dry_run: true,
            dry_run_equity: Decimal::from(100_000),
            max_position_notional: Decimal::from(400_000),
            max_order_notional: Decimal::from(400_000),
            max_orders_per_day: 100,
            max_entries_per_day: None,
            max_daily_loss: Decimal::new(3, 2),
            risk_per_trade: None,
            stale_data_seconds: 120,
            order_timeout_seconds: 120,
            retain_features: false,
            defer_to_next_quote: false,
        }
    }
}

impl IntradayMomentumConfig {
    /// Validates the strategy and session calendar.
    ///
    /// # Errors
    ///
    /// Returns an error when a parameter is unsafe or a session is malformed.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.model_config().validate()?;
        anyhow::ensure!(
            self.dry_run_equity > Decimal::ZERO
                && self.max_position_notional > Decimal::ZERO
                && self.max_order_notional > Decimal::ZERO,
            "capital and notional limits must be positive"
        );
        anyhow::ensure!(
            self.max_orders_per_day > 0
                && self.max_entries_per_day.is_none_or(|limit| limit > 0)
                && self.stale_data_seconds >= 60
                && self.order_timeout_seconds >= 60,
            "invalid execution safety limits"
        );
        anyhow::ensure!(
            self.max_daily_loss > Decimal::ZERO && self.max_daily_loss <= Decimal::ONE,
            "invalid daily loss limit"
        );
        anyhow::ensure!(
            self.risk_per_trade
                .is_none_or(|v| v > Decimal::ZERO && v <= self.max_daily_loss),
            "trade risk must be positive and at most the daily loss limit"
        );
        anyhow::ensure!(self.lookback_days >= 2, "lookback_days must be at least 2");
        anyhow::ensure!(
            self.volatility_multiplier > Decimal::ZERO,
            "volatility_multiplier must be positive",
        );
        anyhow::ensure!(
            self.relative_volume_threshold
                .is_none_or(|threshold| threshold > Decimal::ZERO),
            "relative_volume_threshold must be positive when configured",
        );
        anyhow::ensure!(
            self.target_daily_volatility.is_finite()
                && self.target_daily_volatility > 0.0
                && self.target_daily_volatility <= 1.0,
            "target_daily_volatility must be finite and in (0, 1]",
        );
        anyhow::ensure!(
            self.max_leverage > Decimal::ZERO && self.max_leverage <= Decimal::from(4),
            "max_leverage must be in (0, 4]",
        );
        anyhow::ensure!(
            self.capital_fraction > Decimal::ZERO && self.capital_fraction <= Decimal::ONE,
            "capital_fraction must be in (0, 1]",
        );
        anyhow::ensure!(
            self.decision_interval_minutes > 0 && self.decision_interval_minutes <= 60,
            "decision_interval_minutes must be in [1, 60]",
        );
        anyhow::ensure!(
            !self.sessions.is_empty(),
            "at least one session is required"
        );

        let minute = 60_000_000_000_u64;
        let mut previous_close = None;
        for session in &self.sessions {
            anyhow::ensure!(
                session.open < session.close,
                "session open must precede close"
            );
            let duration = session.close.as_u64() - session.open.as_u64();
            anyhow::ensure!(
                duration % minute == 0,
                "session duration must be minute-aligned"
            );
            let duration_minutes = duration / minute;
            anyhow::ensure!(
                self.flatten_before_close_minutes < duration_minutes,
                "flatten_before_close_minutes must be shorter than every session",
            );
            anyhow::ensure!(
                session.dividend >= Decimal::ZERO,
                "session dividend must be non-negative",
            );
            if let Some(close) = previous_close {
                anyhow::ensure!(
                    close < session.open,
                    "sessions must be sorted and non-overlapping"
                );
            }
            previous_close = Some(session.close);
        }
        Ok(())
    }

    #[must_use]
    pub fn model_config(&self) -> ModelConfig {
        ModelConfig {
            lookback: self.lookback_days,
            volume_lookback: self.volume_lookback_days,
            volatility_lookback: self.volatility_lookback_days,
            volatility_multiplier: self.volatility_multiplier,
            relative_volume_threshold: self.relative_volume_threshold,
            target_volatility: self.target_daily_volatility,
            max_leverage: self.max_leverage,
            interval_minutes: self.decision_interval_minutes as usize,
            timing: self.timing,
            baseline_stop: false,
            directional_stops: self.directional_stops,
        }
    }

    #[must_use]
    pub fn session(&self, timestamp: UnixNanos) -> Option<IntradayMomentumSession> {
        // Validation requires a sorted, non-overlapping calendar
        let index = self
            .sessions
            .partition_point(|session| session.open < timestamp);
        self.sessions
            .get(index.checked_sub(1)?)
            .copied()
            .filter(|session| timestamp <= session.close)
    }
}
