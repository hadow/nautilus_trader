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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::{AHashMap, AHashSet};
use anyhow::Context;
use async_trait::async_trait;
use longbridge::quote::{Period, PushEventDetail, QuoteContext, SubFlags, TradeSessions};
use nautilus_common::{
    clients::DataClient,
    live::{get_runtime, runner::get_data_event_sender},
    messages::{
        DataEvent,
        data::{
            SubscribeBars, SubscribeBookDepth10, SubscribeInstrument, SubscribeQuotes,
            SubscribeTrades, UnsubscribeBars, UnsubscribeBookDepth10, UnsubscribeQuotes,
            UnsubscribeTrades,
        },
    },
};
use nautilus_core::{
    MUTEX_POISONED,
    time::{AtomicTime, get_atomic_clock_realtime},
};
use nautilus_model::{
    data::{BarType, Data},
    identifiers::{ClientId, InstrumentId, Venue},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    common::{
        consts::LONGBRIDGE_VENUE,
        parse::{parse_bar, parse_depth, parse_trades, period_from_bar_type},
    },
    config::LongbridgeDataClientConfig,
};

#[derive(Debug, Default)]
struct SubscriptionState {
    quotes: AHashSet<InstrumentId>,
    depth10: AHashSet<InstrumentId>,
    trades: AHashSet<InstrumentId>,
    bars: AHashMap<(String, i32), BarType>,
}

/// Longbridge live data client.
pub struct LongbridgeDataClient {
    client_id: ClientId,
    config: LongbridgeDataClientConfig,
    context: Option<QuoteContext>,
    stream_handle: Option<JoinHandle<()>>,
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
            stream_handle: None,
            cancellation_token: CancellationToken::new(),
            subscriptions: Arc::new(Mutex::new(SubscriptionState::default())),
            data_sender: get_data_event_sender(),
            clock: get_atomic_clock_realtime(),
            connected: AtomicBool::new(false),
        }
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
        get_runtime().spawn(async move {
            if let Err(error) = future.await {
                log::error!("Longbridge {description} failed: {error}");
            }
        });
    }

    fn terminate_stream(&mut self) {
        self.cancellation_token.cancel();

        if let Some(handle) = self.stream_handle.take() {
            handle.abort();
        }
        self.context = None;
        self.connected.store(false, Ordering::Release);
    }
}

use std::future::Future;

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
        let (context, mut receiver) = QuoteContext::new(sdk_config);
        context
            .subscriptions()
            .await
            .context("failed to establish Longbridge quote session")?;

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
                                            && let Err(error) = sender.send(DataEvent::Data(Data::Depth10(Box::new(depth))))
                                        {
                                            log::error!("Failed to dispatch Longbridge depth: {error}");
                                        }

                                        if state.quotes.contains(&depth.instrument_id)
                                            && let Some(quote) = quote
                                            && let Err(error) = sender.send(DataEvent::Data(Data::Quote(quote)))
                                        {
                                            log::error!("Failed to dispatch Longbridge quote: {error}");
                                        }
                                    }
                                    Err(error) => log::warn!("Failed to parse Longbridge depth: {error:#}"),
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
                                                if let Err(error) = sender.send(DataEvent::Data(Data::Trade(trade))) {
                                                    log::error!("Failed to dispatch Longbridge trade: {error}");
                                                }
                                            }
                                        }
                                    }
                                    Err(error) => log::warn!("Failed to parse Longbridge trades: {error:#}"),
                                }
                            }
                            PushEventDetail::Candlestick(update) => {
                                let key = (event.symbol.clone(), update.period as i32);
                                let bar_type = subscriptions
                                    .lock()
                                    .expect(MUTEX_POISONED)
                                    .bars
                                    .get(&key)
                                    .copied();

                                if let Some(bar_type) = bar_type {
                                    match parse_bar(bar_type, update.candlestick, ts_init) {
                                        Ok(bar) => {
                                            if let Err(error) = sender.send(DataEvent::Data(Data::Bar(bar))) {
                                                log::error!("Failed to dispatch Longbridge bar: {error}");
                                            }
                                        }
                                        Err(error) => log::warn!("Failed to parse Longbridge bar: {error:#}"),
                                    }
                                }
                            }
                            PushEventDetail::Quote(_) | PushEventDetail::Brokers(_) => {}
                        }
                    }
                }
            }
        }));

        self.context = Some(context);
        self.connected.store(true, Ordering::Release);
        log::info!("Connected Longbridge data client {}", self.client_id);
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.terminate_stream();
        log::info!("Disconnected Longbridge data client {}", self.client_id);
        Ok(())
    }

    fn subscribe_instrument(&mut self, cmd: SubscribeInstrument) -> anyhow::Result<()> {
        anyhow::bail!(
            "Longbridge does not expose tick-size metadata for {}; register the Instrument from a catalog or custom provider",
            cmd.instrument_id,
        )
    }

    fn subscribe_quotes(&mut self, cmd: SubscribeQuotes) -> anyhow::Result<()> {
        let context = self.context()?;
        let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
        let already_active = state.depth10.contains(&cmd.instrument_id);
        let inserted = state.quotes.insert(cmd.instrument_id);
        drop(state);

        if already_active || !inserted {
            return Ok(());
        }
        let symbol = cmd.instrument_id.symbol.as_str().to_string();
        self.spawn_result("quote subscription", async move {
            context.subscribe([symbol], SubFlags::DEPTH).await
        });
        Ok(())
    }

    fn subscribe_book_depth10(&mut self, cmd: SubscribeBookDepth10) -> anyhow::Result<()> {
        let context = self.context()?;
        let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
        let already_active = state.quotes.contains(&cmd.instrument_id);
        let inserted = state.depth10.insert(cmd.instrument_id);
        drop(state);

        if already_active || !inserted {
            return Ok(());
        }
        let symbol = cmd.instrument_id.symbol.as_str().to_string();
        self.spawn_result("depth subscription", async move {
            context.subscribe([symbol], SubFlags::DEPTH).await
        });
        Ok(())
    }

    fn subscribe_trades(&mut self, cmd: SubscribeTrades) -> anyhow::Result<()> {
        let context = self.context()?;
        let inserted = self
            .subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .trades
            .insert(cmd.instrument_id);

        if !inserted {
            return Ok(());
        }
        let symbol = cmd.instrument_id.symbol.as_str().to_string();
        self.spawn_result("trade subscription", async move {
            context.subscribe([symbol], SubFlags::TRADE).await
        });
        Ok(())
    }

    fn subscribe_bars(&mut self, cmd: SubscribeBars) -> anyhow::Result<()> {
        let context = self.context()?;
        let period = period_from_bar_type(cmd.bar_type)?;
        let symbol = cmd.bar_type.instrument_id().symbol.as_str().to_string();
        let previous = self
            .subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .bars
            .insert((symbol.clone(), period as i32), cmd.bar_type);

        if previous.is_some() {
            return Ok(());
        }
        self.spawn_result("bar subscription", async move {
            context
                .subscribe_candlesticks(symbol, period, TradeSessions::All)
                .await
                .map(|_| ())
        });
        Ok(())
    }

    fn unsubscribe_quotes(&mut self, cmd: &UnsubscribeQuotes) -> anyhow::Result<()> {
        let context = self.context()?;
        let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
        let removed = state.quotes.remove(&cmd.instrument_id);
        let retain_depth = state.depth10.contains(&cmd.instrument_id);
        drop(state);

        if removed && !retain_depth {
            let symbol = cmd.instrument_id.symbol.as_str().to_string();
            self.spawn_result("quote unsubscription", async move {
                context.unsubscribe([symbol], SubFlags::DEPTH).await
            });
        }
        Ok(())
    }

    fn unsubscribe_book_depth10(&mut self, cmd: &UnsubscribeBookDepth10) -> anyhow::Result<()> {
        let context = self.context()?;
        let mut state = self.subscriptions.lock().expect(MUTEX_POISONED);
        let removed = state.depth10.remove(&cmd.instrument_id);
        let retain_depth = state.quotes.contains(&cmd.instrument_id);
        drop(state);

        if removed && !retain_depth {
            let symbol = cmd.instrument_id.symbol.as_str().to_string();
            self.spawn_result("depth unsubscription", async move {
                context.unsubscribe([symbol], SubFlags::DEPTH).await
            });
        }
        Ok(())
    }

    fn unsubscribe_trades(&mut self, cmd: &UnsubscribeTrades) -> anyhow::Result<()> {
        let context = self.context()?;
        let removed = self
            .subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .trades
            .remove(&cmd.instrument_id);

        if !removed {
            return Ok(());
        }
        let symbol = cmd.instrument_id.symbol.as_str().to_string();
        self.spawn_result("trade unsubscription", async move {
            context.unsubscribe([symbol], SubFlags::TRADE).await
        });
        Ok(())
    }

    fn unsubscribe_bars(&mut self, cmd: &UnsubscribeBars) -> anyhow::Result<()> {
        let context = self.context()?;
        let period: Period = period_from_bar_type(cmd.bar_type)?;
        let symbol = cmd.bar_type.instrument_id().symbol.as_str().to_string();
        let removed = self
            .subscriptions
            .lock()
            .expect(MUTEX_POISONED)
            .bars
            .remove(&(symbol.clone(), period as i32));

        if removed.is_none() {
            return Ok(());
        }
        self.spawn_result("bar unsubscription", async move {
            context.unsubscribe_candlesticks(symbol, period).await
        });
        Ok(())
    }
}
