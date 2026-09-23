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

//! Live Longbridge execution client backed by the official Rust SDK.

use std::{
    collections::VecDeque,
    fmt::Debug,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ahash::{AHashMap, AHashSet};
use anyhow::Context;
use async_trait::async_trait;
use longbridge::{
    Error as LongbridgeError,
    trade::{
        CancelOrderOptions, EstimateMaxPurchaseQuantityOptions, Execution,
        GetHistoryExecutionsOptions, GetHistoryOrdersOptions, GetStockPositionsOptions,
        GetTodayExecutionsOptions, GetTodayOrdersOptions, Order, OrderSide as LongbridgeOrderSide,
        OutsideRTH, PushEvent, ReplaceOrderOptions, SubmitOrderOptions, TopicType, TradeContext,
    },
};
use nautilus_common::{
    clients::ExecutionClient,
    live::{get_runtime, runner::get_exec_event_sender, task::TaskHandles},
    messages::execution::{
        CancelAllOrders, CancelOrder, GenerateFillReports, GenerateOrderStatusReport,
        GenerateOrderStatusReports, GeneratePositionStatusReports, ModifyOrder, QueryAccount,
        QueryOrder, SubmitOrder,
    },
};
use nautilus_core::{
    MUTEX_POISONED, Params, UnixNanos,
    time::{AtomicTime, get_atomic_clock_realtime},
};
use nautilus_live::{ExecutionClientCore, ExecutionEventEmitter};
use nautilus_model::{
    accounts::AccountAny,
    enums::{OmsType, OrderSide},
    identifiers::{
        AccountId, ClientId, ClientOrderId, InstrumentId, StrategyId, Venue, VenueOrderId,
    },
    orders::Order as NautilusOrder,
    reports::{FillReport, OrderStatusReport, PositionStatusReport},
    types::{AccountBalance, MarginBalance},
};
use rust_decimal::Decimal;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::{
    common::{
        consts::LONGBRIDGE_VENUE,
        parse::{
            parse_account_state, parse_fill_report, parse_order_status_report,
            parse_position_status_report, to_longbridge_order_side, to_longbridge_order_type,
            to_longbridge_time_in_force,
        },
        rate_limit::trade_api_call,
    },
    config::LongbridgeExecClientConfig,
};

#[derive(Clone, Debug)]
struct OrderContext {
    client_order_id: ClientOrderId,
    strategy_id: StrategyId,
    instrument_id: InstrumentId,
    order_side: OrderSide,
}

#[derive(Debug, Default)]
struct OrderContexts {
    by_client: AHashMap<String, OrderContext>,
    by_venue: AHashMap<String, OrderContext>,
    client_order: VecDeque<String>,
    venue_order: VecDeque<String>,
    short_preflights: AHashMap<ClientOrderId, bool>,
    pending_queries: AHashSet<ClientOrderId>,
}

struct OrderQueryGuard {
    contexts: Arc<Mutex<OrderContexts>>,
    id: ClientOrderId,
}

impl OrderQueryGuard {
    fn acquire(contexts: &Arc<Mutex<OrderContexts>>, id: ClientOrderId) -> Option<Self> {
        contexts
            .lock()
            .expect(MUTEX_POISONED)
            .pending_queries
            .insert(id)
            .then(|| Self {
                contexts: Arc::clone(contexts),
                id,
            })
    }
}

impl Drop for OrderQueryGuard {
    fn drop(&mut self) {
        // 限流等待、查询失败或任务被取消，都必须释放查询许可；订单资金预留不受影响
        self.contexts
            .lock()
            .expect(MUTEX_POISONED)
            .pending_queries
            .remove(&self.id);
    }
}

impl OrderContexts {
    const CAPACITY: usize = 10_000;

    fn cancel_preflight(&mut self, id: ClientOrderId) -> bool {
        if let Some(canceled) = self.short_preflights.get_mut(&id) {
            *canceled = true;
            true
        } else {
            false
        }
    }

    fn insert_client(&mut self, context: OrderContext) {
        let key = context.client_order_id.to_string();
        if !self.by_client.contains_key(&key) {
            if self.by_client.len() >= Self::CAPACITY
                && let Some(oldest) = self.client_order.pop_front()
            {
                self.by_client.remove(&oldest);
                self.by_venue
                    .retain(|_, value| value.client_order_id.to_string() != oldest);
            }
            self.client_order.push_back(key.clone());
        }
        self.by_client.insert(key, context);
    }

    fn associate_venue(&mut self, venue_order_id: &str, context: OrderContext) {
        if !self.by_venue.contains_key(venue_order_id) {
            if self.by_venue.len() >= Self::CAPACITY
                && let Some(oldest) = self.venue_order.pop_front()
            {
                self.by_venue.remove(&oldest);
            }
            self.venue_order.push_back(venue_order_id.to_string());
        }
        self.by_venue.insert(venue_order_id.to_string(), context);
    }

    fn for_order(&mut self, order: &Order) -> anyhow::Result<Option<OrderContext>> {
        if let Some(context) = self.by_venue.get(&order.order_id) {
            validate_order_identity(order, context)?;
            return Ok(Some(context.clone()));
        }

        if !order.remark.is_empty()
            && let Some(context) = self.by_client.get(&order.remark).cloned()
        {
            validate_order_identity(order, &context)?;
            self.associate_venue(&order.order_id, context.clone());
            return Ok(Some(context));
        }
        Ok(None)
    }
}

/// Longbridge live execution client.
pub struct LongbridgeExecutionClient {
    core: ExecutionClientCore,
    config: LongbridgeExecClientConfig,
    context: Option<TradeContext>,
    emitter: ExecutionEventEmitter,
    stream_handle: Option<JoinHandle<()>>,
    pending_tasks: TaskHandles,
    order_contexts: Arc<Mutex<OrderContexts>>,
    clock: &'static AtomicTime,
}

impl Debug for LongbridgeExecutionClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(LongbridgeExecutionClient))
            .field("client_id", &self.core.client_id)
            .field("account_id", &self.core.account_id)
            .field("config", &self.config)
            .field("has_context", &self.context.is_some())
            .field("connected", &self.core.is_connected())
            .finish_non_exhaustive()
    }
}

impl LongbridgeExecutionClient {
    /// Creates a new client without opening a network connection.
    #[must_use]
    pub fn new(core: ExecutionClientCore, config: LongbridgeExecClientConfig) -> Self {
        let clock = get_atomic_clock_realtime();
        let emitter = ExecutionEventEmitter::new(
            clock,
            core.trader_id,
            core.account_id,
            core.account_type,
            None,
        );
        Self {
            core,
            config,
            context: None,
            emitter,
            stream_handle: None,
            pending_tasks: TaskHandles::default(),
            order_contexts: Arc::new(Mutex::new(OrderContexts::default())),
            clock,
        }
    }

    fn context(&self) -> anyhow::Result<TradeContext> {
        self.context
            .clone()
            .context("Longbridge execution client is not connected")
    }

    fn spawn_task<F>(&self, description: &'static str, future: F)
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let handle = get_runtime().spawn(async move {
            if let Err(e) = future.await {
                log::warn!("Longbridge {description} failed: {e:#}");
            }
        });
        self.pending_tasks.push(handle);
    }

    fn client_order_id_for(&self, order: &Order) -> anyhow::Result<Option<ClientOrderId>> {
        // 重启后只认本地检查点里的订单，不把任意 broker remark 当作本策略订单
        if let Ok(id) = ClientOrderId::new_checked(&order.remark)
            && let Some(cached) = self.core.cache().order(&id)
            && cached
                .account_id()
                .is_none_or(|id| id == self.core.account_id)
        {
            self.order_contexts
                .lock()
                .expect(MUTEX_POISONED)
                .insert_client(OrderContext {
                    client_order_id: id,
                    strategy_id: cached.strategy_id(),
                    instrument_id: cached.instrument_id(),
                    order_side: cached.order_side(),
                });
        }
        self.order_contexts
            .lock()
            .expect(MUTEX_POISONED)
            .for_order(order)
            .map(|context| context.map(|context| context.client_order_id))
    }

    fn terminate(&mut self) {
        self.pending_tasks.abort_all();

        if let Some(handle) = self.stream_handle.take() {
            handle.abort();
        }
        self.context = None;
        self.core.set_disconnected();
    }

    async fn await_account_registered(&self, timeout_secs: f64) -> anyhow::Result<()> {
        let account_id = self.core.account_id;
        if self.core.cache().account(&account_id).is_some() {
            return Ok(());
        }

        let start = Instant::now();
        let timeout = Duration::from_secs_f64(timeout_secs);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;

            if self.core.cache().account(&account_id).is_some() {
                return Ok(());
            }

            if start.elapsed() >= timeout {
                anyhow::bail!(
                    "Timeout waiting for Longbridge account {account_id} to be registered after {timeout_secs}s",
                );
            }
        }
    }
}

fn offset_datetime(timestamp: UnixNanos) -> anyhow::Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp.as_u64()))
        .context("invalid Nautilus timestamp for Longbridge query")
}

fn order_side_from_sdk(side: LongbridgeOrderSide) -> anyhow::Result<OrderSide> {
    match side {
        LongbridgeOrderSide::Buy => Ok(OrderSide::Buy),
        LongbridgeOrderSide::Sell => Ok(OrderSide::Sell),
        LongbridgeOrderSide::Unknown => anyhow::bail!("Longbridge returned unknown order side"),
    }
}

fn is_authoritative_rejection(error: &LongbridgeError) -> bool {
    // 服务端内部错误不证明订单未到达券商；保留 Submitted 和资金预留等待对账
    error
        .openapi_error_code()
        .is_some_and(|code| code != 500_000)
}

fn validate_order_identity(order: &Order, context: &OrderContext) -> anyhow::Result<()> {
    anyhow::ensure!(
        crate::common::parse::instrument_id(&order.symbol) == context.instrument_id
            && order_side_from_sdk(order.side)? == context.order_side,
        "Longbridge order identity mismatch for {} / {}",
        context.strategy_id,
        context.client_order_id,
    );
    Ok(())
}

fn select_order(
    orders: Vec<Order>,
    client_order_id: Option<ClientOrderId>,
    venue_order_id: Option<VenueOrderId>,
) -> anyhow::Result<Order> {
    anyhow::ensure!(
        client_order_id.is_some() || venue_order_id.is_some(),
        "Missing order identity"
    );
    let mut matches = orders.into_iter().filter(|order| {
        venue_order_id.map_or_else(
            || client_order_id.is_some_and(|id| order.remark == id.as_str()),
            |id| order.order_id == id.as_str(),
        )
    });
    // 查不到不等于拒单：不能向原生 missing-order 逻辑返回 None 并释放预留
    let order = matches
        .next()
        .context("Longbridge order outcome unresolved; retain reservations")?;
    anyhow::ensure!(
        matches.next().is_none(),
        "Conflicting Longbridge order identities"
    );
    // 已知券商号优先：手工订单的 remark 不一定等于 Nautilus 为其分配的客户订单号
    Ok(order)
}

fn order_with_fills(
    order: &Order,
    executions: Vec<Execution>,
    account_id: AccountId,
    client_order_id: Option<ClientOrderId>,
    now: UnixNanos,
) -> anyhow::Result<(OrderStatusReport, Vec<FillReport>)> {
    let report = parse_order_status_report(order, account_id, client_order_id, now)?;
    let mut fills = Vec::new();
    let mut seen = AHashSet::new();
    for execution in executions
        .into_iter()
        .filter(|e| e.order_id == order.order_id)
    {
        anyhow::ensure!(
            execution.symbol == order.symbol,
            "Longbridge execution symbol mismatch"
        );
        if seen.insert(execution.trade_id.clone()) {
            fills.push(parse_fill_report(
                &execution,
                account_id,
                order_side_from_sdk(order.side)?,
                &order.currency,
                client_order_id,
                now,
            )?);
        }
    }
    anyhow::ensure!(
        fills
            .iter()
            .map(|fill| fill.last_qty.as_decimal())
            .sum::<Decimal>()
            == order.executed_quantity,
        "Longbridge executions and order snapshot disagree; defer reconciliation"
    );
    fills.sort_by_key(|fill| fill.ts_event);
    // 原生 OrderWithFills 原子对账并按 TradeId 去重，避免先推累计状态而生成推算成交
    Ok((report, fills))
}

async fn reconcile_order(
    context: &TradeContext,
    order: &Order,
    account_id: AccountId,
    client_order_id: Option<ClientOrderId>,
    emitter: &ExecutionEventEmitter,
    now: UnixNanos,
) -> anyhow::Result<()> {
    let executions = if order.executed_quantity.is_zero() {
        Vec::new()
    } else {
        fetch_executions(
            context,
            Some(crate::common::parse::instrument_id(&order.symbol)),
            None,
            None,
        )
        .await?
    };
    let (report, fills) = order_with_fills(order, executions, account_id, client_order_id, now)?;
    emitter.send_order_with_fills(report, fills);
    Ok(())
}

async fn fetch_orders(
    context: &TradeContext,
    instrument_id: Option<InstrumentId>,
    start: Option<UnixNanos>,
    end: Option<UnixNanos>,
    open_only: bool,
) -> anyhow::Result<Vec<Order>> {
    let symbol = instrument_id.map(|id| id.symbol.as_str().to_string());
    let mut today_options = GetTodayOrdersOptions::new();
    if let Some(symbol) = &symbol {
        today_options = today_options.symbol(symbol);
    }
    let mut orders = trade_api_call(context.today_orders(today_options)).await?;

    if !open_only {
        let mut history_options = GetHistoryOrdersOptions::new();
        if let Some(symbol) = &symbol {
            history_options = history_options.symbol(symbol);
        }

        if let Some(start) = start {
            history_options = history_options.start_at(offset_datetime(start)?);
        }

        if let Some(end) = end {
            history_options = history_options.end_at(offset_datetime(end)?);
        }
        orders.extend(trade_api_call(context.history_orders(history_options)).await?);
    }

    let mut seen = AHashSet::new();
    orders.retain(|order| seen.insert(order.order_id.clone()));
    if open_only {
        orders.retain(|order| {
            matches!(
                order.status,
                longbridge::trade::OrderStatus::NotReported
                    | longbridge::trade::OrderStatus::ReplacedNotReported
                    | longbridge::trade::OrderStatus::ProtectedNotReported
                    | longbridge::trade::OrderStatus::VarietiesNotReported
                    | longbridge::trade::OrderStatus::WaitToNew
                    | longbridge::trade::OrderStatus::New
                    | longbridge::trade::OrderStatus::WaitToReplace
                    | longbridge::trade::OrderStatus::PendingReplace
                    | longbridge::trade::OrderStatus::Replaced
                    | longbridge::trade::OrderStatus::PartialFilled
                    | longbridge::trade::OrderStatus::WaitToCancel
                    | longbridge::trade::OrderStatus::PendingCancel
            )
        });
    }
    Ok(orders)
}

async fn fetch_executions(
    context: &TradeContext,
    instrument_id: Option<InstrumentId>,
    start: Option<UnixNanos>,
    end: Option<UnixNanos>,
) -> anyhow::Result<Vec<Execution>> {
    let symbol = instrument_id.map(|id| id.symbol.as_str().to_string());
    let mut today_options = GetTodayExecutionsOptions::new();
    if let Some(symbol) = &symbol {
        today_options = today_options.symbol(symbol);
    }
    let mut executions = trade_api_call(context.today_executions(today_options)).await?;

    let mut history_options = GetHistoryExecutionsOptions::new();
    if let Some(symbol) = &symbol {
        history_options = history_options.symbol(symbol);
    }

    if let Some(start) = start {
        history_options = history_options.start_at(offset_datetime(start)?);
    }

    if let Some(end) = end {
        history_options = history_options.end_at(offset_datetime(end)?);
    }
    executions.extend(trade_api_call(context.history_executions(history_options)).await?);

    let mut seen = AHashSet::new();
    executions.retain(|execution| seen.insert(execution.trade_id.clone()));
    Ok(executions)
}

#[async_trait(?Send)]
impl ExecutionClient for LongbridgeExecutionClient {
    fn is_connected(&self) -> bool {
        self.core.is_connected()
    }

    fn client_id(&self) -> ClientId {
        self.core.client_id
    }

    fn account_id(&self) -> AccountId {
        self.core.account_id
    }

    fn venue(&self) -> Venue {
        *LONGBRIDGE_VENUE
    }

    fn oms_type(&self) -> OmsType {
        self.core.oms_type
    }

    fn get_account(&self) -> Option<AccountAny> {
        self.core.cache().account_owned(&self.core.account_id)
    }

    fn generate_account_state(
        &self,
        balances: Vec<AccountBalance>,
        margins: Vec<MarginBalance>,
        reported: bool,
        ts_event: UnixNanos,
        info: Option<Params>,
    ) -> anyhow::Result<()> {
        self.emitter
            .try_emit_account_state(balances, margins, reported, ts_event, info)
    }

    fn start(&mut self) -> anyhow::Result<()> {
        if self.core.is_started() {
            return Ok(());
        }
        self.emitter.set_sender(get_exec_event_sender());
        self.core.set_started();
        log::info!(
            "Started Longbridge execution client {} for {}",
            self.core.client_id,
            self.core.account_id,
        );
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if self.core.is_stopped() {
            return Ok(());
        }
        self.terminate();
        self.core.set_stopped();
        Ok(())
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.terminate();
        self.order_contexts = Arc::new(Mutex::new(OrderContexts::default()));
        Ok(())
    }

    fn dispose(&mut self) -> anyhow::Result<()> {
        self.stop()
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        if self.core.is_connected() {
            return Ok(());
        }
        let sdk_config = self.config.sdk_config().await?;
        let (context, mut receiver) = TradeContext::new(sdk_config);
        trade_api_call(context.subscribe([TopicType::Private]))
            .await
            .context("failed to subscribe to Longbridge private trade stream")?;

        let account_balances = trade_api_call(context.account_balance(None)).await?;
        let (balances, margins) = parse_account_state(&account_balances)?;
        self.emitter.try_emit_account_state(
            balances,
            margins,
            true,
            self.clock.get_time_ns(),
            None,
        )?;
        self.await_account_registered(30.0).await?;

        let task_context = context.clone();
        let emitter = self.emitter.clone();
        let contexts = Arc::clone(&self.order_contexts);
        let account_id = self.core.account_id;
        let clock = self.clock;

        self.stream_handle = Some(get_runtime().spawn(async move {
            while let Some(event) = receiver.recv().await {
                let PushEvent::OrderChanged(update) = event;
                let options = GetTodayOrdersOptions::new().order_id(update.order_id.clone());
                let order = match trade_api_call(task_context.today_orders(options)).await {
                    Ok(orders) => orders
                        .into_iter()
                        .find(|order| order.order_id == update.order_id),
                    Err(e) => {
                        log::warn!(
                            "Failed to refresh Longbridge pushed order {}: {e}",
                            update.order_id
                        );
                        None
                    }
                };
                let Some(order) = order else {
                    log::warn!(
                        "Longbridge push referenced unavailable order {}",
                        update.order_id
                    );
                    continue;
                };

                let local = contexts.lock().expect(MUTEX_POISONED).for_order(&order);
                let client_order_id = match local {
                    Ok(local) => local.map(|context| context.client_order_id),
                    Err(e) => {
                        log::error!("Refusing mismatched Longbridge push: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = reconcile_order(
                    &task_context,
                    &order,
                    account_id,
                    client_order_id,
                    &emitter,
                    clock.get_time_ns(),
                )
                .await
                {
                    log::warn!("Longbridge push reconciliation deferred: {e:#}");
                }
            }
        }));
        self.context = Some(context);
        self.core.set_connected();
        log::info!(
            "Connected Longbridge execution client {}",
            self.core.client_id
        );
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.terminate();
        log::info!(
            "Disconnected Longbridge execution client {}",
            self.core.client_id
        );
        Ok(())
    }

    fn query_account(&self, _cmd: QueryAccount) -> anyhow::Result<()> {
        let context = self.context()?;
        let emitter = self.emitter.clone();
        let clock = self.clock;
        self.spawn_task("account query", async move {
            let response = trade_api_call(context.account_balance(None)).await?;
            let (balances, margins) = parse_account_state(&response)?;
            emitter.try_emit_account_state(balances, margins, true, clock.get_time_ns(), None)?;
            Ok(())
        });
        Ok(())
    }

    fn query_order(&self, cmd: QueryOrder) -> anyhow::Result<()> {
        let context = self.context()?;
        let emitter = self.emitter.clone();
        let account_id = self.core.account_id;
        let client_order_id = cmd.client_order_id;
        let venue_order_id = cmd.venue_order_id;
        let clock = self.clock;
        let contexts = Arc::clone(&self.order_contexts);
        let cached = self.core.cache().try_order_owned(&client_order_id)?;
        let local = OrderContext {
            client_order_id,
            strategy_id: cached.strategy_id(),
            instrument_id: cached.instrument_id(),
            order_side: cached.order_side(),
        };
        let Some(query_guard) = OrderQueryGuard::acquire(&contexts, client_order_id) else {
            return Ok(());
        };
        self.spawn_task("order query", async move {
            let _query_guard = query_guard;
            let order = select_order(
                fetch_orders(&context, Some(cmd.instrument_id), None, None, false).await?,
                Some(client_order_id),
                venue_order_id,
            )?;
            validate_order_identity(&order, &local)?;
            {
                let mut contexts = contexts.lock().expect(MUTEX_POISONED);
                contexts.insert_client(local.clone());
                contexts.associate_venue(&order.order_id, local);
            }
            reconcile_order(
                &context,
                &order,
                account_id,
                Some(client_order_id),
                &emitter,
                clock.get_time_ns(),
            )
            .await?;
            Ok(())
        });
        Ok(())
    }

    fn submit_order(&self, cmd: SubmitOrder) -> anyhow::Result<()> {
        let order = self.core.cache().try_order_owned(&cmd.client_order_id)?;
        if order.is_closed() {
            log::warn!(
                "Cannot submit closed Longbridge order {}",
                order.client_order_id()
            );
            return Ok(());
        }

        if order.is_post_only() {
            self.emitter.emit_order_denied(
                &order,
                "Longbridge adapter does not support post-only orders",
            );
            return Ok(());
        }

        if order.is_reduce_only() {
            self.emitter
                .emit_order_denied(&order, "Longbridge stock orders do not support reduce-only");
            return Ok(());
        }

        let order_type = match to_longbridge_order_type(order.order_type()) {
            Ok(value) => value,
            Err(e) => {
                self.emitter.emit_order_denied(&order, &e.to_string());
                return Ok(());
            }
        };
        let side = match to_longbridge_order_side(order.order_side()) {
            Ok(value) => value,
            Err(e) => {
                self.emitter.emit_order_denied(&order, &e.to_string());
                return Ok(());
            }
        };
        let time_in_force = match to_longbridge_time_in_force(order.time_in_force()) {
            Ok(value) => value,
            Err(e) => {
                self.emitter.emit_order_denied(&order, &e.to_string());
                return Ok(());
            }
        };

        let client_order_id = order.client_order_id();
        let mut options = SubmitOrderOptions::new(
            order.instrument_id().symbol.as_str(),
            order_type,
            side,
            order.quantity().as_decimal(),
            time_in_force,
        )
        .client_request_id(client_order_id.to_string())
        .remark(client_order_id.to_string());

        if let Some(price) = order.price() {
            options = options.submitted_price(price.as_decimal());
        }

        if let Some(trigger_price) = order.trigger_price() {
            options = options.trigger_price(trigger_price.as_decimal());
        }

        if let Some(expire_time) = order.expire_time() {
            let expire_date = match offset_datetime(expire_time) {
                Ok(value) => value.date(),
                Err(e) => {
                    self.emitter.emit_order_denied(&order, &e.to_string());
                    return Ok(());
                }
            };
            options = options.expire_date(expire_date);
        }

        if self.config.outside_rth {
            options = options.outside_rth(OutsideRTH::AnyTime);
        }

        let context = self.context()?;
        let local_context = OrderContext {
            client_order_id,
            strategy_id: order.strategy_id(),
            instrument_id: order.instrument_id(),
            order_side: order.order_side(),
        };
        self.order_contexts
            .lock()
            .expect(MUTEX_POISONED)
            .insert_client(local_context.clone());
        let short_entry = order
            .tags()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == "SHORT_ENTRY"));
        if short_entry {
            self.order_contexts
                .lock()
                .expect(MUTEX_POISONED)
                .short_preflights
                .insert(client_order_id, false);
        }
        self.emitter.emit_order_submitted(&order);

        let contexts = Arc::clone(&self.order_contexts);
        let emitter = self.emitter.clone();
        let clock = self.clock;
        self.spawn_task("order submission", async move {
            // A cancel received while awaiting short capacity must prevent dispatch.
            if short_entry {
                let mut estimate = EstimateMaxPurchaseQuantityOptions::new(
                    order.instrument_id().symbol.as_str(),
                    order_type,
                    LongbridgeOrderSide::Sell,
                );
                if let Some(price) = order.price() {
                    estimate = estimate.price(price.as_decimal());
                }
                let capacity = tokio::time::timeout(
                    Duration::from_secs(15),
                    trade_api_call(context.estimate_max_purchase_quantity(estimate)),
                )
                .await;
                let failure = match capacity {
                    Ok(Ok(value)) if order.order_side() == OrderSide::Sell
                        && value.margin_max_qty >= order.quantity().as_decimal() => None,
                    Ok(Ok(_)) => Some("Insufficient short capacity or invalid order side".to_string()),
                    Ok(Err(e)) => Some(format!("Short capacity could not be verified: {e}")),
                    Err(_) => Some("Short capacity check timed out".to_string()),
                };
                if let Some(reason) = failure {
                    contexts
                        .lock()
                        .expect(MUTEX_POISONED)
                        .short_preflights
                        .remove(&client_order_id);
                    emitter.emit_order_rejected_event(
                        order.strategy_id(), order.instrument_id(), client_order_id,
                        &reason, clock.get_time_ns(), false,
                    );
                    return Ok(());
                }
            }
            let result = trade_api_call(async {
                // Check after the submit rate limiter, immediately before transport.
                let canceled = contexts
                    .lock()
                    .expect(MUTEX_POISONED)
                    .short_preflights
                    .remove(&client_order_id)
                    .unwrap_or(false);
                if canceled {
                    return Ok(None);
                }
                context.submit_order(options).await.map(Some)
            })
            .await;
            match result {
                Ok(None) => {
                    emitter.emit_order_rejected_event(
                        order.strategy_id(), order.instrument_id(), client_order_id,
                        "Short entry canceled before broker submission", clock.get_time_ns(), false,
                    );
                }
                Ok(Some(response)) => {
                    let venue_order_id = VenueOrderId::from(response.order_id.as_str());
                    contexts
                        .lock()
                        .expect(MUTEX_POISONED)
                        .associate_venue(&response.order_id, local_context);
                    emitter.emit_order_accepted(&order, venue_order_id, clock.get_time_ns());
                }
                Err(e) if is_authoritative_rejection(&e) => {
                    emitter.emit_order_rejected_event(
                        order.strategy_id(),
                        order.instrument_id(),
                        order.client_order_id(),
                        &format!("Longbridge rejected order: {e}"),
                        clock.get_time_ns(),
                        false,
                    );
                }
                Err(e) => {
                    log::error!(
                        "Ambiguous Longbridge submit outcome for {}: {e}; reconcile before retrying",
                        order.client_order_id(),
                    );
                }
            }
            Ok(())
        });
        Ok(())
    }

    fn modify_order(&self, cmd: ModifyOrder) -> anyhow::Result<()> {
        let Some(venue_order_id) = cmd.venue_order_id else {
            self.emitter.emit_order_modify_rejected_event(
                cmd.strategy_id,
                cmd.instrument_id,
                cmd.client_order_id,
                None,
                "Longbridge modify requires venue_order_id",
                self.clock.get_time_ns(),
            );
            return Ok(());
        };
        let cache = self.core.cache();
        let cached_order = cache.order(&cmd.client_order_id);
        let quantity = cmd
            .quantity
            .or_else(|| cached_order.as_ref().map(|order| order.quantity()));
        let Some(quantity) = quantity else {
            self.emitter.emit_order_modify_rejected_event(
                cmd.strategy_id,
                cmd.instrument_id,
                cmd.client_order_id,
                Some(venue_order_id),
                "Longbridge modify requires quantity or a cached order",
                self.clock.get_time_ns(),
            );
            return Ok(());
        };
        let mut options =
            ReplaceOrderOptions::new(venue_order_id.to_string(), quantity.as_decimal());
        if let Some(price) = cmd.price {
            options = options.price(price.as_decimal());
        }

        if let Some(trigger_price) = cmd.trigger_price {
            options = options.trigger_price(trigger_price.as_decimal());
        }
        let context = self.context()?;
        let emitter = self.emitter.clone();
        let clock = self.clock;
        self.spawn_task("order modification", async move {
            if let Err(e) = trade_api_call(context.replace_order(options)).await {
                if is_authoritative_rejection(&e) {
                    emitter.emit_order_modify_rejected_event(
                        cmd.strategy_id,
                        cmd.instrument_id,
                        cmd.client_order_id,
                        Some(venue_order_id),
                        &format!("Longbridge rejected modification: {e}"),
                        clock.get_time_ns(),
                    );
                } else {
                    log::error!(
                        "Ambiguous Longbridge modify outcome for {}: {e}; reconcile before retrying",
                        cmd.client_order_id,
                    );
                }
            }
            Ok(())
        });
        Ok(())
    }

    fn cancel_order(&self, cmd: CancelOrder) -> anyhow::Result<()> {
        if self
            .order_contexts
            .lock()
            .expect(MUTEX_POISONED)
            .cancel_preflight(cmd.client_order_id)
        {
            return Ok(());
        }
        let Some(venue_order_id) = cmd.venue_order_id else {
            self.emitter.emit_order_cancel_rejected_event(
                cmd.strategy_id,
                cmd.instrument_id,
                cmd.client_order_id,
                None,
                "Longbridge cancel requires venue_order_id",
                self.clock.get_time_ns(),
            );
            return Ok(());
        };
        let context = self.context()?;
        let emitter = self.emitter.clone();
        let clock = self.clock;
        self.spawn_task("order cancellation", async move {
            if let Err(e) = trade_api_call(
                context.cancel_order(CancelOrderOptions::new(venue_order_id.to_string())),
            )
            .await
            {
                if is_authoritative_rejection(&e) {
                    emitter.emit_order_cancel_rejected_event(
                        cmd.strategy_id,
                        cmd.instrument_id,
                        cmd.client_order_id,
                        Some(venue_order_id),
                        &format!("Longbridge rejected cancellation: {e}"),
                        clock.get_time_ns(),
                    );
                } else {
                    log::error!(
                        "Ambiguous Longbridge cancel outcome for {}: {e}; reconcile before retrying",
                        cmd.client_order_id,
                    );
                }
            }
            Ok(())
        });
        Ok(())
    }

    fn cancel_all_orders(&self, cmd: CancelAllOrders) -> anyhow::Result<()> {
        let context = self.context()?;
        let symbol = cmd.instrument_id.symbol.as_str().to_string();
        let requested_side = cmd.order_side;
        self.spawn_task("cancel all orders", async move {
            let orders =
                trade_api_call(context.today_orders(GetTodayOrdersOptions::new().symbol(symbol)))
                    .await?;

            for order in orders {
                if requested_side != OrderSide::NoOrderSide
                    && order_side_from_sdk(order.side)? != requested_side
                {
                    continue;
                }

                if matches!(
                    order.status,
                    longbridge::trade::OrderStatus::Filled
                        | longbridge::trade::OrderStatus::Rejected
                        | longbridge::trade::OrderStatus::Canceled
                        | longbridge::trade::OrderStatus::Expired
                        | longbridge::trade::OrderStatus::PartialWithdrawal
                ) {
                    continue;
                }
                trade_api_call(context.cancel_order(order.order_id)).await?;
            }
            Ok(())
        });
        Ok(())
    }

    async fn generate_order_status_report(
        &self,
        cmd: &GenerateOrderStatusReport,
    ) -> anyhow::Result<Option<OrderStatusReport>> {
        let context = self.context()?;
        let order = select_order(
            fetch_orders(&context, cmd.instrument_id, None, None, false).await?,
            cmd.client_order_id,
            cmd.venue_order_id,
        )?;
        let client_order_id = self.client_order_id_for(&order)?.or(cmd.client_order_id);
        parse_order_status_report(&order, self.core.account_id, client_order_id, cmd.ts_init)
            .map(Some)
    }

    async fn generate_order_status_reports(
        &self,
        cmd: &GenerateOrderStatusReports,
    ) -> anyhow::Result<Vec<OrderStatusReport>> {
        let context = self.context()?;
        fetch_orders(
            &context,
            cmd.instrument_id,
            cmd.start,
            cmd.end,
            cmd.open_only,
        )
        .await?
        .iter()
        .map(|order| {
            parse_order_status_report(
                order,
                self.core.account_id,
                self.client_order_id_for(order)?,
                cmd.ts_init,
            )
        })
        .collect()
    }

    async fn generate_fill_reports(
        &self,
        cmd: GenerateFillReports,
    ) -> anyhow::Result<Vec<FillReport>> {
        let context = self.context()?;
        // An execution in the requested window can belong to an order submitted before the
        // window, so fetch unbounded order metadata and apply time filters only to executions.
        let orders = fetch_orders(&context, cmd.instrument_id, None, None, false).await?;
        let order_by_id: AHashMap<&str, &Order> = orders
            .iter()
            .map(|order| (order.order_id.as_str(), order))
            .collect();
        let executions = fetch_executions(&context, cmd.instrument_id, cmd.start, cmd.end).await?;
        let mut reports = Vec::with_capacity(executions.len());
        for execution in executions {
            if cmd
                .venue_order_id
                .is_some_and(|venue_id| venue_id.as_str() != execution.order_id)
            {
                continue;
            }
            let order = order_by_id
                .get(execution.order_id.as_str())
                .with_context(|| {
                    format!(
                        "Longbridge execution {} has no matching order metadata",
                        execution.trade_id,
                    )
                })?;
            reports.push(parse_fill_report(
                &execution,
                self.core.account_id,
                order_side_from_sdk(order.side)?,
                &order.currency,
                self.client_order_id_for(order)?,
                cmd.ts_init,
            )?);
        }
        Ok(reports)
    }

    async fn generate_position_status_reports(
        &self,
        cmd: &GeneratePositionStatusReports,
    ) -> anyhow::Result<Vec<PositionStatusReport>> {
        let context = self.context()?;
        let options = cmd.instrument_id.map(|instrument_id| {
            GetStockPositionsOptions::new().symbols([instrument_id.symbol.as_str()])
        });
        trade_api_call(context.stock_positions(options))
            .await?
            .channels
            .iter()
            .flat_map(|channel| channel.positions.iter())
            .map(|position| {
                parse_position_status_report(position, self.core.account_id, cmd.ts_init)
            })
            .collect()
    }

    fn register_external_order(
        &self,
        client_order_id: ClientOrderId,
        venue_order_id: VenueOrderId,
        instrument_id: InstrumentId,
        strategy_id: StrategyId,
        _ts_init: UnixNanos,
    ) {
        let order_side = self
            .core
            .cache()
            .order(&client_order_id)
            .map_or(OrderSide::NoOrderSide, |order| order.order_side());
        let context = OrderContext {
            client_order_id,
            strategy_id,
            instrument_id,
            order_side,
        };
        let mut contexts = self.order_contexts.lock().expect(MUTEX_POISONED);
        contexts.insert_client(context.clone());
        contexts.associate_venue(venue_order_id.as_str(), context);
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use nautilus_common::{
        cache::Cache,
        clock::TestClock,
        messages::{ExecutionEvent, ExecutionReport},
    };
    use nautilus_core::UUID4;
    use nautilus_execution::engine::ExecutionEngine;
    use nautilus_model::{
        enums::{AccountType, OrderStatus, OrderType},
        identifiers::Symbol,
        instruments::{Equity, InstrumentAny},
        orders::builder::OrderTestBuilder,
        types::{Currency, Price, Quantity},
    };
    use rstest::rstest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn broker_order() -> Order {
        serde_json::from_str(include_str!("../test_data/order_reconciliation.json")).unwrap()
    }

    fn broker_fill(order: &Order) -> Execution {
        Execution {
            order_id: order.order_id.clone(),
            trade_id: "execution-1".into(),
            symbol: order.symbol.clone(),
            trade_done_at: order.updated_at.unwrap(),
            quantity: Decimal::ONE,
            price: Decimal::new(9990, 2),
        }
    }

    #[rstest]
    fn pending_query_is_coalesced_and_released_on_drop() {
        let contexts = Arc::new(Mutex::new(OrderContexts::default()));
        let id = ClientOrderId::from("DG-QUERY-1");
        let guard = OrderQueryGuard::acquire(&contexts, id).unwrap();
        assert!(OrderQueryGuard::acquire(&contexts, id).is_none());
        assert!(OrderQueryGuard::acquire(&contexts, ClientOrderId::from("DG-QUERY-2")).is_some());
        drop(guard);
        assert!(OrderQueryGuard::acquire(&contexts, id).is_some());
    }

    #[tokio::test]
    async fn sdk_recovers_ambiguous_submission_and_native_engine_deduplicates_fills() {
        // 仅测试传输使用本地 HTTP 服务，查询和解析走生产 Adapter 与官方 SDK
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let order_json: serde_json::Value =
            serde_json::from_str(include_str!("../test_data/order_reconciliation.json")).unwrap();
        let executions = serde_json::json!([{
            "order_id":"broker-42", "trade_id":"execution-1", "symbol":"AAPL.US",
            "trade_done_at":"1758547801", "quantity":"1", "price":"99.90"
        }]);
        let (submitted_at_broker, submission_seen) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut submitted_at_broker = Some(submitted_at_broker);
            let mut requests = Vec::new();
            for index in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut buffer = [0_u8; 4096];
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let header_end = bytes
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut buffer = [0_u8; 4096];
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = request.lines().next().unwrap().to_string();
                if index == 0 {
                    assert!(path.starts_with("POST /v1/trade/order"));
                    let submitted: serde_json::Value =
                        serde_json::from_str(&request[header_end..]).unwrap();
                    assert_eq!(submitted["client_request_id"], "DG-TEST-AAPL-1-B-1");
                    assert_eq!(submitted["remark"], submitted["client_request_id"]);
                    requests.push(path);
                    let body = r#"{"code":500000,"message":"unknown outcome","data":null}"#;
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    submitted_at_broker.take().unwrap().send(()).unwrap();
                    continue;
                }
                assert!(
                    path.starts_with("GET "),
                    "reconciliation must never resubmit"
                );
                let data = if path.contains("/order/today") {
                    serde_json::json!({"orders":[order_json.clone()]})
                } else if path.contains("/order/history") {
                    serde_json::json!({"orders":[]})
                } else if path.contains("/execution/today") {
                    serde_json::json!({"trades":executions.clone()})
                } else {
                    assert!(path.contains("/execution/history"));
                    serde_json::json!({"trades":[]})
                };
                requests.push(path);
                let body =
                    serde_json::json!({"code":0,"message":"success","data":data}).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let instrument_id = InstrumentId::from("AAPL.US.LONGBRIDGE");
        let id = ClientOrderId::from("DG-TEST-AAPL-1-B-1");
        let order = OrderTestBuilder::new(OrderType::Limit)
            .instrument_id(instrument_id)
            .client_order_id(id)
            .side(OrderSide::Buy)
            .quantity(Quantity::from(3))
            .price(Price::from("100.00"))
            .build();
        let cache = Rc::new(RefCell::new(Cache::default()));
        cache
            .borrow_mut()
            .add_order(order.clone(), None, None, false)
            .unwrap();
        let core = ExecutionClientCore::new(
            order.trader_id(),
            ClientId::from("LONGBRIDGE"),
            instrument_id.venue,
            OmsType::Netting,
            AccountId::from("LONGBRIDGE-001"),
            AccountType::Cash,
            Some(Currency::USD()),
            cache.clone(),
        );
        let mut client =
            LongbridgeExecutionClient::new(core, LongbridgeExecClientConfig::default());
        let sdk = longbridge::Config::from_apikey("test-key", "test-secret", "test-token")
            .http_url(format!("http://{address}"))
            .trade_ws_url(format!("ws://{address}/unused"));
        let (context, _pushes) = TradeContext::new(Arc::new(sdk));
        client.context = Some(context);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        client.emitter.set_sender(sender);
        client
            .submit_order(SubmitOrder::from_order(
                &order,
                order.trader_id(),
                None,
                None,
                UUID4::new(),
                0.into(),
            ))
            .unwrap();
        let submitted = receiver.recv().await.unwrap();
        let ExecutionEvent::Order(event) = submitted else {
            panic!("expected submitted event");
        };
        cache
            .borrow_mut()
            .order_mut(&id)
            .unwrap()
            .apply(event)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), submission_seen)
            .await
            .unwrap()
            .unwrap();
        client
            .query_order(QueryOrder::new(
                order.trader_id(),
                None,
                order.strategy_id(),
                instrument_id,
                id,
                None,
                UUID4::new(),
                0.into(),
                None,
                None,
            ))
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let ExecutionEvent::Report(report @ ExecutionReport::OrderWithFills(_, _)) = event else {
            panic!("expected an atomic order/fills report");
        };
        assert_eq!(server.await.unwrap().len(), 5);
        cache
            .borrow_mut()
            .add_instrument(InstrumentAny::Equity(
                Equity::builder()
                    .instrument_id(instrument_id)
                    .raw_symbol(Symbol::from("AAPL.US"))
                    .currency(Currency::USD())
                    .price_precision(2)
                    .price_increment(Price::from("0.01"))
                    .lot_size(Quantity::from(1))
                    .ts_event(0.into())
                    .ts_init(0.into())
                    .build()
                    .unwrap(),
            ))
            .unwrap();
        let mut engine =
            ExecutionEngine::new(Rc::new(RefCell::new(TestClock::new())), cache.clone(), None);
        engine.reconcile_execution_report(&report);
        engine.reconcile_execution_report(&report);
        let recovered = cache.borrow().order(&id).unwrap().clone();
        assert_eq!(recovered.status(), OrderStatus::PartiallyFilled);
        assert_eq!(recovered.filled_qty(), Quantity::from(1));
        assert_eq!(recovered.trade_ids().len(), 1);
        assert_eq!(
            recovered.venue_order_id(),
            Some(VenueOrderId::from("broker-42"))
        );

        let mut canceled = broker_order();
        canceled.status = longbridge::trade::OrderStatus::PartialWithdrawal;
        let (snapshot, fills) = order_with_fills(
            &canceled,
            vec![broker_fill(&canceled)],
            client.account_id(),
            Some(id),
            0.into(),
        )
        .unwrap();
        engine.reconcile_order_with_fills(&snapshot, &fills);
        engine.reconcile_order_with_fills(&snapshot, &fills);
        assert_eq!(
            cache.borrow().order(&id).unwrap().status(),
            OrderStatus::Canceled
        );
        assert_eq!(
            cache.borrow().order(&id).unwrap().filled_qty(),
            Quantity::from(1)
        );

        // 重启后的空映射仍从 native cache 找回客户身份，不导入另一策略的订单
        *client.order_contexts.lock().unwrap() = OrderContexts::default();
        assert_eq!(
            client.client_order_id_for(&broker_order()).unwrap(),
            Some(id)
        );
        client.terminate();
    }

    #[rstest]
    fn unresolved_submission_matches_remark_without_venue_id_and_rejects_ambiguity() {
        let order = broker_order();
        let id = ClientOrderId::from(order.remark.as_str());
        assert_eq!(
            select_order(vec![order.clone()], Some(id), None)
                .unwrap()
                .order_id,
            "broker-42"
        );
        assert!(select_order(Vec::new(), Some(id), None).is_err());
        let mut duplicate = order.clone();
        duplicate.order_id = "broker-43".into();
        assert!(select_order(vec![order.clone(), duplicate], Some(id), None).is_err());
        assert!(
            select_order(
                vec![order.clone()],
                Some(ClientOrderId::from("FOREIGN")),
                None
            )
            .is_err()
        );
        assert_eq!(
            select_order(
                vec![order],
                Some(ClientOrderId::from("EXTERNAL-1")),
                Some(VenueOrderId::from("broker-42"))
            )
            .unwrap()
            .order_id,
            "broker-42"
        );
    }

    #[rstest]
    fn reconciliation_validates_identity_before_associating_venue_id() {
        let mut order = broker_order();
        let id = ClientOrderId::from(order.remark.as_str());
        let mut contexts = OrderContexts::default();
        contexts.insert_client(OrderContext {
            client_order_id: id,
            strategy_id: StrategyId::from("GRID-001"),
            instrument_id: InstrumentId::from("AAPL.US.LONGBRIDGE"),
            order_side: OrderSide::Buy,
        });
        order.symbol = "MSFT.US".into();
        assert!(contexts.for_order(&order).is_err());
        assert!(contexts.by_venue.is_empty());
        order.symbol = "AAPL.US".into();
        order.side = LongbridgeOrderSide::Sell;
        assert!(contexts.for_order(&order).is_err());
        order.side = LongbridgeOrderSide::Buy;
        assert_eq!(
            contexts.for_order(&order).unwrap().unwrap().client_order_id,
            id
        );
    }

    #[rstest]
    fn partial_fill_snapshot_requires_consistent_executions_and_deduplicates() {
        let order = broker_order();
        let fill = broker_fill(&order);
        let account = AccountId::from("LONGBRIDGE-001");
        let id = Some(ClientOrderId::from(order.remark.as_str()));
        let (report, fills) = order_with_fills(
            &order,
            vec![fill.clone(), fill.clone()],
            account,
            id,
            0.into(),
        )
        .unwrap();
        assert_eq!(report.filled_qty.as_decimal(), Decimal::ONE);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].last_px.as_decimal(), Decimal::new(9990, 2));
        assert!(order_with_fills(&order, Vec::new(), account, id, 0.into()).is_err());
        let mut wrong = fill;
        wrong.symbol = "MSFT.US".into();
        assert!(order_with_fills(&order, vec![wrong], account, id, 0.into()).is_err());
    }

    #[rstest]
    #[case(500_000, false)]
    #[case(429_001, true)]
    #[case(429_002, true)]
    #[case(429_003, true)]
    fn server_errors_do_not_fabricate_rejections(#[case] code: i32, #[case] rejected: bool) {
        let error = LongbridgeError::HttpClient(longbridge::httpclient::HttpClientError::OpenApi {
            code,
            message: "test response".into(),
            trace_id: "test-trace".into(),
        });
        assert_eq!(is_authoritative_rejection(&error), rejected);
        assert!(!is_authoritative_rejection(&LongbridgeError::HttpClient(
            longbridge::httpclient::HttpClientError::RequestTimeout,
        )));
    }

    #[test]
    fn short_preflight_cancel_survives_until_dispatch() {
        let mut contexts = OrderContexts::default();
        let id = ClientOrderId::from("SHORT-ENTRY-1");
        assert!(!contexts.cancel_preflight(id));
        contexts.short_preflights.insert(id, false);
        assert!(contexts.cancel_preflight(id));
        assert!(contexts.cancel_preflight(id));
        assert_eq!(contexts.short_preflights.remove(&id), Some(true));
        assert!(!contexts.cancel_preflight(id));
        contexts.short_preflights.insert(id, false);
        assert_eq!(contexts.short_preflights.remove(&id), Some(false));
    }
}
