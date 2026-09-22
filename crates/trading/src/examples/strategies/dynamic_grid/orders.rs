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

//! Durable intent identities, partial-fill inventory and exact cycle accounting.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{config::GridConfig, engine::GridLevel};

/// Inventory ownership inside one stock position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionComponent {
    /// Long-lived inventory which ordinary grid profit-taking cannot sell.
    Core,
    /// Tactical inventory owned by grid cycles.
    #[default]
    Grid,
}

/// Submission/cancellation uncertainty remains an active reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderPhase {
    /// Persisted before dispatch.
    Intent,
    /// Transport has received the command.
    Submitted,
    /// Broker accepted the order.
    Accepted,
    /// Some inventory has actually filled.
    PartiallyFilled,
    /// Cancellation requested but not confirmed.
    CancelPending,
    /// Timeout, disconnect or cancel rejection requires reconciliation.
    Unknown,
    /// All quantity executed.
    Filled,
    /// Confirmed cancellation.
    Cancelled,
    /// Broker-confirmed expiry.
    Expired,
    /// Definitive submission rejection.
    Rejected,
}

impl OrderPhase {
    /// Whether further fills are no longer expected from the broker.
    #[must_use]
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Expired | Self::Rejected
        )
    }
}

/// One immutable order intent plus its observed lifecycle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridOrder {
    /// Client identity also used as the broker idempotency key.
    pub id: String,
    /// Grid generation.
    pub grid_id: u64,
    /// Signed pair index.
    pub level: i32,
    /// Core or tactical grid inventory.
    #[serde(default)]
    pub component: PositionComponent,
    /// True for inventory acquisition; false for a covered sale.
    pub buy: bool,
    /// Requested quantity.
    pub quantity: Decimal,
    /// Incremental fills accumulated exactly once.
    pub filled: Decimal,
    /// Limit price, or None for a seed/risk-reduction market order.
    pub limit: Option<Decimal>,
    /// Decision price used to measure execution slippage.
    pub reference: Decimal,
    /// Entry order whose inventory backs a sale, or self for a buy.
    pub lot_id: String,
    /// Latest known phase.
    pub phase: OrderPhase,
    /// Most recent control-plane transition timestamp.
    pub updated_ns: u64,
}

/// Filled inventory and aggregate economics for one buy-to-sell cycle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InventoryLot {
    /// Entry identity.
    pub id: String,
    /// Original grid generation.
    pub grid_id: u64,
    /// Original pair index.
    pub level: i32,
    /// Core or tactical grid inventory.
    #[serde(default)]
    pub component: PositionComponent,
    /// Original profit-taking target, retained across resets.
    pub target: Decimal,
    /// Quantity acquired.
    pub bought: Decimal,
    /// Quantity sold.
    pub sold: Decimal,
    /// Actual entry cash value before fees.
    pub entry_value: Decimal,
    /// Actual exit cash value before fees.
    pub exit_value: Decimal,
    /// Unallocated entry cash and fees for remaining inventory.
    pub remaining_cost: Decimal,
    /// Sum of entry and exit fees.
    pub fees: Decimal,
    /// Signed execution shortfall relative to intent prices.
    pub slippage: Decimal,
    /// Hypothetical entry value at decision prices.
    pub entry_reference: Decimal,
    /// Hypothetical exit value at decision prices.
    pub exit_reference: Decimal,
    /// First fill time.
    pub first_fill_ns: Option<u64>,
    /// Latest exit fill time.
    pub last_fill_ns: u64,
    /// Whether the completed cycle has been emitted.
    pub recorded: bool,
}

/// An individually completed inventory cycle (including partial-entry cancellation).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridCycle {
    /// Original entry identity.
    pub entry_order_id: String,
    /// Original generation.
    pub grid_id: u64,
    /// Core rebalance or completed tactical grid cycle.
    #[serde(default)]
    pub component: PositionComponent,
    /// Average actual entry price.
    pub entry_price: Decimal,
    /// Average actual exit price.
    pub exit_price: Decimal,
    /// Matched quantity.
    pub quantity: Decimal,
    /// Decision-price profit before execution shortfall and fees.
    pub gross_pnl: Decimal,
    /// Actual fill-price profit before fees.
    pub execution_pnl: Decimal,
    /// Entry and exit costs.
    pub fees: Decimal,
    /// Signed slippage, negative for price improvement.
    pub slippage: Decimal,
    /// Gross profit minus fees and slippage, exactly once.
    pub net_pnl: Decimal,
    /// Time from first buy fill to final sell fill.
    pub holding_ns: u64,
}

/// Persisted ledger. No submission is treated as a fill.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderManager {
    /// All intent identities, including terminal orders for replay deduplication.
    orders: BTreeMap<String, GridOrder>,
    /// Entry lots and their retained inventory.
    lots: BTreeMap<String, InventoryLot>,
    /// Individually completed cycles.
    pub cycles: Vec<GridCycle>,
    /// Locally allocated cash including actual/estimated fill costs.
    pub cash: Decimal,
    /// Realized PnL including partial reductions.
    pub realized_pnl: Decimal,
    /// Exact realized PnL from long-lived core rebalances.
    #[serde(default)]
    core_realized_pnl: Decimal,
    /// Exact realized PnL from tactical grid inventory.
    #[serde(default)]
    grid_realized_pnl: Decimal,
    /// Total charged or estimated fees, including open inventory.
    pub fees: Decimal,
    /// Number of fills whose fees were estimated.
    pub estimated_fee_fills: u64,
    /// Number of unique fills.
    pub fill_count: u64,
    /// Total traded notional.
    pub turnover: Decimal,
    /// Traded notional attributable to core rebalances.
    #[serde(default)]
    core_turnover: Decimal,
    /// Traded notional attributable to tactical grid orders.
    #[serde(default)]
    grid_turnover: Decimal,
    /// Signed slippage across all fills.
    pub slippage: Decimal,
    sequence: u64,
    seen_fills: BTreeSet<(String, String)>,
    // Derived only: never trust or persist an index received in a checkpoint.
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
    /// Merges immutable accounting totals for a portfolio report, never for live trading.
    pub(super) fn merge_report(&mut self, other: &Self) {
        self.cycles.extend(other.cycles.iter().cloned());
        self.realized_pnl += other.realized_pnl;
        self.core_realized_pnl += other.core_realized_pnl;
        self.grid_realized_pnl += other.grid_realized_pnl;
        self.fees += other.fees;
        self.slippage += other.slippage;
        self.turnover += other.turnover;
        self.core_turnover += other.core_turnover;
        self.grid_turnover += other.grid_turnover;
        self.fill_count += other.fill_count;
        self.estimated_fee_fills += other.estimated_fee_fills;
    }

    /// Immutable audit history. Mutations must invalidate the derived live index.
    #[must_use]
    pub fn orders(&self) -> &BTreeMap<String, GridOrder> {
        &self.orders
    }

    /// Immutable ownership and cycle history, including retired grids.
    #[must_use]
    pub fn lots(&self) -> &BTreeMap<String, InventoryLot> {
        &self.lots
    }

    fn live(&self) -> &LiveLedger {
        // ponytail: rebuild once after a mutation; use incremental indices if fill throughput
        // (rather than repeated watchdog reads) becomes the measured bottleneck.
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

    /// Open lots which still have inventory not covered by a live sell order.
    #[must_use]
    pub(super) fn exit_candidates(&self) -> Vec<String> {
        self.reduction_candidates(PositionComponent::Grid)
    }

    /// Open lots for one sleeve which are not already covered by another sell.
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

    /// Reduces or defers a local intent before any native submission can exist.
    ///
    /// # Errors
    ///
    /// Rejects unknown, already dispatched, filled, negative or increased intents.
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

    /// Checks durable ledger relationships before any recovered order can be used.
    ///
    /// # Errors
    ///
    /// Returns an error for inconsistent inventory, identity, fill totals or completed cycles.
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
                    .all(|(id, _)| self.orders.contains_key(id)),
            "Recovered audit records do not reconcile"
        );
        Ok(())
    }

    /// Initializes funded cash without any inventory.
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
            sequence: 0,
            seen_fills: BTreeSet::new(),
            live: OnceLock::new(),
        }
    }

    /// Creates an entry only when its pair has no unresolved order or inventory.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate slot ownership or invalid quantity/price.
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

    /// Creates a component-owned entry; core entries use the same durable identity and ledger.
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

    /// Creates a covered exit for available filled inventory, net of existing sell reservations.
    ///
    /// # Errors
    ///
    /// Returns an error if inventory is missing, already reserved or the reference is invalid.
    pub fn exit(
        &mut self,
        namespace: &str,
        lot_id: &str,
        market_reference: Option<Decimal>,
        now: u64,
    ) -> anyhow::Result<GridOrder> {
        self.exit_quantity(namespace, lot_id, market_reference, Decimal::MAX, now)
    }

    /// Creates a covered exit capped by a target-position reduction.
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

    /// Whether this pair is occupied by an active buy or retained inventory.
    #[must_use]
    pub fn slot_busy(&self, grid_id: u64, level: i32) -> bool {
        self.live().occupied.contains(&(grid_id, level))
    }

    /// Updates a control event without permitting delayed acknowledgments to undo a fill/cancel.
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

    /// Applies one incremental fill exactly once. Unknown IDs and oversells fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown orders, invalid fills, overfills or insufficient inventory.
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

    /// Total owned long quantity, including inventory from retired grids.
    #[must_use]
    pub fn inventory(&self) -> Decimal {
        self.live().inventory
    }

    /// Filled inventory owned by one target-position sleeve.
    #[must_use]
    pub(super) fn component_inventory(&self, component: PositionComponent) -> Decimal {
        self.live().component_inventory[component.index()]
    }

    /// Unresolved buy and sell quantities for one target-position sleeve.
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

    /// Remaining entry cost including allocated fees.
    #[must_use]
    pub fn inventory_cost(&self) -> Decimal {
        self.live().cost
    }

    /// Exact realized PnL for one target-position sleeve.
    #[must_use]
    pub(super) const fn component_realized_pnl(&self, component: PositionComponent) -> Decimal {
        match component {
            PositionComponent::Core => self.core_realized_pnl,
            PositionComponent::Grid => self.grid_realized_pnl,
        }
    }

    /// Exact traded notional for one target-position sleeve.
    #[must_use]
    pub(super) const fn component_turnover(&self, component: PositionComponent) -> Decimal {
        match component {
            PositionComponent::Core => self.core_turnover,
            PositionComponent::Grid => self.grid_turnover,
        }
    }

    /// Active order identities in deterministic order.
    #[must_use]
    pub fn active_ids(&self) -> Vec<String> {
        self.live().active_ids.clone()
    }

    /// Number of live orders without cloning their identities.
    #[must_use]
    pub(super) fn active_count(&self) -> usize {
        self.live().active_ids.len()
    }

    /// Worst-case buy reservations, never released by a cancel request.
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

    /// Detects unresolved submissions and cancellations; resting accepted orders do not time out.
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
