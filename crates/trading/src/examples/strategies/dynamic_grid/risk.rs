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

//! Account-aware worst-case reservations and latched loss limits.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    config::{GridConfig, PositionSizing},
    engine::floor_tick,
};

fn unknown_daily_resets() -> u32 {
    // Old checkpoints cannot prove how many resets occurred today; do not invent fresh capacity.
    u32::MAX
}

/// Current marked strategy and broker capacity, without predicted fills.
#[derive(Clone, Debug, Default)]
pub struct RiskSnapshot {
    /// Strategy cash plus marked inventory.
    pub equity: Decimal,
    /// Available strategy cash after actual fill fees.
    pub cash: Decimal,
    /// Marked broker equity (single instrument/account allocation).
    pub account_equity: Decimal,
    /// Broker free cash, excluding broker locks.
    pub account_free: Decimal,
    /// Actual long quantity.
    pub position: Decimal,
    /// Mark-to-market inventory value.
    pub exposure: Decimal,
    /// Inventory PnL including allocated entry fees.
    pub unrealized_pnl: Decimal,
    /// Quantity reserved by unresolved buys, including cancel-pending orders.
    pub pending_buy_quantity: Decimal,
    /// Cash reserved by unresolved buys including estimated costs.
    pub pending_buy_notional: Decimal,
    /// Count of all unresolved orders.
    pub active_orders: usize,
    /// Latest completed-bar ATR divided by the current mark; zero means unavailable.
    pub atr_pct: Decimal,
}

/// Persisted risk state; loss limits never silently unlock on the next bar or restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskManager {
    /// Strategy high-water mark.
    pub peak_equity: Decimal,
    /// Equity when the current UTC day began.
    pub day_start_equity: Decimal,
    /// Most recent equity, used to include overnight gaps in daily loss.
    pub last_equity: Decimal,
    /// UTC date represented by days since Unix epoch.
    pub day: Option<u64>,
    /// Consecutive resets since a profitable completed cycle.
    pub consecutive_resets: u32,
    /// Lifetime number of resets.
    pub total_resets: u64,
    /// Current UTC-day reset count. Missing legacy counts prevent additional same-day resets.
    #[serde(default = "unknown_daily_resets")]
    pub daily_resets: u32,
    /// Largest consecutive reset count observed.
    pub maximum_reset_count: u32,
    /// First hard-limit reason; cleared only by an explicit audited risk reset.
    pub risk_off_reason: Option<String>,
}

impl RiskManager {
    /// Starts with a funded high-water mark.
    #[must_use]
    pub fn new(capital: Decimal) -> Self {
        Self {
            peak_equity: capital,
            day_start_equity: capital,
            last_equity: capital,
            day: None,
            consecutive_resets: 0,
            total_resets: 0,
            daily_resets: 0,
            maximum_reset_count: 0,
            risk_off_reason: None,
        }
    }

    /// Updates loss limits from current marks and reservations.
    pub fn observe(&mut self, config: &GridConfig, snapshot: &RiskSnapshot, now: u64) {
        let day = now / 86_400_000_000_000;
        if self.day != Some(day) {
            self.day = Some(day);
            self.day_start_equity = self.last_equity;
            self.daily_resets = 0;
        }
        // 浮亏也进入权益回撤，不能用已完成网格周期的盈利掩盖仍持有的亏损库存
        self.peak_equity = self.peak_equity.max(snapshot.equity);
        let reason = if snapshot.equity <= Decimal::ZERO {
            Some("Nonpositive equity")
        } else if snapshot.account_equity <= Decimal::ZERO {
            Some("Nonpositive broker equity")
        } else if snapshot.position < Decimal::ZERO {
            Some("Unexpected short inventory")
        } else if snapshot.position + snapshot.pending_buy_quantity > config.max_position {
            Some("Maximum position")
        } else if snapshot.exposure + snapshot.pending_buy_notional
            > config.max_notional.min(config.max_grid_exposure)
        {
            Some("Maximum notional exposure")
        } else if (self.peak_equity - snapshot.equity) >= self.peak_equity * config.max_drawdown {
            Some("Maximum drawdown")
        } else if (self.day_start_equity - snapshot.equity)
            >= self.day_start_equity * config.max_daily_loss
        {
            Some("Maximum daily loss")
        } else if -snapshot.unrealized_pnl >= config.capital * config.max_unrealized_loss {
            Some("Maximum unrealized loss")
        } else if snapshot.active_orders > config.max_orders {
            Some("Maximum number of orders")
        } else if self.consecutive_resets > config.max_consecutive_resets {
            Some("Maximum consecutive resets")
        } else if snapshot.exposure + snapshot.pending_buy_notional
            > snapshot.equity * config.max_position_pct.min(config.max_capital_utilization)
        {
            Some("Maximum capital utilization")
        } else if snapshot.exposure + snapshot.pending_buy_notional
            > snapshot.account_equity * config.max_asset_ratio
        {
            Some("Maximum account asset ratio")
        } else if snapshot.pending_buy_notional > snapshot.account_free.max(Decimal::ZERO) {
            Some("Broker cash capacity changed")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.trip(reason);
        }
        self.last_equity = snapshot.equity;
    }

    /// Latches the first failure; later observations cannot erase it.
    pub fn trip(&mut self, reason: impl Into<String>) {
        if self.risk_off_reason.is_none() {
            let reason = reason.into();
            log::warn!("RISK_LIMIT_TRIGGERED reason={reason}");
            self.risk_off_reason = Some(reason);
        }
    }

    /// Reserves quantity against every configured cap at the worse of mark and limit.
    #[must_use]
    pub fn buy_quantity(
        &self,
        config: &GridConfig,
        snapshot: &RiskSnapshot,
        desired: Decimal,
        price: Decimal,
        lot: Decimal,
    ) -> Decimal {
        if self.risk_off_reason.is_some()
            || snapshot.active_orders >= config.max_orders
            || price <= Decimal::ZERO
            || lot <= Decimal::ZERO
            || snapshot.equity <= Decimal::ZERO
            || snapshot.account_equity <= Decimal::ZERO
        {
            return Decimal::ZERO;
        }
        let exposure_limit = config
            .max_notional
            .min(config.max_grid_exposure)
            .min(snapshot.equity * config.max_position_pct)
            .min(snapshot.equity * config.max_capital_utilization)
            .min(snapshot.account_equity * config.max_asset_ratio);
        // 尚未成交的买单先占用预算，撤单请求本身不释放预算，防止连续信号导致超买
        let cash_limit = (snapshot.cash - config.capital * config.reserve_capital)
            .min(snapshot.account_free)
            .min(exposure_limit - snapshot.exposure)
            - snapshot.pending_buy_notional;
        let unit_cost = price
            * (Decimal::ONE
                + config.maker_fee.max(config.taker_fee)
                + config.commission
                + config.slippage);
        let desired = if config.position_sizing == PositionSizing::VolatilityAdjusted {
            if snapshot.atr_pct <= Decimal::ZERO {
                return Decimal::ZERO;
            }
            desired * (config.position_volatility_target / snapshot.atr_pct).min(Decimal::ONE)
        } else {
            desired
        };
        let quantity = desired
            .min(cash_limit / unit_cost)
            .min(config.max_position - snapshot.position - snapshot.pending_buy_quantity)
            .max(Decimal::ZERO);
        floor_tick(quantity, lot)
    }

    /// Records a completed reset, without injecting new capital.
    pub fn record_reset(&mut self) {
        self.daily_resets = self.daily_resets.saturating_add(1);
        self.consecutive_resets = self.consecutive_resets.saturating_add(1);
        self.total_resets = self.total_resets.saturating_add(1);
        self.maximum_reset_count = self.maximum_reset_count.max(self.consecutive_resets);
    }

    /// Checks both budgets before cancellation and again before committing the reset.
    pub(super) fn reset_limit(&self, config: &GridConfig) -> Option<&'static str> {
        if self.consecutive_resets >= config.max_consecutive_resets {
            Some("Maximum consecutive resets")
        } else if self.daily_resets >= config.maximum_resets_per_day {
            Some("Maximum resets per UTC day")
        } else {
            None
        }
    }
}
