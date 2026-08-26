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
    fmt,
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
        CancelOrderOptions, Execution, GetHistoryExecutionsOptions, GetHistoryOrdersOptions,
        GetStockPositionsOptions, GetTodayExecutionsOptions, GetTodayOrdersOptions, Order,
        OrderSide as LongbridgeOrderSide, OutsideRTH, PushEvent, ReplaceOrderOptions,
        SubmitOrderOptions, TopicType, TradeContext,
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
}

impl OrderContexts {
    const CAPACITY: usize = 10_000;

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

    fn for_order(&mut self, order: &Order) -> Option<OrderContext> {
        if let Some(context) = self.by_venue.get(&order.order_id) {
            return Some(context.clone());
        }
        if !order.remark.is_empty()
            && let Some(context) = self.by_client.get(&order.remark).cloned()
        {
            self.associate_venue(&order.order_id, context.clone());
            return Some(context);
        }
        None
    }
}

#[derive(Debug, Default)]
struct SeenTradeIds {
    ids: AHashSet<String>,
    order: VecDeque<String>,
}

impl SeenTradeIds {
    const CAPACITY: usize = 10_000;

    fn insert(&mut self, trade_id: String) -> bool {
        if self.ids.contains(&trade_id) {
            return false;
        }
        if self.ids.len() >= Self::CAPACITY
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        self.order.push_back(trade_id.clone());
        self.ids.insert(trade_id)
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.order.clear();
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
    seen_trade_ids: Arc<Mutex<SeenTradeIds>>,
    clock: &'static AtomicTime,
}

impl fmt::Debug for LongbridgeExecutionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            seen_trade_ids: Arc::new(Mutex::new(SeenTradeIds::default())),
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
            if let Err(error) = future.await {
                log::warn!("Longbridge {description} failed: {error:#}");
            }
        });
        self.pending_tasks.push(handle);
    }

    fn client_order_id_for(&self, order: &Order) -> Option<ClientOrderId> {
        self.order_contexts
            .lock()
            .expect(MUTEX_POISONED)
            .for_order(order)
            .map(|context| context.client_order_id)
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
    error.openapi_error_code().is_some()
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
    let mut orders = context.today_orders(today_options).await?;

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
        orders.extend(context.history_orders(history_options).await?);
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
    let mut executions = context.today_executions(today_options).await?;

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
    executions.extend(context.history_executions(history_options).await?);

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
        *self.order_contexts.lock().expect(MUTEX_POISONED) = OrderContexts::default();
        self.seen_trade_ids.lock().expect(MUTEX_POISONED).clear();
        Ok(())
    }

    fn dispose(&mut self) -> anyhow::Result<()> {
        self.stop()
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        if self.core.is_connected() {
            return Ok(());
        }
        let sdk_config = self.config.sdk_config()?;
        let (context, mut receiver) = TradeContext::new(sdk_config);
        context
            .subscribe([TopicType::Private])
            .await
            .context("failed to subscribe to Longbridge private trade stream")?;

        let account_balances = context.account_balance(None).await?;
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
        let seen_trade_ids = Arc::clone(&self.seen_trade_ids);
        let account_id = self.core.account_id;
        let clock = self.clock;
        self.stream_handle = Some(get_runtime().spawn(async move {
            while let Some(event) = receiver.recv().await {
                let PushEvent::OrderChanged(update) = event;
                let options = GetTodayOrdersOptions::new().order_id(update.order_id.clone());
                let order = match task_context.today_orders(options).await {
                    Ok(orders) => orders.into_iter().find(|order| order.order_id == update.order_id),
                    Err(error) => {
                        log::warn!("Failed to refresh Longbridge pushed order {}: {error}", update.order_id);
                        None
                    }
                };
                let Some(order) = order else {
                    log::warn!("Longbridge push referenced unavailable order {}", update.order_id);
                    continue;
                };

                let client_context = contexts.lock().expect(MUTEX_POISONED).for_order(&order);
                if let Some(local) = &client_context {
                    let pushed_instrument = crate::common::parse::instrument_id(&order.symbol);
                    if local.instrument_id != pushed_instrument
                        || order_side_from_sdk(order.side).ok() != Some(local.order_side)
                    {
                        log::warn!(
                            "Longbridge order identity mismatch for strategy {} and client order {}",
                            local.strategy_id,
                            local.client_order_id,
                        );
                    }
                }
                let client_order_id = client_context.as_ref().map(|ctx| ctx.client_order_id);
                let ts_init = clock.get_time_ns();
                match parse_order_status_report(&order, account_id, client_order_id, ts_init) {
                    Ok(report) => emitter.send_order_status_report(report),
                    Err(error) => log::warn!("Failed to parse Longbridge order push: {error:#}"),
                }

                let execution_options = GetTodayExecutionsOptions::new().order_id(order.order_id.clone());
                match task_context.today_executions(execution_options).await {
                    Ok(executions) => {
                        for execution in executions {
                            let is_new = seen_trade_ids
                                .lock()
                                .expect(MUTEX_POISONED)
                                .insert(execution.trade_id.clone());
                            if !is_new {
                                continue;
                            }
                            let side = match order_side_from_sdk(order.side) {
                                Ok(side) => side,
                                Err(error) => {
                                    log::warn!("Failed to parse Longbridge fill side: {error:#}");
                                    continue;
                                }
                            };
                            match parse_fill_report(
                                &execution,
                                account_id,
                                side,
                                &order.currency,
                                client_order_id,
                                clock.get_time_ns(),
                            ) {
                                Ok(report) => emitter.send_fill_report(report),
                                Err(error) => log::warn!("Failed to parse Longbridge fill push: {error:#}"),
                            }
                        }
                    }
                    Err(error) => log::warn!(
                        "Failed to refresh executions for Longbridge order {}: {error}",
                        order.order_id,
                    ),
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
            let response = context.account_balance(None).await?;
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
        self.spawn_task("order query", async move {
            let Some(venue_order_id) = venue_order_id else {
                anyhow::bail!("Longbridge order query requires venue_order_id");
            };
            let order = fetch_orders(&context, Some(cmd.instrument_id), None, None, false)
                .await?
                .into_iter()
                .find(|order| order.order_id == venue_order_id.as_str())
                .context("Longbridge order was not found")?;
            let report = parse_order_status_report(
                &order,
                account_id,
                Some(client_order_id),
                clock.get_time_ns(),
            )?;
            emitter.send_order_status_report(report);
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
            Err(error) => {
                self.emitter.emit_order_denied(&order, &error.to_string());
                return Ok(());
            }
        };
        let side = match to_longbridge_order_side(order.order_side()) {
            Ok(value) => value,
            Err(error) => {
                self.emitter.emit_order_denied(&order, &error.to_string());
                return Ok(());
            }
        };
        let time_in_force = match to_longbridge_time_in_force(order.time_in_force()) {
            Ok(value) => value,
            Err(error) => {
                self.emitter.emit_order_denied(&order, &error.to_string());
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
                Err(error) => {
                    self.emitter.emit_order_denied(&order, &error.to_string());
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
        self.emitter.emit_order_submitted(&order);

        let contexts = Arc::clone(&self.order_contexts);
        let emitter = self.emitter.clone();
        let clock = self.clock;
        self.spawn_task("order submission", async move {
            match context.submit_order(options).await {
                Ok(response) => {
                    let venue_order_id = VenueOrderId::from(response.order_id.as_str());
                    contexts
                        .lock()
                        .expect(MUTEX_POISONED)
                        .associate_venue(&response.order_id, local_context);
                    emitter.emit_order_accepted(&order, venue_order_id, clock.get_time_ns());
                }
                Err(error) if is_authoritative_rejection(&error) => {
                    emitter.emit_order_rejected_event(
                        order.strategy_id(),
                        order.instrument_id(),
                        order.client_order_id(),
                        &format!("Longbridge rejected order: {error}"),
                        clock.get_time_ns(),
                        false,
                    );
                }
                Err(error) => {
                    log::error!(
                        "Ambiguous Longbridge submit outcome for {}: {error}; reconcile before retrying",
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
            if let Err(error) = context.replace_order(options).await {
                if is_authoritative_rejection(&error) {
                    emitter.emit_order_modify_rejected_event(
                        cmd.strategy_id,
                        cmd.instrument_id,
                        cmd.client_order_id,
                        Some(venue_order_id),
                        &format!("Longbridge rejected modification: {error}"),
                        clock.get_time_ns(),
                    );
                } else {
                    log::error!(
                        "Ambiguous Longbridge modify outcome for {}: {error}; reconcile before retrying",
                        cmd.client_order_id,
                    );
                }
            }
            Ok(())
        });
        Ok(())
    }

    fn cancel_order(&self, cmd: CancelOrder) -> anyhow::Result<()> {
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
            if let Err(error) = context
                .cancel_order(CancelOrderOptions::new(venue_order_id.to_string()))
                .await
            {
                if is_authoritative_rejection(&error) {
                    emitter.emit_order_cancel_rejected_event(
                        cmd.strategy_id,
                        cmd.instrument_id,
                        cmd.client_order_id,
                        Some(venue_order_id),
                        &format!("Longbridge rejected cancellation: {error}"),
                        clock.get_time_ns(),
                    );
                } else {
                    log::error!(
                        "Ambiguous Longbridge cancel outcome for {}: {error}; reconcile before retrying",
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
            let orders = context
                .today_orders(GetTodayOrdersOptions::new().symbol(symbol))
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
                context.cancel_order(order.order_id).await?;
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
        let venue_order_id = cmd.venue_order_id.or_else(|| {
            cmd.client_order_id.and_then(|client_order_id| {
                self.order_contexts
                    .lock()
                    .expect(MUTEX_POISONED)
                    .by_venue
                    .iter()
                    .find_map(|(venue_id, context)| {
                        (context.client_order_id == client_order_id)
                            .then(|| VenueOrderId::from(venue_id.as_str()))
                    })
            })
        });
        let Some(venue_order_id) = venue_order_id else {
            return Ok(None);
        };
        let order = fetch_orders(&context, cmd.instrument_id, None, None, false)
            .await?
            .into_iter()
            .find(|order| order.order_id == venue_order_id.as_str());
        order
            .as_ref()
            .map(|order| {
                parse_order_status_report(
                    order,
                    self.core.account_id,
                    cmd.client_order_id
                        .or_else(|| self.client_order_id_for(order)),
                    cmd.ts_init,
                )
            })
            .transpose()
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
                self.client_order_id_for(order),
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
                self.client_order_id_for(order),
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
        context
            .stock_positions(options)
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
