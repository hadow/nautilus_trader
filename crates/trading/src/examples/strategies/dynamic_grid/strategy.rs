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

//! NautilusTrader 原生动态网格策略；运行环境由 runner 选择，信号逻辑不区分回测或实盘。

use std::{cell::RefCell, collections::BTreeSet, path::PathBuf, rc::Rc};

use nautilus_common::{
    actor::DataActor,
    messages::system::{SocketState, SocketStateChanged},
    timer::TimeEvent,
};
use nautilus_model::{
    accounts::Account,
    data::{Bar, BarType, CustomData, QuoteTick, TradeTick, bar_vwap::BarWithVwap},
    enums::{LiquiditySide, OmsType, OrderSide, OrderStatus, PositionSide, TimeInForce},
    events::{OrderEventAny, OrderFilled},
    identifiers::{ClientOrderId, InstrumentId, StrategyId},
    instruments::{Instrument, InstrumentAny},
    orders::Order,
    types::{Price, Quantity},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    analytics::{EquityPoint, PerformanceTracker, number},
    config::{GridConfig, RiskPolicy, StrategyMode, TrendPolicy},
    diagnostics::{CancelObservation, ObservationCount, RejectionObservation, ResetObservation},
    engine::{GridEngine, LevelStatus, spacing},
    multi_asset::MultiAssetGridStrategy,
    orders::{GridOrder, OrderManager, OrderPhase, PositionComponent},
    position::{PositionTarget, position_delta, target_position},
    regime::{MarketRegime, Observation, RegimeDetector, is_fresh},
    risk::{RiskManager, RiskSnapshot},
    stock::{BreakoutDirection, StockMarketState},
};
use crate::strategy::{Strategy, StrategyConfig, StrategyNative};

/// 历史回测、sandbox、模拟盘和实盘 runner 共用的运行配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicGridConfig {
    /// NautilusTrader 策略身份与订单所有权配置。
    pub base: StrategyConfig,
    /// 由本引擎独占管理的单个现货或股票标的。
    pub instrument_id: InstrumentId,
    /// 产生因果信号的已完成 K 线类型。
    pub bar_type: BarType,
    /// 网格经济参数与单标的风险配置。
    #[serde(default)]
    pub grid: GridConfig,
    /// 数据源发布已确认 `BarWithVwap` 而非普通最终 K 线时设为 true。
    #[serde(default)]
    pub confirmed_custom_bars: bool,
    /// 是否用最新已完成 K 线状态，在 Quote/Trade Tick 上触发执行判断。
    #[serde(default)]
    pub tick_execution: bool,
    /// 可选持久化检查点；随附的模拟盘与实盘 runner 必须配置。
    pub state_path: Option<PathBuf>,
    /// 由 runner 提供的账户/环境身份，防止跨账户误用检查点。
    #[serde(default)]
    pub recovery_context: Option<String>,
}

impl DynamicGridConfig {
    /// 使用明确标的和已完成 K 线类型创建保守默认配置。
    #[must_use]
    pub fn new(instrument_id: InstrumentId, bar_type: BarType) -> Self {
        Self {
            base: StrategyConfig {
                strategy_id: Some(StrategyId::from("DYNAMIC-GRID-901")),
                order_id_tag: Some("901".to_string()),
                oms_type: Some(OmsType::Netting),
                market_exit_reduce_only: false,
                ..Default::default()
            },
            instrument_id,
            bar_type,
            grid: GridConfig::default(),
            confirmed_custom_bars: false,
            tick_execution: false,
            state_path: None,
            recovery_context: None,
        }
    }
}

/// 显式策略状态机；状态迁移代替相互冲突的布尔开关。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyState {
    /// 运行时尚未提供账户或标的信息。
    Initializing,
    /// 指标未预热或市场状态不允许新增库存。
    WaitingForRange,
    /// 已越过网格边界，但连续收盘确认数量尚未满足。
    BreakoutPending,
    /// 股票时段、跳空或流动性条件禁止新增网格库存，但仍允许减仓卖出。
    Paused,
    /// 当前网格可以正常交易。
    GridActive,
    /// 正在撤销旧网格订单；全部终结前不得建立新网格。
    GridResetting,
    /// 风险减仓中，等待撤单确认后再执行有库存覆盖的卖出。
    RiskReducing,
    /// 已锁存风险失败，禁止新增买单。
    RiskOff,
    /// 交易前必须使券商、Nautilus 缓存与持久化状态一致。
    Recovering,
    /// 策略已停止，不允许产生新订单。
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct GridState {
    /// 当前策略状态机节点。
    pub(super) state: StrategyState,
    /// 当前有效网格；重置或等待状态下可以为空。
    pub(super) grid: Option<GridEngine>,
    /// 仅由已完成 K 线更新、可重放恢复的市场状态检测器。
    pub(super) regime: RegimeDetector,
    /// 订单、库存批次与周期盈亏的唯一策略账本。
    pub(super) orders: OrderManager,
    /// 单标的风险高水位、日损失和重置预算。
    pub(super) risk: RiskManager,
    /// 单调递增的网格代次，用于隔离旧网格订单。
    pub(super) generation: u64,
    /// 创建当前网格时的市场状态，不随每根 K 线漂移。
    pub(super) grid_regime: MarketRegime,
    /// 当前等待执行或最近完成的重置原因。
    pub(super) reset_reason: Option<String>,
    /// 最近可用市场价格。
    pub(super) last_price: Option<Decimal>,
    /// 最近市场事件时间戳，用于行情新鲜度判断。
    pub(super) last_market_ns: u64,
    #[serde(default)]
    /// 最近一次 Tick 驱动决策的事件时间戳，用于事件去重。
    pub(super) last_tick_event_ns: u64,
    /// 已经纳入风险连续重置判断的完成周期数量。
    pub(super) completed_cycles: usize,
    #[serde(default)]
    /// 当前核心仓、网格仓及合计目标仓位。
    pub(super) position_target: PositionTarget,
    #[serde(default)]
    /// 当前市场状态连续稳定的已完成 K 线数量。
    pub(super) regime_confirmation_bars: u32,
    #[serde(default)]
    /// 美股时段、跳空、流动性与突破确认状态。
    pub(super) stock: StockMarketState,
}

#[derive(Debug)]
pub(super) struct GridStrategyEngine {
    pub config: DynamicGridConfig,
    pub state: GridState,
    pub instrument: Option<InstrumentAny>,
    pub report: Rc<RefCell<PerformanceTracker>>,
}

impl GridStrategyEngine {
    pub(super) fn new(config: DynamicGridConfig) -> Self {
        Self {
            state: GridState {
                state: StrategyState::Initializing,
                grid: None,
                regime: RegimeDetector::default(),
                orders: OrderManager::new(config.grid.capital),
                risk: RiskManager::new(config.grid.capital),
                generation: 0,
                grid_regime: MarketRegime::Disabled,
                reset_reason: None,
                last_price: None,
                last_market_ns: 0,
                last_tick_event_ns: 0,
                completed_cycles: 0,
                position_target: PositionTarget::default(),
                regime_confirmation_bars: 0,
                stock: StockMarketState::default(),
            },
            config,
            instrument: None,
            report: Rc::new(RefCell::new(PerformanceTracker::default())),
        }
    }

    /// 仅在操作员完成订单与库存对账后，显式解除已锁存的风险状态。
    ///
    /// # Errors
    ///
    /// 仍有未终结订单、缺少市场价格，或当前暴露仍违反限制时返回错误。
    pub(super) fn reset_risk(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state.orders.active_ids().is_empty(),
            "Risk reset requires terminal orders"
        );
        self.recover(runtime)?;
        let mark = self
            .state
            .last_price
            .ok_or_else(|| anyhow::anyhow!("Risk reset requires a market price"))?;
        let snapshot = self.snapshot(runtime, mark)?;
        let now = runtime.clock().timestamp_ns().as_u64();
        let mut risk = RiskManager::new(snapshot.equity);
        risk.total_resets = self.state.risk.total_resets;
        risk.maximum_reset_count = self.state.risk.maximum_reset_count;
        risk.day = self.state.risk.day;
        risk.daily_resets = self.state.risk.daily_resets;
        risk.observe(&self.config.grid, &snapshot, now);
        anyhow::ensure!(
            risk.risk_off_reason.is_none(),
            "Current exposure still violates risk limits"
        );
        self.state.risk = risk;
        self.state.reset_reason = Some("RISK_RESET".to_string());
        self.state.state = StrategyState::GridResetting;
        runtime.persist_engine(self, None)
    }

    fn namespace(&self) -> String {
        format!(
            "{}-{}",
            self.config
                .base
                .order_id_tag
                .as_deref()
                .expect("Validated tag"),
            self.config.instrument_id
        )
    }
    pub(super) fn halt(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        reason: impl Into<anyhow::Error>,
    ) {
        let reason = reason.into().to_string();
        runtime.halt_portfolio(reason.clone());
        self.state.risk.trip(reason.clone());
        self.state.state = StrategyState::Recovering;
        log::error!(
            "RISK_OFF symbol={} reason={reason}",
            self.config.instrument_id
        );
        if let Err(e) = runtime.persist_engine(self, None) {
            log::error!("Checkpoint failed: {e}");
        }
        // 即使持久化失败也先执行撤单，以立即降低潜在风险暴露。
        for id in self.state.orders.active_ids() {
            if !self.state.orders.orders()[&id].buy {
                continue;
            }
            if matches!(
                self.state.orders.orders()[&id].phase,
                OrderPhase::CancelPending | OrderPhase::Unknown
            ) {
                continue;
            }
            if let Ok(client_id) = ClientOrderId::new_checked(&id)
                && runtime
                    .cache()
                    .order(&client_id)
                    .is_some_and(|o| o.venue_order_id().is_some())
            {
                match runtime.cancel_order(client_id, None, None) {
                    Ok(()) => self.record_cancel(
                        &id,
                        "RECOVERY_HALT",
                        runtime.clock().timestamp_ns().as_u64(),
                    ),
                    Err(e) => log::error!("Emergency cancellation failed: {e}"),
                }
            }
        }
    }

    pub(super) fn snapshot(
        &self,
        runtime: &MultiAssetGridStrategy,
        mark: Decimal,
    ) -> anyhow::Result<RiskSnapshot> {
        let instrument = self
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let currency = instrument.quote_currency();

        // 只读取账户当前值，避免复制持续增长的账户事件历史。
        let (account_id, free) = {
            let cache = runtime.strategy_core().cache_ref();
            let account = cache
                .account_for_venue(&self.config.instrument_id.venue)
                .ok_or_else(|| anyhow::anyhow!("Account unavailable"))?;
            let free = account
                .balance_free(Some(currency))
                .ok_or_else(|| anyhow::anyhow!("Quote-currency cash unavailable"))?
                .as_decimal();
            (account.id(), free)
        };
        let account_equity = runtime
            .portfolio()
            .equity(&self.config.instrument_id.venue, Some(&account_id))
            .get(&currency)
            .map(nautilus_model::types::Money::as_decimal)
            .ok_or_else(|| anyhow::anyhow!("Account equity unavailable"))?;
        let position = self.state.orders.inventory();
        let exposure = position * mark;
        let broker_quantity: Decimal = {
            let cache = runtime.strategy_core().cache_ref();
            let positions =
                cache.positions_open_refs(None, Some(&self.config.instrument_id), None, None, None);
            anyhow::ensure!(
                positions.iter().all(|p| p.side == PositionSide::Long
                    && Some(p.strategy_id) == self.config.base.strategy_id),
                "Unexpected broker position ownership"
            );
            positions.iter().map(|p| p.quantity.as_decimal()).sum()
        };
        anyhow::ensure!(
            broker_quantity == position,
            "Broker inventory changed outside the grid ledger"
        );
        let (pending_buy_quantity, pending_buy_notional) =
            self.state.orders.buy_reservations(&self.config.grid, mark);
        Ok(RiskSnapshot {
            equity: self.state.orders.cash + exposure,
            cash: self.state.orders.cash,
            account_equity,
            account_free: free,
            position,
            exposure,
            unrealized_pnl: exposure - self.state.orders.inventory_cost(),
            pending_buy_quantity,
            pending_buy_notional,
            active_orders: self.state.orders.active_ids().len(),
            atr_pct: Decimal::from_f64_retain(self.state.regime.snapshot.atr)
                .and_then(|atr| atr.checked_div(mark))
                .unwrap_or(Decimal::ZERO),
        })
    }

    fn update_position_target(
        &mut self,
        snapshot: &RiskSnapshot,
        price: Decimal,
        lot: Decimal,
        now: u64,
    ) -> anyhow::Result<PositionTarget> {
        let target = target_position(
            &self.config.grid,
            self.state.regime.snapshot.regime,
            snapshot.equity,
            price,
            lot,
        )?;
        if target != self.state.position_target {
            log::info!(
                "POSITION_TARGET_CHANGED timestamp_ns={now} symbol={} regime={:?} price={price} core={} grid={} total={} previous_total={}",
                self.config.instrument_id,
                self.state.regime.snapshot.regime,
                target.core,
                target.grid,
                target.total,
                self.state.position_target.total,
            );
            self.state.position_target = target;
        }
        Ok(target)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "explicit target, venue increments and current risk snapshot"
    )]
    fn seed_core_position(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        target: PositionTarget,
        snapshot: &RiskSnapshot,
        grid: &GridEngine,
        price: Decimal,
        tick: Decimal,
        lot: Decimal,
        now: u64,
    ) -> anyhow::Result<()> {
        let current = self
            .state
            .orders
            .component_inventory(PositionComponent::Core);
        let (pending_buys, pending_sells) = self
            .state
            .orders
            .component_reservations(PositionComponent::Core);
        let desired = position_delta(target.core, current, pending_buys, pending_sells, lot);
        if desired <= Decimal::ZERO {
            return Ok(());
        }
        let desired =
            self.state
                .risk
                .buy_quantity(&self.config.grid, snapshot, desired, price, lot);
        let desired = runtime.buy_quantity(self, desired, price, None, false)?;
        if desired <= Decimal::ZERO {
            return Ok(());
        }
        // Core 复用同一订单账本，但使用独立 component，普通网格止盈不会出售它。
        let level = super::engine::GridLevel {
            level_index: 0,
            price,
            side: OrderSide::Buy,
            exit_price: grid.upper_bound.max(price + tick),
            quantity: desired,
            status: LevelStatus::Pending,
            entry_order_id: None,
            exit_order_id: None,
        };
        let intent = self.state.orders.entry_component(
            &self.namespace(),
            grid.grid_id,
            &level,
            desired,
            Some(price),
            PositionComponent::Core,
            now,
        )?;
        self.submit(runtime, &intent)
    }

    /// 将真实成交库存降至更低的市场状态目标，绝不卖出尚未成交的数量。
    fn reduce_component_to(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        component: PositionComponent,
        target: Decimal,
        price: Decimal,
        lot_size: Decimal,
        now: u64,
    ) -> anyhow::Result<bool> {
        let current = self.state.orders.component_inventory(component);
        let (pending_buys, pending_sells) = self.state.orders.component_reservations(component);
        if position_delta(target, current, pending_buys, pending_sells, lot_size) >= Decimal::ZERO {
            return Ok(false);
        }

        // 先撤掉同仓位分层的买单与远端限价卖单。CancelPending 仍占预留，终态确认前不重挂。
        let conflicts: Vec<_> = self
            .state
            .orders
            .active_ids()
            .into_iter()
            .filter(|id| {
                let order = &self.state.orders.orders()[id];
                order.component == component && (order.buy || order.limit.is_some())
            })
            .collect();
        if !conflicts.is_empty() {
            self.cancel_ids(runtime, conflicts, "POSITION_TARGET_REDUCTION")?;
            return Ok(true);
        }

        let (pending_buys, pending_sells) = self.state.orders.component_reservations(component);
        let mut remaining = -position_delta(
            target,
            self.state.orders.component_inventory(component),
            pending_buys,
            pending_sells,
            lot_size,
        );
        if remaining <= Decimal::ZERO {
            return Ok(false);
        }
        let namespace = self.namespace();
        for lot_id in self.state.orders.reduction_candidates(component) {
            if remaining <= Decimal::ZERO
                || self.state.orders.active_count() >= self.config.grid.max_orders
            {
                break;
            }
            let intent = self.state.orders.exit_quantity(
                &namespace,
                &lot_id,
                Some(price),
                remaining,
                now,
            )?;
            let id = intent.id.clone();
            self.submit(runtime, &intent)?;
            remaining -= self
                .state
                .orders
                .orders()
                .get(&id)
                .map_or(Decimal::ZERO, |order| order.quantity);
        }
        log::info!(
            "POSITION_TARGET_REDUCING timestamp_ns={now} symbol={} component={component:?} target={target} remaining={remaining}",
            self.config.instrument_id,
        );
        Ok(true)
    }

    fn reduce_to_target(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        target: PositionTarget,
        price: Decimal,
        lot_size: Decimal,
        now: u64,
    ) -> anyhow::Result<bool> {
        // 降低目标仓位时先卖战术网格仓，最后才触及长期核心仓。
        let grid = self.reduce_component_to(
            runtime,
            PositionComponent::Grid,
            target.grid,
            price,
            lot_size,
            now,
        )?;
        let core = self.reduce_component_to(
            runtime,
            PositionComponent::Core,
            target.core,
            price,
            lot_size,
            now,
        )?;
        Ok(grid || core)
    }

    pub(super) fn recover(&mut self, runtime: &MultiAssetGridStrategy) -> anyhow::Result<()> {
        // 检查点只提供归属线索，必须与原生订单、成交、持仓逐项对账后才能恢复买入
        self.state.state = StrategyState::Recovering;
        self.state.regime.validate(&self.config.grid)?;
        let now = runtime.clock().timestamp_ns().as_u64();
        anyhow::ensure!(
            self.state.regime.snapshot.ts_ns <= now
                && self.state.last_market_ns <= now
                && self.state.last_tick_event_ns <= now
                && self.state.grid.as_ref().is_none_or(|g| g.created_ns <= now),
            "Recovered market state contains future timestamps"
        );
        if let Some(grid) = &self.state.grid {
            let instrument = self
                .instrument
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Instrument unavailable for grid recovery"))?;
            let lot = instrument
                .lot_size()
                .unwrap_or(instrument.size_increment())
                .as_decimal()
                .max(instrument.size_increment().as_decimal());
            let expected = GridEngine::build(
                &self.config.grid,
                grid.grid_id,
                grid.center,
                grid.spacing,
                Decimal::ZERO,
                instrument.price_increment().as_decimal(),
                lot,
                grid.created_ns,
            )?;
            anyhow::ensure!(
                grid.lower_bound == expected.lower_bound
                    && grid.upper_bound == expected.upper_bound
                    && grid.levels.len() == expected.levels.len()
                    && grid
                        .levels
                        .iter()
                        .zip(&expected.levels)
                        .all(|(actual, expected)| {
                            actual.level_index == expected.level_index
                                && actual.price == expected.price
                                && actual.exit_price == expected.exit_price
                                && actual.side == expected.side
                                && actual.quantity >= Decimal::ZERO
                                && actual.quantity % lot == Decimal::ZERO
                        }),
                "Recovered grid geometry differs from configuration/native instrument"
            );
        }
        let orders =
            runtime
                .cache()
                .orders(None, Some(&self.config.instrument_id), None, None, None);
        let mut found = BTreeSet::new();
        for order in orders {
            let id = order.client_order_id().to_string();
            if !self.state.orders.orders().contains_key(&id) {
                anyhow::ensure!(
                    order.is_closed(),
                    "Unowned broker order blocks recovery: {id}"
                );
                continue;
            }
            anyhow::ensure!(
                order.strategy_id()
                    == self
                        .config
                        .base
                        .strategy_id
                        .expect("Validated strategy identity"),
                "Recovered order ownership mismatch"
            );
            found.insert(id.clone());
            for event in order.events() {
                if let OrderEventAny::Filled(fill) = event {
                    self.apply_fill(fill)?;
                }
            }
            anyhow::ensure!(
                order.filled_qty().as_decimal() == self.state.orders.orders()[&id].filled,
                "Fill history mismatch for {id}"
            );
            let phase = phase(order.status());
            let now = runtime.clock().timestamp_ns().as_u64();
            self.state.orders.transition(&id, phase, now);
        }
        for id in self.state.orders.active_ids() {
            anyhow::ensure!(
                found.contains(&id),
                "Unresolved durable intent missing from broker/cache: {id}"
            );
            anyhow::ensure!(
                !matches!(
                    self.state.orders.orders()[&id].phase,
                    OrderPhase::Intent | OrderPhase::Submitted | OrderPhase::Unknown
                ),
                "Order outcome remains unknown: {id}"
            );
        }
        let positions = runtime.cache().positions_open(
            None,
            Some(&self.config.instrument_id),
            None,
            None,
            None,
        );
        anyhow::ensure!(
            positions.iter().all(|p| p.side == PositionSide::Long
                && p.strategy_id
                    == self
                        .config
                        .base
                        .strategy_id
                        .expect("Validated strategy identity")),
            "Unowned or short broker inventory blocks recovery"
        );
        let quantity: Decimal = positions.iter().map(|p| p.quantity.as_decimal()).sum();
        anyhow::ensure!(
            quantity == self.state.orders.inventory(),
            "Broker inventory {quantity} differs from grid inventory {}",
            self.state.orders.inventory()
        );
        self.state.state = if self.state.risk.risk_off_reason.is_some() {
            StrategyState::RiskOff
        } else if self.state.reset_reason.is_some() {
            StrategyState::GridResetting
        } else if self.state.grid.is_some() {
            StrategyState::GridActive
        } else {
            StrategyState::WaitingForRange
        };
        log::info!(
            "STATE_RECOVERED symbol={} inventory={quantity} state={:?}",
            self.config.instrument_id,
            self.state.state
        );
        runtime.persist_engine(self, None)
    }

    pub(super) fn submit(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        intent: &GridOrder,
    ) -> anyhow::Result<()> {
        let mut intent = intent.clone();

        // 生成意图后再次经过共享账户准入，其他股票的待成交买单也占用同一笔现金
        let quantity = runtime.gate(self, &intent)?;
        self.state.orders.resize_intent(&intent.id, quantity)?;
        if quantity <= Decimal::ZERO {
            ObservationCount::record(
                &mut self.report.borrow_mut().diagnostics.zero_admissions,
                "FINAL_ORDER_GATE",
                runtime.clock().timestamp_ns().as_u64(),
            );
            // 原生订单尚未创建：延后处理，并删除没有成交的空库存批次。
            if let Some(grid) = &mut self.state.grid {
                for level in &mut grid.levels {
                    if level.entry_order_id.as_deref() == Some(&intent.id) {
                        level.entry_order_id = None;
                        level.status = LevelStatus::Pending;
                    }
                }
            }
            return runtime.persist_engine(self, None);
        }
        intent.quantity = quantity;
        let instrument = self
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let quantity = Quantity::from_decimal_dp(intent.quantity, instrument.size_precision())?;
        anyhow::ensure!(
            instrument.min_quantity().is_none_or(|q| quantity >= q)
                && instrument.max_quantity().is_none_or(|q| quantity <= q),
            "Order quantity outside instrument limits"
        );
        anyhow::ensure!(
            instrument
                .min_notional()
                .is_none_or(|n| intent.quantity * intent.reference >= n.as_decimal())
                && instrument
                    .max_notional()
                    .is_none_or(|n| intent.quantity * intent.reference <= n.as_decimal()),
            "Order notional outside instrument limits"
        );
        let id = ClientOrderId::new_checked(&intent.id)?;
        anyhow::ensure!(
            runtime.cache().order(&id).is_none(),
            "Duplicate order identity {id}"
        );
        let side = if intent.buy {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };
        if let Some(grid) = &mut self.state.grid
            && grid.grid_id == intent.grid_id
            && let Some(level) = grid
                .levels
                .iter_mut()
                .find(|l| l.level_index == intent.level)
        {
            if intent.buy {
                level.entry_order_id = Some(intent.id.clone());
            } else {
                level.exit_order_id = Some(intent.id.clone());
            }
            level.status = LevelStatus::Active;
        }
        let order = if let Some(price) = intent.limit {
            runtime.order().limit(
                self.config.instrument_id,
                side,
                quantity,
                Price::from_decimal_dp(price, instrument.price_precision())?,
                Some(TimeInForce::Gtc),
                None,
                Some(false),
                Some(false),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(id),
            )
        } else {
            runtime.order().market(
                self.config.instrument_id,
                side,
                quantity,
                Some(TimeInForce::Day),
                Some(false),
                None,
                None,
                None,
                None,
                Some(id),
            )
        };
        // 先持久化稳定订单身份再发单；进程崩溃后对账，不能把未知结果当作未下单
        runtime.persist_engine(self, Some(&order))?;
        runtime.submit_order(order, None, None, None)?;
        log::info!(
            "ORDER_SUBMITTED symbol={} grid_id={} level={} order_id={id} side={side} quantity={quantity} price={}",
            self.config.instrument_id,
            intent.grid_id,
            intent.level,
            intent.reference
        );
        Ok(())
    }

    pub(super) fn cancel(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        buys_only: bool,
        reason: &str,
    ) -> anyhow::Result<()> {
        let ids = self
            .state
            .orders
            .active_ids()
            .into_iter()
            .filter(|id| !buys_only || self.state.orders.orders()[id].buy)
            .collect();
        self.cancel_ids(runtime, ids, reason)
    }

    pub(super) fn cancel_ids(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        ids: Vec<String>,
        reason: &str,
    ) -> anyhow::Result<()> {
        for id in ids {
            let order = &self.state.orders.orders()[&id];
            if matches!(order.phase, OrderPhase::CancelPending | OrderPhase::Unknown) {
                continue;
            }
            if self.state.state == StrategyState::RiskReducing
                && !order.buy
                && order.limit.is_none()
            {
                continue;
            }
            let client_id = ClientOrderId::new_checked(&id)?;
            let Some(cached) = runtime.cache().order(&client_id) else {
                continue;
            };
            if cached.venue_order_id().is_none() {
                continue;
            }
            let now = runtime.clock().timestamp_ns().as_u64();
            self.state
                .orders
                .transition(&id, OrderPhase::CancelPending, now);
            runtime.persist_engine(self, None)?;
            runtime.cancel_order(client_id, None, None)?;
            self.record_cancel(&id, reason, now);
        }
        Ok(())
    }

    fn record_cancel(&self, id: &str, reason: &str, now: u64) {
        let order = &self.state.orders.orders()[id];
        // 只对同代网格计算距首个负层的距离；旧库存退出单不能套用新网格的中心。
        let first_buy_level = self
            .state
            .grid
            .as_ref()
            .filter(|g| g.grid_id == order.grid_id)
            .and_then(|g| {
                g.levels
                    .iter()
                    .filter(|l| l.level_index < 0)
                    .map(|l| l.price)
                    .max()
            });
        let first_buy_distance_pct = self
            .state
            .last_price
            .zip(first_buy_level)
            .and_then(|(price, level)| price.checked_sub(level)?.checked_div(level));
        self.report
            .borrow_mut()
            .diagnostics
            .cancellations
            .push(CancelObservation {
                ts_ns: now,
                order_id: id.to_string(),
                grid_id: order.grid_id,
                level: order.level,
                buy: order.buy,
                reason: reason.to_string(),
                filled: order.filled,
                price: self.state.last_price,
                mark_ns: self.state.last_market_ns,
                first_buy_level,
                first_buy_distance_pct,
            });
    }

    pub(super) fn exits(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        now: u64,
        flatten: bool,
    ) -> anyhow::Result<()> {
        let namespace = self.namespace();
        // 只遍历尚有未预留库存的批次；完整历史仍保留在 OrderManager 中审计。
        let lots = self.state.orders.exit_candidates();
        for id in lots {
            if self.state.orders.active_count() >= self.config.grid.max_orders {
                break;
            }
            let lot = &self.state.orders.lots()[&id];
            if !flatten
                && self.state.risk.risk_off_reason.is_none()
                && !runtime.portfolio_blocked()
                && !self
                    .state
                    .regime
                    .permits_order(&self.config.grid, false, lot.level)
            {
                continue;
            }
            let reference = if flatten { self.state.last_price } else { None };
            let order = self.state.orders.exit(&namespace, &id, reference, now)?;
            self.submit(runtime, &order)?;
        }
        Ok(())
    }

    pub(super) fn drive(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        price: Decimal,
        now: u64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(price > Decimal::ZERO, "Invalid market price");
        if now < self.state.last_market_ns {
            return Ok(());
        }
        // 穿越计算用于事件观测，真实买入由已挂限价单的成交回报确认，不在这里虚构成交
        if let (Some(previous), Some(grid)) = (self.state.last_price, &self.state.grid) {
            for level in grid.crossed(previous, price) {
                log::debug!(
                    "GRID_LEVEL_CROSSED timestamp_ns={now} symbol={} grid_id={} level={level} price={price}",
                    self.config.instrument_id,
                    grid.grid_id
                );
            }
        }
        self.state.last_price = Some(price);
        self.state.last_market_ns = now;
        if matches!(
            self.state.state,
            StrategyState::Initializing | StrategyState::Recovering | StrategyState::Stopped
        ) {
            return Ok(());
        }
        let config = self.config.grid.clone();
        let snapshot = self.snapshot(runtime, price)?;
        self.report.borrow_mut().observe_mark(
            config.capital,
            snapshot.equity,
            snapshot.exposure,
            snapshot.position,
            now,
        );
        let prior_peak = self.state.risk.peak_equity;
        let prior_day = self.state.risk.day;
        self.state.risk.observe(&config, &snapshot, now);
        if prior_peak != self.state.risk.peak_equity || prior_day != self.state.risk.day {
            runtime.persist_engine(self, None)?;
        }
        if self.state.orders.timed_out(config.order_timeout_secs, now) {
            self.state
                .risk
                .trip("Unknown order outcome or cancellation timeout");
        }
        if self.state.risk.risk_off_reason.is_some() || runtime.portfolio_flatten() {
            self.state.state = StrategyState::RiskOff;
            if config.risk_policy == RiskPolicy::Flatten || runtime.portfolio_flatten() {
                self.state.state = StrategyState::RiskReducing;
                self.cancel(runtime, false, "RISK_FLATTEN")?;
                if self.state.orders.active_ids().is_empty() {
                    self.exits(runtime, now, true)?;
                }
            } else {
                self.cancel(runtime, true, "INSTRUMENT_RISK_OFF")?;
                self.exits(runtime, now, false)?;
            }
            return runtime.persist_engine(self, None);
        }
        if runtime.portfolio_blocked() {
            self.cancel(runtime, true, "PORTFOLIO_BLOCKED")?;
            self.exits(runtime, now, false)?;
            return Ok(());
        }
        let disallowed = self
            .state
            .orders
            .active_ids()
            .into_iter()
            .filter(|id| {
                let order = &self.state.orders.orders()[id];
                !self
                    .state
                    .regime
                    .permits_order(&config, order.buy, order.level)
            })
            .collect();
        self.cancel_ids(runtime, disallowed, "REGIME_ORDER_POLICY")?;
        let signal = self.state.regime.snapshot.clone();
        let stale = !signal.initialized || !is_fresh(signal.ts_ns, now, config.max_signal_age_secs);
        let regime = signal.regime;
        let policy = self.state.regime.policy(&config);
        let instrument = self
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let tick = instrument.price_increment().as_decimal();
        let lot = instrument
            .lot_size()
            .unwrap_or(instrument.size_increment())
            .as_decimal()
            .max(instrument.size_increment().as_decimal());

        // Target Position 是股票适配层的唯一仓位目标；先减仓，再讨论新网格和新买单。
        if config.strategy_mode == StrategyMode::StockAdaptive && !stale {
            let target = self.update_position_target(&snapshot, price, lot, now)?;
            if self.reduce_to_target(runtime, target, price, lot, now)? {
                self.state.state = StrategyState::Paused;
                return runtime.persist_engine(self, None);
            }
            if let Some(gate) = self.state.stock.gate(&config, now, price) {
                self.cancel(runtime, true, gate.reason())?;
                self.exits(runtime, now, false)?;
                self.state.state = StrategyState::Paused;
                log::info!(
                    "GRID_PAUSED timestamp_ns={now} symbol={} reason={}",
                    self.config.instrument_id,
                    gate.reason()
                );
                return Ok(());
            }
        }
        // 这是行情准入而非最终下单许可，单标的风控、组合风控和原生 RiskEngine 仍可否决
        let regime_confirmed = config.strategy_mode != StrategyMode::StockAdaptive
            || self.state.regime_confirmation_bars >= config.regime_confirmation_bars;
        let can_buy = !stale
            && regime_confirmed
            && !matches!(
                regime,
                MarketRegime::Disabled | MarketRegime::HighVolatility
            )
            && policy != TrendPolicy::Disable;
        let multiplier = if policy == TrendPolicy::WiderGrid {
            config.trend_spacing_multiplier
        } else {
            Decimal::ONE
        };
        let atr =
            Decimal::from_f64_retain(signal.atr).ok_or_else(|| anyhow::anyhow!("Nonfinite ATR"))?;
        let spacing = spacing(&config, atr, price, multiplier)?;
        if let Some(grid) = &self.state.grid {
            let raw_breakout_up = price > grid.upper_bound;
            let raw_breakout_down = price < grid.lower_bound;
            let (breakout_up, breakout_down, breakout_pending) =
                if config.strategy_mode == StrategyMode::StockAdaptive {
                    match self.state.stock.breakout() {
                        Some((BreakoutDirection::Up, confirmed)) => {
                            (confirmed, false, raw_breakout_up && !confirmed)
                        }
                        Some((BreakoutDirection::Down, confirmed)) => {
                            (false, confirmed, raw_breakout_down && !confirmed)
                        }
                        None => (false, false, raw_breakout_up || raw_breakout_down),
                    }
                } else {
                    (raw_breakout_up, raw_breakout_down, false)
                };

            // 突破确认期间停止新增库存；已有覆盖卖单继续工作，避免假突破反复 reset。
            if breakout_pending {
                self.cancel(runtime, true, "BREAKOUT_PENDING")?;
                self.exits(runtime, now, false)?;
                self.state.state = StrategyState::BreakoutPending;
                return Ok(());
            }
            if config.strategy_mode == StrategyMode::StockAdaptive
                && breakout_down
                && regime == MarketRegime::TrendDown
            {
                self.cancel(runtime, true, "DOWNTREND_BREAKOUT")?;
                self.exits(runtime, now, false)?;
                self.state.state = StrategyState::Paused;
                log::warn!(
                    "GRID_PAUSED timestamp_ns={now} symbol={} reason=DOWNTREND_BREAKOUT price={price}",
                    self.config.instrument_id
                );
                return Ok(());
            }
            let volatility_changed =
                (spacing - grid.spacing).abs() / grid.spacing >= config.volatility_reset_ratio;
            let regime_changed = regime != self.state.grid_regime;
            if !config.enable_dynamic_reset && (breakout_up || breakout_down) {
                self.state.risk.trip("Fixed grid boundary reached");
                return self.drive(runtime, price, now);
            }
            if self.state.reset_reason.is_none()
                && config.enable_dynamic_reset
                && !stale
                && grid.can_reset(&config, price, atr, now)
                && (breakout_up
                    || breakout_down
                    || (can_buy && (volatility_changed || regime_changed)))
            {
                if let Some(reason) = self.state.risk.reset_limit(&config) {
                    self.state.risk.trip(reason);
                    return self.drive(runtime, price, now);
                }
                self.state.reset_reason = Some(
                    if breakout_up {
                        "GRID_RESET_UP"
                    } else if breakout_down {
                        "GRID_RESET_DOWN"
                    } else if regime_changed {
                        "REGIME_CHANGED"
                    } else {
                        "VOLATILITY_RESET"
                    }
                    .to_string(),
                );
                self.state.state = StrategyState::GridResetting;
                runtime.persist_engine(self, None)?;
            }
        }
        if self.state.reset_reason.is_some() {
            // 撤单请求不等于撤单完成，旧订单终态全部确认前不能创建新一代网格
            let reason = self.state.reset_reason.clone().unwrap_or_default();
            self.cancel(runtime, false, &reason)?;
            if !self.state.orders.active_ids().is_empty() || stale {
                return Ok(());
            }
            // 撤单期间保留已确认的重置事件。新锚点必须使用新鲜价格，但价格回撤不能抹掉
            // 先前已经确认的边界突破。
            if let Some(reason) = self.state.risk.reset_limit(&config) {
                self.state.risk.trip(reason);
                return self.drive(runtime, price, now);
            }
            log::info!(
                "{} timestamp_ns={now} symbol={} grid_id={} center={price}",
                self.state.reset_reason.as_deref().unwrap_or("GRID_RESET"),
                self.config.instrument_id,
                self.state.generation
            );
            self.state.risk.record_reset();
            let entries: Vec<_> = self
                .state
                .orders
                .orders()
                .values()
                .filter(|o| o.grid_id == self.state.generation && o.buy && o.filled > Decimal::ZERO)
                .collect();
            self.report
                .borrow_mut()
                .diagnostics
                .resets
                .push(ResetObservation {
                    ts_ns: now,
                    grid_id: self.state.generation,
                    reason,
                    price,
                    entry_orders_with_fills: entries.len(),
                    lower_entry_orders_with_fills: entries.iter().filter(|o| o.level < 0).count(),
                });
            self.state.grid = None;
            self.state.reset_reason = None;
        }
        if !can_buy {
            self.cancel(runtime, true, "ENTRY_FILTER")?;
            self.exits(runtime, now, false)?;
            self.state.state = if config.strategy_mode == StrategyMode::StockAdaptive
                && matches!(
                    regime,
                    MarketRegime::TrendDown | MarketRegime::HighVolatility
                ) {
                StrategyState::Paused
            } else {
                StrategyState::WaitingForRange
            };
            return Ok(());
        }
        if self.state.grid.is_none() {
            // 只在首次激活或完成 reset 后锚定当前价，不随每根 K 线追着价格移动
            self.state.generation = self
                .state
                .generation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Grid identity exhausted"))?;
            let capital = (snapshot.equity * config.capital_allocation)
                .min(snapshot.cash)
                .max(Decimal::ZERO);
            let capital = if config.strategy_mode == StrategyMode::StockAdaptive {
                capital * config.grid_max_pct
            } else {
                capital
            };
            let grid = GridEngine::build(
                &config,
                self.state.generation,
                price,
                spacing,
                capital,
                tick,
                lot,
                now,
            )?;
            self.report
                .borrow_mut()
                .diagnostics
                .grids
                .push(grid.clone());
            self.state.grid = Some(grid);
            self.state.grid_regime = regime;
            self.state.stock.reset_breakout(self.state.generation);
            log::info!(
                "GRID_CREATED timestamp_ns={now} symbol={} grid_id={} center={price} spacing={spacing}",
                self.config.instrument_id,
                self.state.generation
            );
            runtime.persist_engine(self, None)?;
        }
        self.state.state = StrategyState::GridActive;
        let grid = self.state.grid.as_ref().expect("Grid created").clone();
        if config.strategy_mode == StrategyMode::StockAdaptive {
            let target = self.update_position_target(&snapshot, price, lot, now)?;
            self.seed_core_position(runtime, target, &snapshot, &grid, price, tick, lot, now)?;
        }
        self.exits(runtime, now, false)?;
        let namespace = self.namespace();
        for level in grid.levels {
            if self.state.orders.slot_busy(grid.grid_id, level.level_index)
                || level.quantity <= Decimal::ZERO
            {
                continue;
            }
            if !self
                .state
                .regime
                .permits_order(&config, true, level.level_index)
            {
                continue;
            }
            // 正层首次通过市价买入准备卖出库存，比例来自 initial_inventory_fraction
            // 同代同层已有买单历史就不再 seed，库存清空后仅按该层限价重新参与。
            let seeded =
                level.level_index > 0
                    && !self.state.orders.orders().values().any(|o| {
                        o.buy && o.grid_id == grid.grid_id && o.level == level.level_index
                    });
            if !seeded && level.price >= price {
                continue;
            }
            let reference = if seeded { price } else { level.price };

            // 初始库存用当前市价而非上方层价计算成本，预期价差须覆盖双边成本和利润下限
            if level.exit_price / reference - Decimal::ONE < config.cost_floor() {
                continue;
            }
            let snapshot = self.snapshot(runtime, price)?;
            let desired = if config.strategy_mode == StrategyMode::StockAdaptive {
                let current = self
                    .state
                    .orders
                    .component_inventory(PositionComponent::Grid);
                let (pending_buys, pending_sells) = self
                    .state
                    .orders
                    .component_reservations(PositionComponent::Grid);
                level.quantity.min(
                    position_delta(
                        self.state.position_target.grid,
                        current,
                        pending_buys,
                        pending_sells,
                        lot,
                    )
                    .max(Decimal::ZERO),
                )
            } else {
                level.quantity
            };
            let quantity = self.state.risk.buy_quantity(
                &config,
                &snapshot,
                desired,
                price.max(reference),
                lot,
            );
            if quantity <= Decimal::ZERO {
                ObservationCount::record(
                    &mut self.report.borrow_mut().diagnostics.zero_admissions,
                    "INSTRUMENT_SIZING",
                    now,
                );
                continue;
            }
            let quantity = runtime.buy_quantity(
                self,
                quantity,
                reference,
                if seeded { None } else { Some(level.price) },
                false,
            )?;
            if quantity <= Decimal::ZERO {
                ObservationCount::record(
                    &mut self.report.borrow_mut().diagnostics.zero_admissions,
                    "PORTFOLIO_SIZING",
                    now,
                );
                continue;
            }
            let intent = self.state.orders.entry(
                &namespace,
                grid.grid_id,
                &level,
                quantity,
                seeded.then_some(price),
                now,
            )?;
            if let Some(current) = &mut self.state.grid
                && let Some(slot) = current
                    .levels
                    .iter_mut()
                    .find(|l| l.level_index == level.level_index)
            {
                slot.entry_order_id = Some(intent.id.clone());
                slot.status = LevelStatus::Active;
            }
            self.submit(runtime, &intent)?;
        }
        Ok(())
    }

    pub(super) fn apply_fill(&mut self, fill: &OrderFilled) -> anyhow::Result<()> {
        anyhow::ensure!(
            fill.instrument_id == self.config.instrument_id,
            "Unexpected instrument fill"
        );
        let order = self
            .state
            .orders
            .orders()
            .get(fill.client_order_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("Unknown fill identity"))?;
        anyhow::ensure!(
            order.buy == (fill.order_side == OrderSide::Buy),
            "Fill side differs from intent"
        );
        let quantity = fill.last_qty.as_decimal();
        let price = fill.last_px.as_decimal();
        let estimated =
            fill.liquidity_side == LiquiditySide::NoLiquiditySide || fill.commission.is_none();
        let fee = if estimated {
            quantity
                * price
                * (self.config.grid.maker_fee.max(self.config.grid.taker_fee)
                    + self.config.grid.commission)
        } else {
            let commission = fill.commission.expect("Reported commission");
            anyhow::ensure!(
                commission.currency == fill.currency,
                "Fee currency requires conversion before trading"
            );
            commission.as_decimal()
        };
        let applied = self.state.orders.fill(
            fill.client_order_id.as_str(),
            fill.trade_id.as_str(),
            quantity,
            price,
            fee,
            estimated,
            fill.ts_event.as_u64(),
        )?;
        if applied {
            let order = &self.state.orders.orders()[fill.client_order_id.as_str()];
            let event = if order.phase == OrderPhase::Filled {
                "ORDER_FILLED"
            } else {
                "ORDER_PARTIALLY_FILLED"
            };
            log::info!(
                "{event} timestamp_ns={} symbol={} grid_id={} level={} order_id={} quantity={quantity} price={price} fee={fee} estimated={estimated}",
                fill.ts_event,
                self.config.instrument_id,
                order.grid_id,
                order.level,
                fill.client_order_id
            );
            let cycles = &self.state.orders.cycles;
            if cycles[self.state.completed_cycles..]
                .iter()
                .any(|c| c.net_pnl > Decimal::ZERO)
            {
                self.state.risk.consecutive_resets = 0;
            }
            self.state.completed_cycles = cycles.len();
            if let Some(grid) = &mut self.state.grid
                && let Some(level) = grid.levels.iter_mut().find(|l| {
                    l.entry_order_id.as_deref() == Some(fill.client_order_id.as_str())
                        || l.exit_order_id.as_deref() == Some(fill.client_order_id.as_str())
                })
            {
                level.status = if self.state.orders.lots()
                    [&self.state.orders.orders()[fill.client_order_id.as_str()].lot_id]
                    .recorded
                {
                    LevelStatus::Completed
                } else {
                    LevelStatus::Filled
                };
            }
            let mut report = self.report.borrow_mut();
            report.metrics.maximum_position = report
                .metrics
                .maximum_position
                .max(self.state.orders.inventory());
            report.metrics.maximum_exposure = report
                .metrics
                .maximum_exposure
                .max(self.state.orders.inventory() * price);
        }
        Ok(())
    }

    pub(super) fn record_equity(&self, now: u64) {
        let Some(price) = self.state.last_price else {
            return;
        };
        let position = self.state.orders.inventory();
        let exposure = position * price;
        let equity = self.state.orders.cash + exposure;
        let (_, pending) = self.state.orders.buy_reservations(&self.config.grid, price);
        let point = EquityPoint {
            trend_inventory: None,
            cumulative_fees: self.state.orders.fees,
            cumulative_turnover: self.state.orders.turnover,
            ts_ns: now,
            price,
            equity,
            exposure,
            position,
            utilization: if equity > Decimal::ZERO {
                number((exposure + pending) / equity)
            } else {
                0.0
            },
            regime: self.state.regime.snapshot.regime,
        };
        let mut report = self.report.borrow_mut();
        if report.equity.last().is_some_and(|p| p.ts_ns == now) {
            report.equity.pop();
        }
        if report.equity.last().is_none_or(|p| p.ts_ns <= now) {
            report.equity.push(point);
        }
    }

    pub(super) fn completed_bar(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        bar: &Bar,
    ) -> anyhow::Result<()> {
        if bar.bar_type != self.config.bar_type {
            return Ok(());
        }
        let now = runtime.clock().timestamp_ns().as_u64();
        anyhow::ensure!(
            bar.ts_event.as_u64() <= now,
            "Future bar cannot drive current decisions"
        );
        if bar.ts_event.as_u64() <= self.state.regime.snapshot.ts_ns {
            return Ok(());
        }
        let stock_adaptive = self.config.grid.strategy_mode == StrategyMode::StockAdaptive;
        if stock_adaptive
            && self.config.grid.regular_session_only
            && !StockMarketState::is_regular_session_bar(bar.ts_event.as_u64())
        {
            self.cancel(runtime, true, "OUTSIDE_REGULAR_SESSION")?;
            ObservationCount::record(
                &mut self.report.borrow_mut().diagnostics.blocked_bars,
                "OUTSIDE_REGULAR_SESSION",
                now,
            );
            return runtime.persist_engine(self, None);
        }
        let prior_atr =
            Decimal::from_f64_retain(self.state.regime.snapshot.atr).unwrap_or(Decimal::ZERO);
        let (stock_bar, opening_gap) = if stock_adaptive {
            self.state.stock.observe_bar(
                &self.config.grid,
                bar.ts_event.as_u64(),
                bar.open.as_decimal(),
                bar.close.as_decimal(),
                bar.volume.as_decimal(),
                prior_atr,
            )
        } else {
            (false, None)
        };
        if let Some(change) = opening_gap {
            let pnl = self.state.orders.inventory() * change;
            let mut report = self.report.borrow_mut();
            report.metrics.gap_pnl += pnl;
            report.metrics.gap_loss += (-pnl).max(Decimal::ZERO);
        }
        let previous = self.state.regime.snapshot.regime;
        // 只消费已结束且时间递增的信号 K 线，Tick 模式同样不使用未收盘指标
        self.state.regime.update(
            &self.config.grid,
            Observation {
                ts_ns: bar.ts_event.as_u64(),
                high: bar.high.as_f64(),
                low: bar.low.as_f64(),
                close: bar.close.as_f64(),
            },
        )?;
        runtime.observe_close(
            self.config.instrument_id,
            bar.ts_event.as_u64(),
            bar.close.as_decimal(),
        );
        if previous == self.state.regime.snapshot.regime {
            self.state.regime_confirmation_bars =
                self.state.regime_confirmation_bars.saturating_add(1);
        } else {
            self.state.regime_confirmation_bars = 1;
            log::info!(
                "REGIME_CHANGED timestamp_ns={now} symbol={} from={previous:?} to={:?}",
                self.config.instrument_id,
                self.state.regime.snapshot.regime
            );
        }
        if stock_bar && let Some(grid) = &self.state.grid {
            let atr =
                Decimal::from_f64_retain(self.state.regime.snapshot.atr).unwrap_or(Decimal::ZERO);
            self.state.stock.observe_breakout(
                &self.config.grid,
                grid.grid_id,
                bar.close.as_decimal(),
                grid.lower_bound,
                grid.upper_bound,
                atr,
            );
        }
        let stock_gate = stock_adaptive
            .then(|| {
                self.state
                    .stock
                    .gate(&self.config.grid, now, bar.close.as_decimal())
            })
            .flatten();
        let drive_result = if !self.config.tick_execution {
            self.drive(runtime, bar.close.as_decimal(), now)
        } else if self.state.last_price.is_none() {
            self.state.last_price = Some(bar.close.as_decimal());
            Ok(())
        } else {
            Ok(())
        };
        if stock_bar {
            self.state.stock.finish_bar();
        }
        drive_result?;
        self.record_equity(now);
        let blocked = if let Some(reason) = &self.state.risk.risk_off_reason {
            Some(format!("INSTRUMENT_RISK: {reason}"))
        } else if runtime.portfolio_blocked() {
            Some("PORTFOLIO_BLOCKED".to_string())
        } else if let Some(gate) = stock_gate {
            Some(gate.reason().to_string())
        } else if !self.state.regime.snapshot.initialized {
            Some("SIGNAL_WARMUP".to_string())
        } else if stock_adaptive
            && self.state.regime_confirmation_bars < self.config.grid.regime_confirmation_bars
        {
            Some("REGIME_CONFIRMATION".to_string())
        } else if !is_fresh(
            self.state.regime.snapshot.ts_ns,
            now,
            self.config.grid.max_signal_age_secs,
        ) {
            Some("STALE_SIGNAL".to_string())
        } else if matches!(
            self.state.regime.snapshot.regime,
            MarketRegime::Disabled | MarketRegime::HighVolatility
        ) {
            Some(format!("REGIME_{:?}", self.state.regime.snapshot.regime))
        } else if self.state.regime.policy(&self.config.grid) == TrendPolicy::Disable {
            Some("TREND_POLICY_DISABLE".to_string())
        } else if self.state.reset_reason.is_some() {
            Some("RESET_RECONCILIATION".to_string())
        } else {
            None
        };
        let mut report = self.report.borrow_mut();
        report.diagnostics.observe_spacing(
            &self.config.grid,
            &self.state.regime,
            bar.close.as_decimal(),
            self.state.grid.as_ref(),
        );
        if let Some(reason) = blocked {
            ObservationCount::record(&mut report.diagnostics.blocked_bars, &reason, now);
        }
        drop(report);
        runtime.persist_engine(self, None)
    }

    pub(super) fn control(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        id: &str,
        phase: OrderPhase,
    ) {
        let now = runtime.clock().timestamp_ns().as_u64();
        self.state.orders.transition(id, phase, now);
        if let Some(order) = self.state.orders.orders().get(id) {
            log::info!(
                "GRID_ORDER_EVENT timestamp_ns={now} symbol={} grid_id={} level={} order_id={id} observed={phase:?} state={:?} filled={} position={}",
                self.config.instrument_id,
                order.grid_id,
                order.level,
                order.phase,
                order.filled,
                self.state.orders.inventory()
            );
        }
        if let Some(grid) = &mut self.state.grid
            && let Some(level) = grid.levels.iter_mut().find(|l| {
                l.entry_order_id.as_deref() == Some(id) || l.exit_order_id.as_deref() == Some(id)
            })
            && matches!(
                phase,
                OrderPhase::Cancelled | OrderPhase::Expired | OrderPhase::Rejected
            )
        {
            level.status = LevelStatus::Cancelled;
        }
        if let Err(e) = runtime.persist_engine(self, None) {
            self.halt(runtime, e);
        }
    }

    pub(super) fn risk_off(&mut self, runtime: &mut MultiAssetGridStrategy, reason: String) {
        self.state.risk.trip(reason);
        self.state.state = StrategyState::RiskOff;
        if let Err(e) = runtime.persist_engine(self, None) {
            self.halt(runtime, e);
        }
    }

    pub(super) fn rejected(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        id: &str,
        reason: String,
    ) {
        let exit = self.state.orders.orders().get(id).is_some_and(|o| !o.buy);
        if self
            .state
            .orders
            .orders()
            .get(id)
            .is_some_and(|o| !o.phase.terminal())
        {
            self.report
                .borrow_mut()
                .diagnostics
                .rejections
                .entry(id.to_string())
                .or_insert_with(|| RejectionObservation {
                    ts_ns: runtime.clock().timestamp_ns().as_u64(),
                    reason: reason.clone(),
                });
        }
        self.control(runtime, id, OrderPhase::Rejected);
        if exit {
            self.halt(runtime, anyhow::anyhow!(reason));
        } else {
            self.risk_off(runtime, reason);
        }
    }
}

impl GridStrategyEngine {
    pub(super) fn on_start(&mut self, runtime: &mut MultiAssetGridStrategy) -> anyhow::Result<()> {
        let instrument = runtime.cache().try_instrument(&self.config.instrument_id)?;
        anyhow::ensure!(
            matches!(
                instrument,
                InstrumentAny::Equity(_) | InstrumentAny::CurrencyPair(_)
            ),
            "Grid supports spot/equity instruments only"
        );
        anyhow::ensure!(
            instrument.tick_scheme().is_none(),
            "Variable tick schemes require a price-dependent grid quantizer"
        );
        self.instrument = Some(instrument);
        if let Err(e) = self.recover(runtime) {
            self.halt(runtime, e);
        }
        runtime.subscribe_bars(self.config.bar_type, None, None);
        if self.config.tick_execution {
            runtime.subscribe_quotes(self.config.instrument_id, None, None);
            runtime.subscribe_trades(self.config.instrument_id, None, None);
        }
        Ok(())
    }

    pub(super) fn on_bar(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        bar: &Bar,
    ) -> anyhow::Result<()> {
        if !self.config.confirmed_custom_bars {
            self.completed_bar(runtime, bar)?;
        }
        Ok(())
    }

    pub(super) fn on_data(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        data: &CustomData,
    ) -> anyhow::Result<()> {
        if self.config.confirmed_custom_bars
            && let Some(value) = data.data.as_any().downcast_ref::<BarWithVwap>()
        {
            self.completed_bar(runtime, &value.bar)?;
        }
        Ok(())
    }

    pub(super) fn on_quote(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        quote: &QuoteTick,
    ) -> anyhow::Result<()> {
        if self.config.tick_execution && quote.instrument_id == self.config.instrument_id {
            if quote.bid_price.as_decimal() <= Decimal::ZERO
                || quote.ask_price < quote.bid_price
                || quote.bid_size.is_zero()
                || quote.ask_size.is_zero()
            {
                return Ok(());
            }
            let price =
                (quote.bid_price.as_decimal() + quote.ask_price.as_decimal()) / Decimal::from(2);
            if self.config.grid.strategy_mode == StrategyMode::StockAdaptive {
                self.state
                    .stock
                    .observe_quote(quote.bid_price.as_decimal(), quote.ask_price.as_decimal());
            }
            let now = runtime.clock().timestamp_ns().as_u64();
            if quote.ts_event.as_u64() < self.state.last_tick_event_ns
                || !is_fresh(
                    quote.ts_event.as_u64(),
                    now,
                    self.config.grid.max_signal_age_secs,
                )
            {
                return Ok(());
            }
            self.state.last_tick_event_ns = quote.ts_event.as_u64();
            self.drive(runtime, price, now)?;
        }
        Ok(())
    }

    pub(super) fn on_trade(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        trade: &TradeTick,
    ) -> anyhow::Result<()> {
        if self.config.tick_execution && trade.instrument_id == self.config.instrument_id {
            let now = runtime.clock().timestamp_ns().as_u64();
            if trade.ts_event.as_u64() < self.state.last_tick_event_ns
                || !is_fresh(
                    trade.ts_event.as_u64(),
                    now,
                    self.config.grid.max_signal_age_secs,
                )
            {
                return Ok(());
            }
            self.state.last_tick_event_ns = trade.ts_event.as_u64();
            self.drive(runtime, trade.price.as_decimal(), now)?;
        }
        Ok(())
    }

    pub(super) fn on_time_event(
        &mut self,
        runtime: &mut MultiAssetGridStrategy,
        _event: &TimeEvent,
    ) -> anyhow::Result<()> {
        let now = runtime.clock().timestamp_ns().as_u64();
        if self
            .state
            .orders
            .timed_out(self.config.grid.order_timeout_secs, now)
        {
            self.state.risk.trip("Order confirmation timeout");
            self.state.state = StrategyState::RiskOff;
            if let Err(e) = runtime.persist_engine(self, None) {
                self.halt(runtime, e);
            }
            for id in self.state.orders.active_ids() {
                if let Some(order) = runtime.cache().order(&ClientOrderId::new_checked(id)?)
                    && order.venue_order_id().is_some()
                {
                    runtime.query_order(&order, None, None)?;
                }
            }
        }
        if !is_fresh(
            self.state.last_market_ns,
            now,
            self.config.grid.max_signal_age_secs,
        ) {
            self.cancel(runtime, true, "STALE_MARK")?;
        }
        Ok(())
    }

    pub(super) fn watchdog_required(&self, now: u64) -> bool {
        self.state
            .orders
            .timed_out(self.config.grid.order_timeout_secs, now)
            || (!is_fresh(
                self.state.last_market_ns,
                now,
                self.config.grid.max_signal_age_secs,
            ) && self
                .state
                .orders
                .active_ids()
                .iter()
                .any(|id| self.state.orders.orders()[id].buy))
    }

    pub(super) fn on_socket_state(
        &self,
        _runtime: &mut MultiAssetGridStrategy,
        event: &SocketStateChanged,
    ) -> anyhow::Result<()> {
        if event.venue == Some(self.config.instrument_id.venue)
            && event.state == SocketState::Disconnected
        {
            anyhow::bail!("Transport disconnected; reconcile before resuming");
        }
        Ok(())
    }

    pub(super) fn on_stop(&mut self, runtime: &mut MultiAssetGridStrategy) -> anyhow::Result<()> {
        self.state.state = StrategyState::Stopped;
        self.cancel(runtime, false, "STRATEGY_STOP")?;
        let now = runtime.clock().timestamp_ns().as_u64();
        self.record_equity(now);
        self.report.borrow_mut().finish(
            self.config.grid.capital,
            &self.state.orders,
            &self.state.risk,
        );
        runtime.persist_engine(self, None)
    }
}

fn phase(status: OrderStatus) -> OrderPhase {
    match status {
        OrderStatus::Filled => OrderPhase::Filled,
        OrderStatus::Canceled => OrderPhase::Cancelled,
        OrderStatus::Expired => OrderPhase::Expired,
        OrderStatus::Rejected | OrderStatus::Denied => OrderPhase::Rejected,
        OrderStatus::Accepted => OrderPhase::Accepted,
        OrderStatus::PartiallyFilled => OrderPhase::PartiallyFilled,
        OrderStatus::PendingCancel => OrderPhase::CancelPending,
        OrderStatus::Submitted => OrderPhase::Submitted,
        _ => OrderPhase::Unknown,
    }
}
