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

//! 纳入账户容量的最坏情形资金预留，以及触发后保持锁定的亏损限制。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    config::{GridConfig, PositionSizing},
    engine::floor_tick,
};

fn unknown_daily_resets() -> u32 {
    // 旧检查点无法证明当日已经重置多少次，因此不能凭空恢复新的重置额度。
    u32::MAX
}

/// 当前策略与券商账户的按市值计价容量，不包含任何假设成交。
#[derive(Clone, Debug, Default)]
pub struct RiskSnapshot {
    /// 策略现金与库存市值之和。
    pub equity: Decimal,
    /// 扣除实际成交费用后的策略可用现金。
    pub cash: Decimal,
    /// 券商账户按市值计价权益，用于约束单标的分配。
    pub account_equity: Decimal,
    /// 券商可用现金，不包含已被券商冻结的资金。
    pub account_free: Decimal,
    /// 实际多头持仓数量。
    pub position: Decimal,
    /// 库存按市值计价的名义金额。
    pub exposure: Decimal,
    /// 包含已分摊入场费用的库存浮动盈亏。
    pub unrealized_pnl: Decimal,
    /// 未终结买单预留数量，包含正在等待撤单确认的订单。
    pub pending_buy_quantity: Decimal,
    /// 未终结买单预留现金，包含估算交易成本。
    pub pending_buy_notional: Decimal,
    /// 全部未终结订单数量。
    pub active_orders: usize,
    /// 最近已完成 K 线 ATR/当前价格；零表示不可用。
    pub atr_pct: Decimal,
}

/// 可持久化风险状态；亏损限制不会因下一根 K 线或程序重启而自动解除。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskManager {
    /// 策略权益历史高水位。
    pub peak_equity: Decimal,
    /// 当前 UTC 日开始时的权益。
    pub day_start_equity: Decimal,
    /// 最近一次权益，用于将隔夜跳空计入当日亏损。
    pub last_equity: Decimal,
    /// 自 Unix 纪元起的天数表示的 UTC 日期。
    pub day: Option<u64>,
    /// 自最近一个盈利完成周期以来的连续重置次数。
    pub consecutive_resets: u32,
    /// 策略生命周期累计重置次数。
    pub total_resets: u64,
    /// 当前 UTC 日重置次数；旧检查点缺少该值时禁止当日继续重置。
    #[serde(default = "unknown_daily_resets")]
    pub daily_resets: u32,
    /// 历史观测到的最大连续重置次数。
    pub maximum_reset_count: u32,
    /// 首个硬性限制原因；只能通过显式、可审计的风险重置清除。
    pub risk_off_reason: Option<String>,
}

impl RiskManager {
    /// 使用初始资金建立权益高水位。
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

    /// 使用当前市值与订单预留更新亏损及暴露限制。
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

    /// 锁存第一个风险失败原因，后续行情观测不能覆盖或自动清除。
    pub fn trip(&mut self, reason: impl Into<String>) {
        if self.risk_off_reason.is_none() {
            let reason = reason.into();
            log::warn!("RISK_LIMIT_TRIGGERED reason={reason}");
            self.risk_off_reason = Some(reason);
        }
    }

    /// 使用市价与限价中更保守的一侧，在全部配置上限内计算可买数量。
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

    /// 记录一次已完成重置，不向策略注入新资金。
    pub fn record_reset(&mut self) {
        self.daily_resets = self.daily_resets.saturating_add(1);
        self.consecutive_resets = self.consecutive_resets.saturating_add(1);
        self.total_resets = self.total_resets.saturating_add(1);
        self.maximum_reset_count = self.maximum_reset_count.max(self.consecutive_resets);
    }

    /// 在发起撤单前和正式提交重置前，都检查连续次数与当日次数预算。
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
