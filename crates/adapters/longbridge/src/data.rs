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

//! Live Longbridge market-data client backed by the official Rust SDK.

use std::{
    fmt::Debug,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::{AHashMap, AHashSet};
use anyhow::Context;
use async_trait::async_trait;
use jiff::Timestamp;
use longbridge::quote::{
    AdjustType, Period, PushEventDetail, QuoteContext, SubFlags, TradeSessions,
};
use nautilus_common::{
    clients::DataClient,
    live::{get_runtime, runner::get_data_event_sender, task::TaskHandles},
    messages::{
        DataEvent, DataResponse,
        data::{
            BarsResponse, InstrumentResponse, InstrumentsResponse, RequestBars, RequestInstrument,
            RequestInstruments, SubscribeBars, SubscribeBookDepth10, SubscribeInstrument,
            SubscribeInstruments, SubscribeQuotes, SubscribeTrades, UnsubscribeBars,
            UnsubscribeBookDepth10, UnsubscribeQuotes, UnsubscribeTrades,
        },
    },
};
use nautilus_core::{
    MUTEX_POISONED, UnixNanos,
    datetime::{datetime_to_unix_nanos, get_timezone},
    time::{AtomicTime, get_atomic_clock_realtime},
};
use nautilus_model::{
    data::{Bar, BarType, CustomData, Data},
    enums::AggregationSource,
    identifiers::{ClientId, InstrumentId, Venue},
    instruments::{Instrument, InstrumentAny},
};
use time::{Date, Month, PrimitiveDateTime, Time};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    common::{
        consts::LONGBRIDGE_VENUE,
        parse::{
            instrument_id, parse_bar_with_price_precision, parse_completed_minute_bar, parse_depth,
            parse_instrument, parse_trades, period_from_bar_type,
        },
        rate_limit::{
            MAX_QUOTE_SUBSCRIPTION_SYMBOLS, QuoteConnectionGuard, history_api_call_with_retry,
            quote_api_call_with_retry, try_acquire_quote_connection,
        },
    },
    config::LongbridgeDataClientConfig,
};

#[derive(Debug, Default)]
struct SubscriptionState {
    quotes: AHashSet<InstrumentId>,
    depth10: AHashSet<InstrumentId>,
    trades: AHashSet<InstrumentId>,
    bars: AHashMap<(String, i32), (BarType, u8)>,
    reserved_symbols: AHashSet<String>,
    applied_flags: AHashMap<String, SubFlags>,
    applied_bars: AHashMap<(String, i32), Period>,
}

impl SubscriptionState {
    fn release_acknowledged(&mut self, symbol: &str) {
        let id = instrument_id(symbol);
        if !self.quotes.contains(&id)
            && !self.depth10.contains(&id)
            && !self.trades.contains(&id)
            && !self.bars.keys().any(|(s, _)| s == symbol)
            && self
                .applied_flags
                .get(symbol)
                .is_none_or(SubFlags::is_empty)
            && !self.applied_bars.keys().any(|(s, _)| s == symbol)
        {
            self.reserved_symbols.remove(symbol);
            self.applied_flags.remove(symbol);
        }
    }

    fn reserve_subscription(&mut self, symbol: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.reserved_symbols.contains(symbol)
                || self.reserved_symbols.len() < MAX_QUOTE_SUBSCRIPTION_SYMBOLS,
            "Longbridge cannot subscribe to more than {MAX_QUOTE_SUBSCRIPTION_SYMBOLS} unique symbols",
        );
        self.reserved_symbols.insert(symbol.to_string());
        Ok(())
    }
}

/// Longbridge live data client.
pub struct LongbridgeDataClient {
    client_id: ClientId,
    config: LongbridgeDataClientConfig,
    context: Option<QuoteContext>,
    context_sender: Option<tokio::sync::watch::Sender<Option<QuoteContext>>>,
    subscription_lock: Arc<tokio::sync::Mutex<()>>,
    connection_guard: Option<QuoteConnectionGuard>,
    stream_handle: Option<JoinHandle<()>>,
    pending_tasks: TaskHandles,
    cancellation_token: CancellationToken,
    subscriptions: Arc<Mutex<SubscriptionState>>,
    data_sender: tokio::sync::mpsc::UnboundedSender<DataEvent>,
    clock: &'static AtomicTime,
    connected: AtomicBool,
}

impl Debug for LongbridgeDataClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(LongbridgeDataClient))
            .field("client_id", &self.client_id)
            .field("config", &self.config)
            .field("has_context", &self.context.is_some())
            .field("holds_quote_connection", &self.connection_guard.is_some())
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl LongbridgeDataClient {
    /// Creates a new client without opening a network connection.
    #[must_use]
    pub fn new(client_id: ClientId, config: LongbridgeDataClientConfig) -> Self {
        Self {
            client_id,
            config,
            context: None,
            context_sender: None,
            subscription_lock: Arc::new(tokio::sync::Mutex::new(())),
            connection_guard: None,
            stream_handle: None,
            pending_tasks: TaskHandles::default(),
            cancellation_token: CancellationToken::new(),
            subscriptions: Arc::new(Mutex::new(SubscriptionState::default())),
            data_sender: get_data_event_sender(),
            clock: get_atomic_clock_realtime(),
            connected: AtomicBool::new(false),
        }
    }

    /// Shares this client's single quote connection with a native market-data collector.
    #[must_use]
    pub fn with_quote_context(
        mut self,
        sender: tokio::sync::watch::Sender<Option<QuoteContext>>,
    ) -> Self {
        self.context_sender = Some(sender);
        self
    }

    fn reconcile_subscriptions(&self, symbol: String) -> anyhow::Result<()> {
        let context = self.context()?;
        let state = self.subscriptions.clone();
        let serial = self.subscription_lock.clone();
        self.spawn_result("subscription reconciliation", async move {
            // Serialize desired/applied changes, including quote/depth sharing and bar subscriptions.
            let _guard = serial.lock().await;
            let id = instrument_id(&symbol);
            let (wanted, applied, wanted_bars, applied_bars) = {
                let s = state.lock().expect(MUTEX_POISONED);
                let mut flags = SubFlags::empty();
                if s.quotes.contains(&id) || s.depth10.contains(&id) {
                    flags |= SubFlags::DEPTH;
                }
                if s.trades.contains(&id) {
                    flags |= SubFlags::TRADE;
                }
                (
                    flags,
                    s.applied_flags
                        .get(&symbol)
                        .copied()
                        .unwrap_or_else(SubFlags::empty),
                    s.bars
                        .iter()
                        .filter(|((name, _), _)| name == &symbol)
                        .map(|(key, (bar, _))| {
                            (
                                key.clone(),
                                period_from_bar_type(*bar).expect("validated period"),
                            )
                        })
                        .collect::<AHashMap<_, _>>(),
                    s.applied_bars
                        .iter()
                        .filter(|((name, _), _)| name == &symbol)
                        .map(|(k, v)| (k.clone(), *v))
                        .collect::<AHashMap<_, _>>(),
                )
            };
            let removed = applied & !wanted;
            if !removed.is_empty() {
                quote_api_call_with_retry(|| context.unsubscribe([symbol.clone()], removed))
                    .await?;
                state
                    .lock()
                    .expect(MUTEX_POISONED)
                    .applied_flags
                    .insert(symbol.clone(), applied & !removed);
            }
            for (key, period) in &applied_bars {
                if !wanted_bars.contains_key(key) {
                    quote_api_call_with_retry(|| {
                        context.unsubscribe_candlesticks(symbol.clone(), *period)
                    })
                    .await?;
                    state.lock().expect(MUTEX_POISONED).applied_bars.remove(key);
                }
            }
            let added = wanted & !applied;
            if !added.is_empty() {
                quote_api_call_with_retry(|| context.subscribe([symbol.clone()], added)).await?;
                state
                    .lock()
                    .expect(MUTEX_POISONED)
                    .applied_flags
                    .insert(symbol.clone(), wanted);
            }
            for (key, period) in &wanted_bars {
                if !applied_bars.contains_key(key) {
                    quote_api_call_with_retry(|| {
                        context.subscribe_candlesticks(symbol.clone(), *period, TradeSessions::All)
                    })
                    .await?;
                    state
                        .lock()
                        .expect(MUTEX_POISONED)
                        .applied_bars
                        .insert(key.clone(), *period);
                }
            }
            state
                .lock()
                .expect(MUTEX_POISONED)
                .release_acknowledged(&symbol);
            Ok(())
        });
        Ok(())
    }

    fn context(&self) -> anyhow::Result<QuoteContext> {
        self.context
            .clone()
            .context("Longbridge data client is not connected")
    }

    fn spawn_result<F>(&self, description: &'static str, future: F)
    where
        F: Future<Output = longbridge::Result<()>> + Send + 'static,
    {
        self.spawn_task(async move {
            if let Err(e) = future.await {
                log::error!("Longbridge {description} failed: {e}");
            }
        });
    }

    fn spawn_task<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.pending_tasks.push(get_runtime().spawn(future));
    }

    fn terminate_stream(&mut self) {
        self.cancellation_token.cancel();
        self.pending_tasks.abort_all();

        if let Some(handle) = self.stream_handle.take() {
            handle.abort();
        }
        if let Some(sender) = &self.context_sender {
            sender.send_replace(None);
        }
        self.context = None;
        self.connected.store(false, Ordering::Release);
    }
}

impl Drop for LongbridgeDataClient {
    fn drop(&mut self) {
        self.terminate_stream();
    }
}

const MAX_HISTORICAL_BARS: usize = 1_000;
const MAX_STATIC_INFO_SYMBOLS: usize = 500;

async fn load_instruments(
    context: &QuoteContext,
    config: &LongbridgeDataClientConfig,
    instrument_ids: &[InstrumentId],
    ts_init: UnixNanos,
) -> anyhow::Result<Vec<InstrumentAny>> {
    let mut instruments = Vec::with_capacity(instrument_ids.len());

    for ids in instrument_ids.chunks(MAX_STATIC_INFO_SYMBOLS) {
        let symbols = ids
            .iter()
            .map(|id| id.symbol.as_str().to_string())
            .collect::<Vec<_>>();
        let static_info = quote_api_call_with_retry(|| context.static_info(symbols.clone()))
            .await
            .context("failed to request Longbridge static security info")?;

        for info in static_info {
            let instrument_id = instrument_id(&info.symbol);
            let price_increment = config.price_increment(instrument_id)?;
            instruments.push(parse_instrument(&info, price_increment, ts_init)?);
        }
    }

    anyhow::ensure!(
        instruments.len() == instrument_ids.len(),
        "Longbridge returned static info for {} of {} requested instruments",
        instruments.len(),
        instrument_ids.len(),
    );
    instruments.sort_unstable_by_key(|instrument| instrument.id().to_string());
    Ok(instruments)
}

fn history_date(symbol: &str, value: UnixNanos) -> anyhow::Result<Date> {
    Ok(history_datetime(symbol, value)?.date())
}

fn history_datetime(symbol: &str, value: UnixNanos) -> anyhow::Result<PrimitiveDateTime> {
    let timezone_name = match symbol.rsplit_once('.').map(|(_, market)| market) {
        Some("US") => "America/New_York",
        Some("HK") => "Asia/Hong_Kong",
        Some("SH" | "SZ") => "Asia/Shanghai",
        Some("SG") => "Asia/Singapore",
        _ => anyhow::bail!("unsupported Longbridge market suffix in {symbol}"),
    };
    let timestamp = Timestamp::from_nanosecond(i128::from(value.as_u64()))?;
    let local = timestamp.to_zoned(get_timezone(timezone_name)?);
    let date = local.date();
    Ok(PrimitiveDateTime::new(
        Date::from_calendar_date(
            i32::from(date.year()),
            Month::try_from(u8::try_from(date.month())?)?,
            u8::try_from(date.day())?,
        )?,
        Time::from_hms(
            u8::try_from(local.hour())?,
            u8::try_from(local.minute())?,
            0,
        )?,
    ))
}

fn validate_historical_bar_request(
    bar_type: BarType,
    start: Option<UnixNanos>,
    end: Option<UnixNanos>,
    limit: usize,
) -> anyhow::Result<Period> {
    anyhow::ensure!(
        bar_type.aggregation_source() == AggregationSource::External,
        "Longbridge historical bars require EXTERNAL aggregation",
    );
    anyhow::ensure!(
        limit <= MAX_HISTORICAL_BARS,
        "Longbridge historical bar limit must not exceed {MAX_HISTORICAL_BARS}",
    );
    anyhow::ensure!(
        !matches!((start, end), (Some(start), Some(end)) if start > end),
        "Longbridge historical bar start must not be after end",
    );
    period_from_bar_type(bar_type)
}

async fn request_historical_bars(
    context: QuoteContext,
    bar_type: BarType,
    start: Option<UnixNanos>,
    end: Option<UnixNanos>,
    limit: usize,
    ts_init: UnixNanos,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let period = validate_historical_bar_request(bar_type, start, end, limit)?;
    let symbol = bar_type.instrument_id().symbol.as_str().to_string();
    let candlesticks = if let (Some(start), Some(end)) = (start, end) {
        let start_date = history_date(&symbol, start)?;
        let end_date = history_date(&symbol, end)?;
        history_api_call_with_retry(|| {
            context.history_candlesticks_by_date(
                symbol.clone(),
                period,
                AdjustType::NoAdjust,
                Some(start_date),
                Some(end_date),
                TradeSessions::All,
            )
        })
        .await?
    } else if let Some(end) = end {
        let end = history_datetime(&symbol, end)?;
        history_api_call_with_retry(|| {
            context.history_candlesticks_by_offset(
                symbol.clone(),
                period,
                AdjustType::NoAdjust,
                false,
                Some(end),
                limit,
                TradeSessions::All,
            )
        })
        .await?
    } else if start.is_none() {
        quote_api_call_with_retry(|| {
            context.candlesticks(
                symbol.clone(),
                period,
                limit,
                AdjustType::NoAdjust,
                TradeSessions::All,
            )
        })
        .await?
    } else {
        let start_date = start
            .map(|value| history_date(&symbol, value))
            .transpose()?;
        history_api_call_with_retry(|| {
            context.history_candlesticks_by_date(
                symbol.clone(),
                period,
                AdjustType::NoAdjust,
                start_date,
                None,
                TradeSessions::All,
            )
        })
        .await?
    };

    let mut bars = candlesticks
        .into_iter()
        .map(|candlestick| {
            parse_bar_with_price_precision(bar_type, candlestick, ts_init, price_precision)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    bars.retain(|bar| {
        start.is_none_or(|start| bar.ts_event >= start) && end.is_none_or(|end| bar.ts_event <= end)
    });

    if bars.len() > limit {
        bars = bars.split_off(bars.len() - limit);
    }
    Ok(bars)
}

#[async_trait(?Send)]
impl DataClient for LongbridgeDataClient {
    fn client_id(&self) -> ClientId {
        self.client_id
    }

    fn venue(&self) -> Option<Venue> {
        Some(*LONGBRIDGE_VENUE)
    }

    fn start(&mut self) -> anyhow::Result<()> {
        log::info!("Starting Longbridge data client {}", self.client_id);
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.terminate_stream();
        log::info!("Stopped Longbridge data client {}", self.client_id);
        Ok(())
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.terminate_stream();
        self.cancellation_token = CancellationToken::new();
        *self.subscriptions.lock().expect(MUTEX_POISONED) = SubscriptionState::default();
        Ok(())
    }

    fn dispose(&mut self) -> anyhow::Result<()> {
        self.stop()
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn is_disconnected(&self) -> bool {
        !self.is_connected()
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        if self.is_connected() {
            return Ok(());
        }

        self.cancellation_token = CancellationToken::new();
        let sdk_config = self.config.sdk_config().await?;
        let connection_guard = if let Some(guard) = self.connection_guard.take() {
            guard
        } else {
            try_acquire_quote_connection()
                .context("only one Longbridge quote connection is allowed per process")?
        };
        let (context, mut receiver) = QuoteContext::new(sdk_config);
        quote_api_call_with_retry(|| context.subscriptions())
            .await
            .context("failed to establish Longbridge quote session")?;

        let instrument_ids = self.config.instrument_ids()?;
        let instruments = load_instruments(
            &context,
            &self.config,
            &instrument_ids,
            self.clock.get_time_ns(),
        )
        .await?;

        for instrument in instruments {
            self.data_sender
                .send(DataEvent::Instrument(instrument))
                .context("failed to dispatch Longbridge instrument definition")?;
        }

        let sender = self.data_sender.clone();
        let subscriptions = Arc::clone(&self.subscriptions);
        let cancellation = self.cancellation_token.clone();
        let clock = self.clock;

        self.stream_handle = Some(get_runtime().spawn(async move {
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    event = receiver.recv() => {
                        let Some(event) = event else { break };
                        let ts_init = clock.get_time_ns();

                        match event.detail {
                            PushEventDetail::Depth(depth) => {
                                match parse_depth(&event.symbol, &depth.bids, &depth.asks, ts_init, ts_init) {
                                    Ok((depth, quote)) => {
                                        let state = subscriptions.lock().expect(MUTEX_POISONED);
                                        if state.depth10.contains(&depth.instrument_id)
                                            && let Err(e) = sender.send(DataEvent::Data(Data::Depth10(Box::new(depth))))
                                        {
                                            log::error!("Failed to dispatch Longbridge depth: {e}");
                                        }

                                        if state.quotes.contains(&depth.instrument_id)
                                            && let Some(quote) = quote
                                            && let Err(e) = sender.send(DataEvent::Data(Data::Quote(quote)))
                                        {
                                            log::error!("Failed to dispatch Longbridge quote: {e}");
                                        }
                                    }
                                    Err(e) => log::warn!("Failed to parse Longbridge depth: {e:#}"),
                                }
                            }
                            PushEventDetail::Trade(batch) => {
                                match parse_trades(&event.symbol, &batch.trades, ts_init) {
                                    Ok(trades) => {
                                        let subscribed = subscriptions
                                            .lock()
                                            .expect(MUTEX_POISONED)
                                            .trades
                                            .contains(&crate::common::parse::instrument_id(&event.symbol));

                                        if subscribed {
                                            for trade in trades {
                                                if let Err(e) = sender.send(DataEvent::Data(Data::Trade(trade))) {
                                                    log::error!("Failed to dispatch Longbridge trade: {e}");
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => log::warn!("Failed to parse Longbridge trades: {e:#}"),
                                }
                            }
                            PushEventDetail::Candlestick(update) => {
                                let key = (event.symbol.clone(), update.period as i32);
                                let bar_subscription = subscriptions
                                    .lock()
                                    .expect(MUTEX_POISONED)
                                    .bars
                                    .get(&key)
                                    .copied();

                                if let Some((bar_type, price_precision)) = bar_subscription {
                                    match parse_bar_with_price_precision(
                                        bar_type,
                                        update.candlestick,
                                        ts_init,
                                        price_precision,
                                    ) {
                                        Ok(bar) => {
                                            if update.is_confirmed && update.period == Period::OneMinute {
                                                match parse_completed_minute_bar(bar_type,update.candlestick,ts_init) {
                                                    Ok(value) => {
                                                        let data = CustomData::from_arc(Arc::new(value));
                                                        if let Err(e) = sender.send(DataEvent::Data(Data::Custom(data))) {
                                                            log::error!("Failed to dispatch confirmed Longbridge VWAP bar: {e}");
                                                        }
                                                    }
                                                    Err(e) => log::error!("Invalid confirmed Longbridge VWAP bar: {e:#}"),
                                                }
                                            }
                                            if let Err(e) = sender.send(DataEvent::Data(Data::Bar(bar))) {
                                                log::error!("Failed to dispatch Longbridge bar: {e}");
                                            }
                                        }
                                        Err(e) => log::warn!("Failed to parse Longbridge bar: {e:#}"),
                                    }
                                }
                            }
                            PushEventDetail::Quote(_) | PushEventDetail::Brokers(_) => {}
                        }
                    }
                }
            }
        }));

        if let Some(sender) = &self.context_sender {
            sender.send_replace(Some(context.clone()));
        }
        self.context = Some(context);
        self.connection_guard = Some(connection_guard);
        self.connected.store(true, Ordering::Release);
        log::info!("Connected Longbridge data client {}", self.client_id);
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.terminate_stream();
        log::info!("Disconnected Longbridge data client {}", self.client_id);
        Ok(())
    }

    fn subscribe_instruments(&mut self, cmd: SubscribeInstruments) -> anyhow::Result<()> {
        anyhow::ensure!(
            cmd.venue == *LONGBRIDGE_VENUE,
            "Longbridge cannot subscribe to instruments for venue {}",
            cmd.venue,
        );
        let context = self.context()?;
        let config = self.config.clone();
        let instrument_ids = config.instrument_ids()?;
        let sender = self.data_sender.clone();
        let clock = self.clock;
        self.spawn_task(async move {
            match load_instruments(&context, &config, &instrument_ids, clock.get_time_ns()).await {
                Ok(instruments) => {
                    for instrument in instruments {
                        if let Err(e) = sender.send(DataEvent::Instrument(instrument)) {
                            log::error!(
                                "Failed to dispatch Longbridge instrument definition: {e}",
                            );
                        }
                    }
                }
                Err(e) => log::error!("Longbridge instruments subscription failed: {e:#}"),
            }
        });
        Ok(())
    }

    fn subscribe_instrument(&mut self, cmd: SubscribeInstrument) -> anyhow::Result<()> {
        let context = self.context()?;
        let config = self.config.clone();
        config.price_increment(cmd.instrument_id)?;
        let sender = self.data_sender.clone();
        let clock = self.clock;
        self.spawn_task(async move {
            match load_instruments(&context, &config, &[cmd.instrument_id], clock.get_time_ns())
                .await
            {
                Ok(mut instruments) => {
                    let instrument = instruments
                        .pop()
                        .expect("one requested Longbridge instrument");
                    if let Err(e) = sender.send(DataEvent::Instrument(instrument)) {
                        log::error!("Failed to dispatch Longbridge instrument definition: {e}");
                    }
                }
                Err(e) => log::error!(
                    "Longbridge instrument subscription failed for {}: {e:#}",
                    cmd.instrument_id,
                ),
            }
        });
        Ok(())
    }

    fn subscribe_quotes(&mut self, cmd: SubscribeQuotes) -> anyhow::Result<()> {
        self.context()?;
        let symbol = cmd.instrument_id.symbol.to_string();
        {
            let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
            state.reserve_subscription(&symbol)?;
            state.quotes.insert(cmd.instrument_id);
        }
        self.reconcile_subscriptions(symbol)
    }

    fn unsubscribe_quotes(&mut self, cmd: &UnsubscribeQuotes) -> anyhow::Result<()> {
        self.context()?;
        self.subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .quotes
            .remove(&cmd.instrument_id);
        self.reconcile_subscriptions(cmd.instrument_id.symbol.to_string())
    }

    fn subscribe_book_depth10(&mut self, cmd: SubscribeBookDepth10) -> anyhow::Result<()> {
        self.context()?;
        let symbol = cmd.instrument_id.symbol.to_string();
        {
            let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
            state.reserve_subscription(&symbol)?;
            state.depth10.insert(cmd.instrument_id);
        }
        self.reconcile_subscriptions(symbol)
    }

    fn unsubscribe_book_depth10(&mut self, cmd: &UnsubscribeBookDepth10) -> anyhow::Result<()> {
        self.context()?;
        self.subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .depth10
            .remove(&cmd.instrument_id);
        self.reconcile_subscriptions(cmd.instrument_id.symbol.to_string())
    }

    fn subscribe_trades(&mut self, cmd: SubscribeTrades) -> anyhow::Result<()> {
        self.context()?;
        let symbol = cmd.instrument_id.symbol.to_string();
        {
            let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
            state.reserve_subscription(&symbol)?;
            state.trades.insert(cmd.instrument_id);
        }
        self.reconcile_subscriptions(symbol)
    }

    fn unsubscribe_trades(&mut self, cmd: &UnsubscribeTrades) -> anyhow::Result<()> {
        self.context()?;
        self.subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .trades
            .remove(&cmd.instrument_id);
        self.reconcile_subscriptions(cmd.instrument_id.symbol.to_string())
    }

    fn subscribe_bars(&mut self, cmd: SubscribeBars) -> anyhow::Result<()> {
        self.context()?;
        let period = period_from_bar_type(cmd.bar_type)?;
        let precision = self
            .config
            .price_increment(cmd.bar_type.instrument_id())?
            .precision;
        let symbol = cmd.bar_type.instrument_id().symbol.to_string();
        {
            let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
            state.reserve_subscription(&symbol)?;
            state
                .bars
                .insert((symbol.clone(), period as i32), (cmd.bar_type, precision));
        }
        self.reconcile_subscriptions(symbol)
    }

    fn unsubscribe_bars(&mut self, cmd: &UnsubscribeBars) -> anyhow::Result<()> {
        self.context()?;
        let period = period_from_bar_type(cmd.bar_type)?;
        let symbol = cmd.bar_type.instrument_id().symbol.to_string();
        self.subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .bars
            .remove(&(symbol.clone(), period as i32));
        self.reconcile_subscriptions(symbol)
    }

    fn request_instruments(&self, request: RequestInstruments) -> anyhow::Result<()> {
        if let Some(venue) = request.venue {
            anyhow::ensure!(
                venue == *LONGBRIDGE_VENUE,
                "Longbridge cannot request instruments for venue {venue}",
            );
        }
        let context = self.context()?;
        let config = self.config.clone();
        let instrument_ids = config.instrument_ids()?;
        let sender = self.data_sender.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);
        let start = datetime_to_unix_nanos(request.start);
        let end = datetime_to_unix_nanos(request.end);
        let clock = self.clock;

        self.spawn_task(async move {
            match load_instruments(&context, &config, &instrument_ids, clock.get_time_ns()).await {
                Ok(instruments) => {
                    let response = DataResponse::Instruments(InstrumentsResponse::new(
                        request.request_id,
                        client_id,
                        *LONGBRIDGE_VENUE,
                        instruments,
                        start,
                        end,
                        clock.get_time_ns(),
                        request.params,
                    ));

                    if let Err(e) = sender.send(DataEvent::Response(response)) {
                        log::error!("Failed to send Longbridge instruments response: {e}");
                    }
                }
                Err(e) => log::error!("Longbridge instruments request failed: {e:#}"),
            }
        });
        Ok(())
    }

    fn request_instrument(&self, request: RequestInstrument) -> anyhow::Result<()> {
        let context = self.context()?;
        let config = self.config.clone();
        config.price_increment(request.instrument_id)?;
        let sender = self.data_sender.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);
        let start = datetime_to_unix_nanos(request.start);
        let end = datetime_to_unix_nanos(request.end);
        let clock = self.clock;

        self.spawn_task(async move {
            match load_instruments(
                &context,
                &config,
                &[request.instrument_id],
                clock.get_time_ns(),
            )
            .await
            {
                Ok(mut instruments) => {
                    let instrument = instruments
                        .pop()
                        .expect("one requested Longbridge instrument");
                    let response = DataResponse::Instrument(Box::new(InstrumentResponse::new(
                        request.request_id,
                        client_id,
                        request.instrument_id,
                        instrument,
                        start,
                        end,
                        clock.get_time_ns(),
                        request.params,
                    )));

                    if let Err(e) = sender.send(DataEvent::Response(response)) {
                        log::error!("Failed to send Longbridge instrument response: {e}");
                    }
                }
                Err(e) => log::error!(
                    "Longbridge instrument request failed for {}: {e:#}",
                    request.instrument_id,
                ),
            }
        });
        Ok(())
    }

    fn request_bars(&self, request: RequestBars) -> anyhow::Result<()> {
        let context = self.context()?;
        let start = datetime_to_unix_nanos(request.start);
        let end = datetime_to_unix_nanos(request.end);
        let limit = request
            .limit
            .map_or(MAX_HISTORICAL_BARS, |value| value.get());
        validate_historical_bar_request(request.bar_type, start, end, limit)?;
        let price_precision = self
            .config
            .price_increment(request.bar_type.instrument_id())?
            .precision;
        let sender = self.data_sender.clone();
        let client_id = request.client_id.unwrap_or(self.client_id);
        let clock = self.clock;

        self.spawn_task(async move {
            match request_historical_bars(
                context,
                request.bar_type,
                start,
                end,
                limit,
                clock.get_time_ns(),
                price_precision,
            )
            .await
            {
                Ok(bars) => {
                    let response = DataResponse::Bars(BarsResponse::new(
                        request.request_id,
                        client_id,
                        request.bar_type,
                        bars,
                        start,
                        end,
                        clock.get_time_ns(),
                        request.params,
                    ));

                    if let Err(e) = sender.send(DataEvent::Response(response)) {
                        log::error!("Failed to send Longbridge bars response: {e}");
                    }
                }
                Err(e) => log::error!(
                    "Longbridge historical bar request failed for {}: {e:#}",
                    request.bar_type,
                ),
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nautilus_model::{
        data::{BarSpecification, BarType},
        enums::{AggregationSource, BarAggregation, PriceType},
    };
    use rstest::rstest;
    use time::macros::{date, datetime, time};

    use super::*;
    use crate::common::parse::instrument_id;

    #[rstest]
    fn test_historical_bar_request_validation() {
        let external = BarType::new(
            instrument_id("AAPL.US"),
            BarSpecification::new(1, BarAggregation::Minute, PriceType::Last),
            AggregationSource::External,
        );
        assert!(
            validate_historical_bar_request(
                external,
                Some(UnixNanos::from(1)),
                Some(UnixNanos::from(2)),
                1_000,
            )
            .is_ok()
        );
        assert!(validate_historical_bar_request(external, None, None, 1_001).is_err());
        assert!(
            validate_historical_bar_request(
                external,
                Some(UnixNanos::from(2)),
                Some(UnixNanos::from(1)),
                1,
            )
            .is_err()
        );

        let internal = BarType::new(
            instrument_id("AAPL.US"),
            BarSpecification::new(1, BarAggregation::Minute, PriceType::Last),
            AggregationSource::Internal,
        );
        assert!(validate_historical_bar_request(internal, None, None, 1).is_err());
    }

    #[rstest]
    fn test_history_date_uses_the_security_market_timezone() {
        let nanos = u64::try_from(datetime!(2026-01-01 01:00 UTC).unix_timestamp_nanos()).unwrap();
        let timestamp = UnixNanos::from(nanos);

        assert_eq!(
            history_date("AAPL.US", timestamp).unwrap(),
            date!(2025 - 12 - 31)
        );
        assert_eq!(
            history_date("700.HK", timestamp).unwrap(),
            date!(2026 - 01 - 01)
        );
        assert_eq!(
            history_datetime("AAPL.US", timestamp).unwrap().time(),
            time!(20:00)
        );
    }

    #[test]
    fn test_rotating_subscriptions_release_only_after_acknowledgement() {
        let mut state = SubscriptionState::default();
        for n in 0..1_000 {
            let symbol = format!("STOCK{n}.US");
            let id = instrument_id(&symbol);
            state.reserve_subscription(&symbol).unwrap();
            state.quotes.insert(id);
            state.applied_flags.insert(symbol.clone(), SubFlags::DEPTH);
            state.quotes.remove(&id);
            state.release_acknowledged(&symbol);
            assert!(state.reserved_symbols.contains(&symbol));
            // A resubscription received while unsubscribe is in flight retains its slot.
            state.quotes.insert(id);
            state.applied_flags.remove(&symbol);
            state.release_acknowledged(&symbol);
            assert!(state.reserved_symbols.contains(&symbol));
            state.quotes.remove(&id);
            state.release_acknowledged(&symbol);
            assert!(state.reserved_symbols.is_empty());
        }
        let id = instrument_id("AAPL.US");
        state.reserve_subscription("AAPL.US").unwrap();
        state.depth10.insert(id);
        state.release_acknowledged("AAPL.US");
        assert_eq!(state.reserved_symbols.len(), 1);
    }

    #[rstest]
    fn test_subscription_limit_counts_unique_symbols() {
        let mut state = SubscriptionState::default();
        for index in 0..crate::common::rate_limit::MAX_QUOTE_SUBSCRIPTION_SYMBOLS {
            state.reserve_subscription(&format!("{index}.US")).unwrap();
        }

        assert!(state.reserve_subscription("0.US").is_ok());
        assert!(state.reserve_subscription("OVER.US").is_err());
    }
}
