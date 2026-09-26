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

//! 持久化订单意图标识、部分成交库存管理与精确网格周期核算。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    config::GridConfig,
    engine::{GridEngine, GridLevel},
};

/// 单个股票持仓内部的库存归属。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionComponent {
    /// 长期核心库存，普通网格止盈不能卖出。
    Core,
    /// 由网格周期持有的战术库存。
    #[default]
    Grid,
}

/// 订单提交或撤销结果不确定时，仍视为有效资金与仓位预留。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderPhase {
    /// 已先行持久化，但尚未发送至执行引擎。
    Intent,
    /// 执行传输层已收到命令。
    Submitted,
    /// 券商已接受订单。
    Accepted,
    /// 已发生部分真实成交。
    PartiallyFilled,
    /// 已请求撤单，但尚未确认。
    CancelPending,
    /// 因超时、断线或撤单拒绝而需要与券商对账。
    Unknown,
    /// 全部委托数量已成交。
    Filled,
    /// 撤单已确认。
    Cancelled,
    /// 券商已确认订单过期。
    Expired,
    /// 订单提交被明确拒绝。
    Rejected,
}

impl OrderPhase {
    /// 券商是否已不可能再回报该订单的新成交。
    #[must_use]
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Expired | Self::Rejected
        )
    }
}

/// 一笔不可变订单意图及其已观测生命周期。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridOrder {
    /// 客户端订单标识，同时作为券商侧幂等键。
    pub id: String,
    /// 所属网格代次。
    pub grid_id: u64,
    /// 有符号网格层级索引。
    pub level: i32,
    /// 订单归属核心仓或战术网格仓。
    #[serde(default)]
    pub component: PositionComponent,
    /// true 表示买入库存，false 表示有库存覆盖的卖出。
    pub buy: bool,
    /// 委托总数量。
    pub quantity: Decimal,
    /// 已去重并只累计一次的增量成交数量。
    pub filled: Decimal,
    /// 限价；建立种子仓或风险减仓的市价单为 None。
    pub limit: Option<Decimal>,
    /// 产生订单意图时的参考价，用于衡量执行滑点。
    pub reference: Decimal,
    /// 为卖单提供库存的入场订单标识；买单则指向自身。
    pub lot_id: String,
    /// 最近已知订单阶段。
    pub phase: OrderPhase,
    /// 最近一次控制面状态变更时间戳。
    pub updated_ns: u64,
}

/// 单个买入到卖出周期的成交库存与累计经济结果。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InventoryLot {
    /// 入场订单标识。
    pub id: String,
    /// 原始网格代次。
    pub grid_id: u64,
    /// 原始网格层级索引。
    pub level: i32,
    /// 库存归属核心仓或战术网格仓。
    #[serde(default)]
    pub component: PositionComponent,
    /// 止盈目标；可选成本适配只能在首次卖单前降低，网格重置后仍保留。
    pub target: Decimal,
    /// 已买入数量。
    pub bought: Decimal,
    /// 已卖出数量。
    pub sold: Decimal,
    /// 未扣费用的实际买入成交金额。
    pub entry_value: Decimal,
    /// 未扣费用的实际卖出成交金额。
    pub exit_value: Decimal,
    /// 剩余库存尚未分摊完的入场金额与费用。
    pub remaining_cost: Decimal,
    /// 入场与出场费用合计。
    pub fees: Decimal,
    /// 相对订单意图价格的有符号执行损耗；负值表示价格改善。
    pub slippage: Decimal,
    /// 按决策参考价计算的理论入场金额。
    pub entry_reference: Decimal,
    /// 按决策参考价计算的理论出场金额。
    pub exit_reference: Decimal,
    /// 首次成交时间。
    pub first_fill_ns: Option<u64>,
    /// 最近一次出场成交时间。
    pub last_fill_ns: u64,
    /// 完成周期是否已经写入统计，防止重复结算。
    pub recorded: bool,
}

/// 一个独立完成的库存周期，也包含部分买入后撤销剩余委托的情况。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridCycle {
    /// 原始入场订单标识。
    pub entry_order_id: String,
    /// 原始网格代次。
    pub grid_id: u64,
    /// 核心仓再平衡周期或已完成的战术网格周期。
    #[serde(default)]
    pub component: PositionComponent,
    /// 实际平均入场价。
    pub entry_price: Decimal,
    /// 实际平均出场价。
    pub exit_price: Decimal,
    /// 完成买卖配对的数量。
    pub quantity: Decimal,
    /// 未扣执行损耗和费用、按决策价格计算的毛利润。
    pub gross_pnl: Decimal,
    /// 未扣费用、按实际成交价格计算的利润。
    pub execution_pnl: Decimal,
    /// 入场与出场费用。
    pub fees: Decimal,
    /// 有符号滑点；负值表示成交价格改善。
    pub slippage: Decimal,
    /// 毛利润只扣一次费用与滑点后的净利润。
    pub net_pnl: Decimal,
    /// 从首次买入成交到最后一次卖出成交的持有时间。
    pub holding_ns: u64,
}

/// 可持久化订单账本；任何“已提交”状态都不会被当作成交。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderManager {
    /// 全部订单意图标识，包括用于重放去重的已终结订单。
    orders: BTreeMap<String, GridOrder>,
    /// 入场批次及其仍保留的库存。
    lots: BTreeMap<String, InventoryLot>,
    /// 逐笔完成的库存周期。
    pub cycles: Vec<GridCycle>,
    /// 本地分配现金，已计入真实或估算的成交费用。
    pub cash: Decimal,
    /// 已实现盈亏，包含部分减仓。
    pub realized_pnl: Decimal,
    /// 长期核心仓再平衡产生的精确已实现盈亏。
    #[serde(default)]
    core_realized_pnl: Decimal,
    /// 战术网格仓产生的精确已实现盈亏。
    #[serde(default)]
    grid_realized_pnl: Decimal,
    /// 已收取或估算的费用合计，包含未平库存的入场费用。
    pub fees: Decimal,
    /// 费用使用估算值的成交数量。
    pub estimated_fee_fills: u64,
    /// 去重后的成交数量。
    pub fill_count: u64,
    /// 总成交名义金额。
    pub turnover: Decimal,
    /// 归属于核心仓再平衡的成交名义金额。
    #[serde(default)]
    core_turnover: Decimal,
    /// 归属于战术网格订单的成交名义金额。
    #[serde(default)]
    grid_turnover: Decimal,
    /// 全部成交的有符号滑点。
    pub slippage: Decimal,
    /// 全部成交的不利执行损耗。
    #[serde(default)]
    pub adverse_slippage: Decimal,
    /// 全部成交的有利价格改善。
    #[serde(default)]
    pub price_improvement: Decimal,
    sequence: u64,
    seen_fills: BTreeSet<(String, String)>,
    // 纯派生索引：绝不信任或持久化检查点中传入的索引。
    #[serde(skip)]
    live: OnceLock<LiveLedger>,
}

#[derive(Clone, Debug)]
struct LiveLedger {
    active_ids: Vec<String>,
    open_lot_ids: Vec<String>,
    sell_reservations: BTreeMap<String, Decimal>,
    occupied: BTreeSet<(u64, i32)>,
    inventory: Decimal,
    cost: Decimal,
    component_inventory: [Decimal; 2],
    component_buys: [Decimal; 2],
    component_sells: [Decimal; 2],
}

impl PositionComponent {
    const fn index(self) -> usize {
        match self {
            Self::Core => 0,
            Self::Grid => 1,
        }
    }
}

impl OrderManager {
    /// 仅为组合报告合并不可变核算结果，绝不用于实盘交易状态。
    pub(super) fn merge_report(&mut self, other: &Self) {
        self.cycles.extend(other.cycles.iter().cloned());
        self.realized_pnl += other.realized_pnl;
        self.core_realized_pnl += other.core_realized_pnl;
        self.grid_realized_pnl += other.grid_realized_pnl;
        self.fees += other.fees;
        self.slippage += other.slippage;
        self.adverse_slippage += other.adverse_slippage;
        self.price_improvement += other.price_improvement;
        self.turnover += other.turnover;
        self.core_turnover += other.core_turnover;
        self.grid_turnover += other.grid_turnover;
        self.fill_count += other.fill_count;
        self.estimated_fee_fills += other.estimated_fee_fills;
    }

    /// 不可变订单审计历史；任何修改都必须使派生实时索引失效。
    #[must_use]
    pub fn orders(&self) -> &BTreeMap<String, GridOrder> {
        &self.orders
    }

    /// 不可变库存归属与周期历史，包含已退出的网格代次。
    #[must_use]
    pub fn lots(&self) -> &BTreeMap<String, InventoryLot> {
        &self.lots
    }

    fn live(&self) -> &LiveLedger {
        // ponytail: 每次修改后最多重建一次；只有性能采样证明成交吞吐而非 watchdog
        // 重复读取成为瓶颈时，才维护增量索引。
        self.live.get_or_init(|| {
            let mut live = LiveLedger {
                active_ids: Vec::new(),
                open_lot_ids: Vec::new(),
                sell_reservations: BTreeMap::new(),
                occupied: BTreeSet::new(),
                inventory: Decimal::ZERO,
                cost: Decimal::ZERO,
                component_inventory: [Decimal::ZERO; 2],
                component_buys: [Decimal::ZERO; 2],
                component_sells: [Decimal::ZERO; 2],
            };
            for order in self.orders.values().filter(|o| !o.phase.terminal()) {
                live.active_ids.push(order.id.clone());
                live.occupied.insert((order.grid_id, order.level));
                if order.buy {
                    live.component_buys[order.component.index()] += order.quantity - order.filled;
                } else {
                    *live
                        .sell_reservations
                        .entry(order.lot_id.clone())
                        .or_default() += order.quantity - order.filled;
                    live.component_sells[order.component.index()] += order.quantity - order.filled;
                }
            }
            for lot in self.lots.values() {
                let quantity = lot.bought - lot.sold;
                live.inventory += quantity;
                live.component_inventory[lot.component.index()] += quantity;
                live.cost += lot.remaining_cost;
                if lot.bought > lot.sold {
                    live.open_lot_ids.push(lot.id.clone());
                    live.occupied.insert((lot.grid_id, lot.level));
                }
            }
            live
        })
    }

    fn active_orders(&self) -> impl Iterator<Item = &GridOrder> {
        self.live().active_ids.iter().map(|id| &self.orders[id])
    }

    /// 仍有库存且尚未被有效卖单覆盖的未平批次。
    #[must_use]
    pub(super) fn exit_candidates(&self) -> Vec<String> {
        self.reduction_candidates(PositionComponent::Grid)
    }

    /// 指定仓位组件中，尚未被其他卖单覆盖的未平批次。
    #[must_use]
    pub(super) fn reduction_candidates(&self, component: PositionComponent) -> Vec<String> {
        self.live()
            .open_lot_ids
            .iter()
            .filter(|id| {
                self.lots[*id].component == component
                    && self.unreserved_inventory(id) > Decimal::ZERO
            })
            .cloned()
            .collect()
    }

    fn unreserved_inventory(&self, lot_id: &str) -> Decimal {
        self.lots.get(lot_id).map_or(Decimal::ZERO, |lot| {
            lot.bought
                - lot.sold
                - self
                    .live()
                    .sell_reservations
                    .get(lot_id)
                    .copied()
                    .unwrap_or_default()
        })
    }

    /// 在任何 Nautilus 原生订单产生前，缩减或延后本地订单意图。
    ///
    /// # Errors
    ///
    /// 意图未知、已发送、已有成交、数量为负或试图放大数量时返回错误。
    pub(super) fn resize_intent(&mut self, id: &str, quantity: Decimal) -> anyhow::Result<()> {
        let order = self
            .orders
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown intent"))?;
        anyhow::ensure!(
            order.phase == OrderPhase::Intent
                && order.filled.is_zero()
                && quantity >= Decimal::ZERO
                && quantity <= order.quantity,
            "Cannot resize a dispatched or increasing intent"
        );
        let buy = order.buy;
        self.live.take();
        if quantity.is_zero() {
            self.orders.remove(id);
            if buy {
                self.lots.remove(id);
            }
        } else if let Some(order) = self.orders.get_mut(id) {
            order.quantity = quantity;
        }
        Ok(())
    }

    /// 在使用任何恢复订单前，校验持久化账本内的所有关联关系。
    ///
    /// # Errors
    ///
    /// 库存、标识、成交汇总或完成周期不一致时返回错误。
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.fill_count == self.seen_fills.len() as u64
                && self.estimated_fee_fills <= self.fill_count
                && self.realized_pnl == self.core_realized_pnl + self.grid_realized_pnl
                && self.turnover == self.core_turnover + self.grid_turnover,
            "Invalid recovered fill counters"
        );
        for (id, order) in &self.orders {
            anyhow::ensure!(
                id == &order.id
                    && order.quantity > Decimal::ZERO
                    && order.filled >= Decimal::ZERO
                    && order.filled <= order.quantity
                    && order.reference > Decimal::ZERO
                    && order.limit.is_none_or(|p| p > Decimal::ZERO),
                "Invalid recovered order"
            );
            anyhow::ensure!(
                self.lots.get(&order.lot_id).is_some_and(|lot| {
                    order.component == lot.component && (!order.buy || order.lot_id == *id)
                }),
                "Recovered order has no inventory owner"
            );
            let sequence: u64 = id
                .rsplit('-')
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing order sequence"))?
                .parse()?;
            anyhow::ensure!(
                sequence <= self.sequence,
                "Recovered sequence would reuse an identity"
            );
            anyhow::ensure!(
                order.phase != OrderPhase::Filled || order.filled == order.quantity,
                "Inconsistent terminal fill quantity"
            );
        }
        for (id, lot) in &self.lots {
            let entry = self
                .orders
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("Recovered inventory has no entry"))?;
            let sold: Decimal = self
                .orders
                .values()
                .filter(|o| !o.buy && o.lot_id == *id)
                .map(|o| o.filled)
                .sum();
            anyhow::ensure!(
                lot.id == *id
                    && entry.buy
                    && lot.bought == entry.filled
                    && lot.sold == sold
                    && lot.bought >= lot.sold
                    && lot.sold >= Decimal::ZERO
                    && lot.target > Decimal::ZERO,
                "Recovered inventory does not reconcile"
            );
            anyhow::ensure!(
                !lot.recorded
                    || (lot.bought > Decimal::ZERO
                        && lot.bought == lot.sold
                        && entry.phase.terminal()),
                "Invalid completed inventory"
            );
        }
        let mut cycles = BTreeSet::new();
        for cycle in &self.cycles {
            anyhow::ensure!(
                cycles.insert(&cycle.entry_order_id)
                    && self
                        .lots
                        .get(&cycle.entry_order_id)
                        .is_some_and(|l| l.recorded)
                    && cycle.gross_pnl - cycle.fees - cycle.slippage == cycle.net_pnl,
                "Invalid recovered cycle"
            );
        }
        anyhow::ensure!(
            cycles.len() == self.lots.values().filter(|l| l.recorded).count()
                && self
                    .seen_fills
                    .iter()
                    .all(|(id, _)| self.orders.contains_key(id))
                && self.adverse_slippage - self.price_improvement == self.slippage,
            "Recovered audit records do not reconcile"
        );
        Ok(())
    }

    /// 使用给定资金初始化空库存订单账本。
    #[must_use]
    pub fn new(capital: Decimal) -> Self {
        Self {
            orders: BTreeMap::new(),
            lots: BTreeMap::new(),
            cycles: Vec::new(),
            cash: capital,
            realized_pnl: Decimal::ZERO,
            core_realized_pnl: Decimal::ZERO,
            grid_realized_pnl: Decimal::ZERO,
            fees: Decimal::ZERO,
            estimated_fee_fills: 0,
            fill_count: 0,
            turnover: Decimal::ZERO,
            core_turnover: Decimal::ZERO,
            grid_turnover: Decimal::ZERO,
            slippage: Decimal::ZERO,
            adverse_slippage: Decimal::ZERO,
            price_improvement: Decimal::ZERO,
            sequence: 0,
            seen_fills: BTreeSet::new(),
            live: OnceLock::new(),
        }
    }

    /// 仅当对应层级没有未终结订单和遗留库存时创建入场意图。
    ///
    /// # Errors
    ///
    /// 层级被重复占用或数量、价格无效时返回错误。
    pub fn entry(
        &mut self,
        namespace: &str,
        grid_id: u64,
        level: &GridLevel,
        quantity: Decimal,
        market_reference: Option<Decimal>,
        now: u64,
    ) -> anyhow::Result<GridOrder> {
        self.entry_component(
            namespace,
            grid_id,
            level,
            quantity,
            market_reference,
            PositionComponent::Grid,
            now,
        )
    }

    /// 创建归属于指定仓位组件的入场；核心仓同样使用稳定标识与统一账本。
    #[allow(
        clippy::too_many_arguments,
        reason = "explicit durable order ownership"
    )]
    pub(super) fn entry_component(
        &mut self,
        namespace: &str,
        grid_id: u64,
        level: &GridLevel,
        quantity: Decimal,
        market_reference: Option<Decimal>,
        component: PositionComponent,
        now: u64,
    ) -> anyhow::Result<GridOrder> {
        anyhow::ensure!(
            component == PositionComponent::Core || !self.slot_busy(grid_id, level.level_index),
            "Grid pair already has an entry or inventory"
        );
        let reference = market_reference.unwrap_or(level.price);
        anyhow::ensure!(
            quantity > Decimal::ZERO && reference > Decimal::ZERO && level.exit_price > reference,
            "Invalid entry intent"
        );
        // 序号随检查点持久化，namespace 含策略及标的，reset 和重启都不能复用已发送身份
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Order identity exhausted"))?;
        let id = format!(
            "DG-{namespace}-{grid_id}-{}-B-{}",
            level.level_index, self.sequence
        );
        let order = GridOrder {
            id: id.clone(),
            grid_id,
            level: level.level_index,
            component,
            buy: true,
            quantity,
            filled: Decimal::ZERO,
            limit: market_reference.is_none().then_some(level.price),
            reference,
            lot_id: id.clone(),
            phase: OrderPhase::Intent,
            updated_ns: now,
        };
        self.lots.insert(
            id.clone(),
            InventoryLot {
                id: id.clone(),
                grid_id,
                level: level.level_index,
                component,
                target: level.exit_price,
                bought: Decimal::ZERO,
                sold: Decimal::ZERO,
                entry_value: Decimal::ZERO,
                exit_value: Decimal::ZERO,
                remaining_cost: Decimal::ZERO,
                fees: Decimal::ZERO,
                slippage: Decimal::ZERO,
                entry_reference: Decimal::ZERO,
                exit_reference: Decimal::ZERO,
                first_fill_ns: None,
                last_fill_ns: 0,
                recorded: false,
            },
        );
        self.live.take();
        self.orders.insert(id, order.clone());
        Ok(order)
    }

    /// 仅为全额成交、尚无任何卖单历史的 Grid 限价买入调整一次目标。
    /// 部分成交即时覆盖路径、种子/Core、旧代库存及既有卖单均不改价。
    pub(super) fn adapt_exit_target(
        &mut self,
        id: &str,
        grid: &GridEngine,
        config: &GridConfig,
    ) -> Option<(Decimal, Decimal)> {
        let entry = self.orders.get(id)?;
        let lot = self.lots.get(id)?;
        let entry_limit = entry.limit?;
        if !entry.buy
            || entry.component != PositionComponent::Grid
            || entry.phase != OrderPhase::Filled
            || entry.grid_id != grid.grid_id
            || lot.bought <= Decimal::ZERO
            || lot.sold > Decimal::ZERO
            || self.orders.values().any(|o| !o.buy && o.lot_id == id)
        {
            return None;
        }
        // 入场成本已含真实成交价和费用，不能再扣一次入场滑点。出场按保守费率和滑点预算。
        let exit_cost =
            config.maker_fee.max(config.taker_fee) + config.commission + config.slippage;
        let minimum_profit = lot.entry_value * config.minimum_profit_margin;
        let target = grid
            .levels
            .iter()
            .map(|level| level.price)
            .filter(|price| {
                let net = *price * lot.bought * (Decimal::ONE - exit_cost) - lot.remaining_cost;
                // 至少换到原买入层或更低；不能把买价向下/卖价向上取整的一 tick 差当成换层。
                *price <= entry_limit
                    && *price < lot.target
                    && net > Decimal::ZERO
                    && net >= minimum_profit
            })
            .min()?;
        let previous = lot.target;
        self.lots.get_mut(id)?.target = target;
        self.live.take();
        Some((previous, target))
    }

    /// 为真实已成交库存创建覆盖卖单，并扣除已有卖单的预留数量。
    ///
    /// # Errors
    ///
    /// 库存不存在、已被完全预留或参考价格无效时返回错误。
    pub fn exit(
        &mut self,
        namespace: &str,
        lot_id: &str,
        market_reference: Option<Decimal>,
        now: u64,
    ) -> anyhow::Result<GridOrder> {
        self.exit_quantity(namespace, lot_id, market_reference, Decimal::MAX, now)
    }

    /// 创建受目标减仓数量上限约束的覆盖卖单。
    pub(super) fn exit_quantity(
        &mut self,
        namespace: &str,
        lot_id: &str,
        market_reference: Option<Decimal>,
        maximum: Decimal,
        now: u64,
    ) -> anyhow::Result<GridOrder> {
        let lot = self
            .lots
            .get(lot_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown inventory lot"))?;
        // 只能卖出实际买到且未被其他卖单预留的数量，部分成交不能按原委托总量挂卖单
        let quantity = self.unreserved_inventory(lot_id).min(maximum);
        let reference = market_reference.unwrap_or(lot.target);
        anyhow::ensure!(
            quantity > Decimal::ZERO && reference > Decimal::ZERO,
            "No unreserved inventory for exit"
        );
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Order identity exhausted"))?;
        let id = format!(
            "DG-{namespace}-{}-{}-S-{}",
            lot.grid_id, lot.level, self.sequence
        );
        let order = GridOrder {
            id: id.clone(),
            grid_id: lot.grid_id,
            level: lot.level,
            component: lot.component,
            buy: false,
            quantity,
            filled: Decimal::ZERO,
            limit: market_reference.is_none().then_some(lot.target),
            reference,
            lot_id: lot_id.to_string(),
            phase: OrderPhase::Intent,
            updated_ns: now,
        };
        self.live.take();
        self.orders.insert(id, order.clone());
        Ok(order)
    }

    /// 该层级是否已被有效买单或保留库存占用。
    #[must_use]
    pub fn slot_busy(&self, grid_id: u64, level: i32) -> bool {
        self.live().occupied.contains(&(grid_id, level))
    }

    /// 更新控制面事件，但不允许延迟确认反向覆盖已成交或已撤销状态。
    pub fn transition(&mut self, id: &str, phase: OrderPhase, now: u64) {
        if let Some(order) = self.orders.get_mut(id) {
            if order.phase.terminal() {
                return;
            }
            if order.phase == OrderPhase::CancelPending
                && matches!(phase, OrderPhase::Submitted | OrderPhase::Accepted)
            {
                return;
            }
            if phase == OrderPhase::Intent
                || (phase == OrderPhase::Submitted && order.phase == OrderPhase::Accepted)
                || (order.filled > Decimal::ZERO
                    && matches!(phase, OrderPhase::Submitted | OrderPhase::Accepted))
            {
                return;
            }
            // Submitted/Accepted/CancelPending/Unknown 都仍占用同一预留，无需让派生索引失效。
            if phase.terminal() {
                self.live.take();
            }
            let lot_id = order.lot_id.clone();
            order.phase = phase;
            order.updated_ns = now;
            self.complete_cycle(&lot_id);
        }
    }

    /// 一笔增量成交只应用一次；未知订单或超卖一律按失败关闭处理。
    ///
    /// # Errors
    ///
    /// 订单未知、成交无效、超额成交或库存不足时返回错误。
    #[allow(
        clippy::too_many_arguments,
        reason = "One incremental execution and its accounting metadata"
    )]
    pub fn fill(
        &mut self,
        id: &str,
        trade_id: &str,
        quantity: Decimal,
        price: Decimal,
        fee: Decimal,
        estimated: bool,
        now: u64,
    ) -> anyhow::Result<bool> {
        // 用订单身份与成交身份联合去重，重复推送不能重复扣现金、增加库存或累计费用
        let key = (id.to_string(), trade_id.to_string());
        if self.seen_fills.contains(&key) {
            return Ok(false);
        }
        let order = self
            .orders
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Fill for unknown grid order {id}"))?;
        anyhow::ensure!(
            quantity > Decimal::ZERO
                && price > Decimal::ZERO
                && order.filled + quantity <= order.quantity,
            "Invalid or excessive fill quantity"
        );
        let lot = self
            .lots
            .get(&order.lot_id)
            .ok_or_else(|| anyhow::anyhow!("Missing fill inventory lot"))?;
        let lot_id = order.lot_id.clone();
        anyhow::ensure!(
            !lot.recorded,
            "Late fill after cycle completion requires audit"
        );
        anyhow::ensure!(
            order.buy || quantity <= lot.bought - lot.sold,
            "Sell exceeds filled inventory"
        );
        let value = quantity * price;
        let reference = quantity * order.reference;
        let shortfall = if order.buy {
            value - reference
        } else {
            reference - value
        };
        let order = self
            .orders
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Missing validated order"))?;
        let lot = self
            .lots
            .get_mut(&order.lot_id)
            .ok_or_else(|| anyhow::anyhow!("Missing validated lot"))?;
        self.live.take();
        order.filled += quantity;
        if order.filled == order.quantity {
            order.phase = OrderPhase::Filled;
        } else if !order.phase.terminal() && order.phase != OrderPhase::CancelPending {
            order.phase = OrderPhase::PartiallyFilled;
        }
        if order.buy {
            self.cash -= value + fee;
            lot.bought += quantity;
            lot.entry_value += value;
            lot.entry_reference += reference;
            lot.remaining_cost += value + fee;
            lot.first_fill_ns.get_or_insert(now);
        } else {
            let cost = lot.remaining_cost * quantity / (lot.bought - lot.sold);
            lot.remaining_cost -= cost;
            self.cash += value - fee;
            let realized = value - fee - cost;
            self.realized_pnl += realized;
            match lot.component {
                PositionComponent::Core => self.core_realized_pnl += realized,
                PositionComponent::Grid => self.grid_realized_pnl += realized,
            }
            lot.sold += quantity;
            lot.exit_value += value;
            lot.exit_reference += reference;
        }
        lot.last_fill_ns = lot.last_fill_ns.max(now);
        lot.fees += fee;
        lot.slippage += shortfall;
        self.fees += fee;
        self.slippage += shortfall;
        self.adverse_slippage += shortfall.max(Decimal::ZERO);
        self.price_improvement += (-shortfall).max(Decimal::ZERO);
        self.turnover += value;
        match lot.component {
            PositionComponent::Core => self.core_turnover += value,
            PositionComponent::Grid => self.grid_turnover += value,
        }
        self.fill_count += 1;
        self.estimated_fee_fills += u64::from(estimated);
        self.seen_fills.insert(key);
        self.complete_cycle(&lot_id);
        Ok(true)
    }

    /// 实际拥有的多头总数量，包含已退出网格代次的遗留库存。
    #[must_use]
    pub fn inventory(&self) -> Decimal {
        self.live().inventory
    }

    /// 指定目标仓位组件实际成交的库存数量。
    #[must_use]
    pub(super) fn component_inventory(&self, component: PositionComponent) -> Decimal {
        self.live().component_inventory[component.index()]
    }

    /// 指定目标仓位组件尚未终结的买入与卖出预留数量。
    #[must_use]
    pub(super) fn component_reservations(
        &self,
        component: PositionComponent,
    ) -> (Decimal, Decimal) {
        (
            self.live().component_buys[component.index()],
            self.live().component_sells[component.index()],
        )
    }

    /// 剩余库存的入场成本，包含已分摊费用。
    #[must_use]
    pub fn inventory_cost(&self) -> Decimal {
        self.live().cost
    }

    /// 指定目标仓位组件的精确已实现盈亏。
    #[must_use]
    pub(super) const fn component_realized_pnl(&self, component: PositionComponent) -> Decimal {
        match component {
            PositionComponent::Core => self.core_realized_pnl,
            PositionComponent::Grid => self.grid_realized_pnl,
        }
    }

    /// 指定目标仓位组件的精确成交名义金额。
    #[must_use]
    pub(super) const fn component_turnover(&self, component: PositionComponent) -> Decimal {
        match component {
            PositionComponent::Core => self.core_turnover,
            PositionComponent::Grid => self.grid_turnover,
        }
    }

    /// 按确定性顺序返回有效订单标识。
    #[must_use]
    pub fn active_ids(&self) -> Vec<String> {
        self.live().active_ids.clone()
    }

    /// 不复制订单标识即可取得有效订单数量。
    #[must_use]
    pub(super) fn active_count(&self) -> usize {
        self.live().active_ids.len()
    }

    /// 最坏情形买入预留；仅发出撤单请求不会释放该预留。
    #[must_use]
    pub fn buy_reservations(&self, config: &GridConfig, mark: Decimal) -> (Decimal, Decimal) {
        self.active_orders()
            .filter(|o| o.buy)
            .fold((Decimal::ZERO, Decimal::ZERO), |(q, n), o| {
                let leaves = o.quantity - o.filled;
                (
                    q + leaves,
                    n + leaves
                        * o.limit.unwrap_or_else(|| o.reference.max(mark))
                        * (Decimal::ONE
                            + config.maker_fee.max(config.taker_fee)
                            + config.commission
                            + config.slippage),
                )
            })
    }

    /// 检测未解决的提交与撤单；已接受并正常挂单的订单不按此规则超时。
    #[must_use]
    pub fn timed_out(&self, timeout_secs: u64, now: u64) -> bool {
        self.active_orders().any(|o| {
            matches!(
                o.phase,
                OrderPhase::Intent
                    | OrderPhase::Submitted
                    | OrderPhase::CancelPending
                    | OrderPhase::Unknown
            ) && now.saturating_sub(o.updated_ns) / 1_000_000_000 >= timeout_secs
        })
    }

    fn complete_cycle(&mut self, lot_id: &str) {
        let Some(lot) = self.lots.get_mut(lot_id) else {
            return;
        };
        if !lot.recorded
            && lot.bought > Decimal::ZERO
            && lot.bought == lot.sold
            && self.orders[&lot.id].phase.terminal()
        {
            // gross 使用决策价，滑点只扣一次；actual fill 价差已经包含滑点，不能再重复扣除
            let gross_pnl = lot.exit_reference - lot.entry_reference;
            self.cycles.push(GridCycle {
                entry_order_id: lot.id.clone(),
                grid_id: lot.grid_id,
                component: lot.component,
                entry_price: lot.entry_value / lot.bought,
                exit_price: lot.exit_value / lot.sold,
                quantity: lot.bought,
                gross_pnl,
                execution_pnl: lot.exit_value - lot.entry_value,
                fees: lot.fees,
                slippage: lot.slippage,
                net_pnl: gross_pnl - lot.fees - lot.slippage,
                holding_ns: lot
                    .last_fill_ns
                    .saturating_sub(lot.first_fill_ns.unwrap_or(lot.last_fill_ns)),
            });
            lot.recorded = true;
            log::info!(
                "GRID_CYCLE_COMPLETED entry={} net_pnl={}",
                lot.id,
                gross_pnl - lot.fees - lot.slippage
            );
        }
    }
}
