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

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use nautilus_common::{actor::DataActor, timer::TimeEvent};
use nautilus_core::UnixNanos;
use nautilus_model::{
    accounts::Account,
    data::{
        Bar, BarType, CustomData, CustomDataTrait, DataType, QuoteTick,
        bar::BAR_SPEC_1_MINUTE_LAST, bar_vwap::BarWithVwap,
    },
    enums::{AggregationSource, OrderSide, PositionSide, TimeInForce},
    events::{
        OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired, OrderFilled, OrderRejected,
        PositionClosed, PositionOpened,
    },
    identifiers::{ClientOrderId, PositionId},
    instruments::Instrument,
    orders::Order,
    types::{Currency, Price, Quantity},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    IntradayMomentumConfig, IntradayMomentumDecision, IntradayMomentumModel, PositionTarget,
    model::sized_quantity, reference::MinuteFeatures,
};
use crate::{
    nautilus_strategy,
    strategy::{Strategy, StrategyCore},
};

const MINUTE: u64 = 60_000_000_000;
const FLATTEN_ALERT_PREFIX: &str = "INTRADAY_MOMENTUM_FLATTEN_";

/// Research output retained by the strategy instance.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct IntradayMomentumReport {
    pub decisions: Vec<IntradayMomentumDecision>,
    pub features: Vec<MinuteFeatures>,
    pub fills: Vec<FillRecord>,
    pub theoretical_orders: usize,
    pub errors: Vec<String>,
    #[serde(default)]
    pub risk_events: Vec<RiskEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RiskEvent {
    pub timestamp: UnixNanos,
    pub reason: String,
    pub equity: Decimal,
    pub loss_limit: Decimal,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FillRecord {
    pub timestamp: u64,
    pub signal_timestamp: u64,
    pub order_id: String,
    pub side: i32,
    pub quantity: Decimal,
    pub signal_price: Option<Decimal>,
    pub expected_price: Option<Decimal>,
    #[serde(default)]
    pub order_price: Option<Decimal>,
    #[serde(default)]
    pub slippage: Option<Decimal>,
    pub fill_price: Decimal,
    pub commission: Decimal,
}

pub type SharedIntradayMomentumReport = Arc<Mutex<IntradayMomentumReport>>;

#[derive(Clone, Copy, Debug)]
struct OrderContext {
    signal_timestamp: UnixNanos,
    submitted_at: UnixNanos,
    signal_price: Option<Decimal>,
    expected_price: Option<Decimal>,
}

#[derive(Clone, Copy, Debug)]
struct PositionSnapshot {
    id: PositionId,
    side: PositionSide,
    quantity: Quantity,
}

/// Nautilus strategy wrapper around the causal intraday momentum model.
// ponytail: restart recovery is intentionally refused; add persisted model/order state before
// allowing a node to adopt exposure from an interrupted session.
pub struct IntradayMomentumStrategy {
    core: StrategyCore,
    config: IntradayMomentumConfig,
    model: IntradayMomentumModel,
    report: SharedIntradayMomentumReport,
    working_bar: Option<Bar>,
    desired: PositionTarget,
    desired_open: Option<Price>,
    desired_leverage: Decimal,
    sizing_equity: Option<(UnixNanos, Decimal)>,
    pending_order: Option<ClientOrderId>,
    order_contexts: BTreeMap<ClientOrderId, OrderContext>,
    pending_target: PositionTarget,
    pending_closes_position: bool,
    halted: bool,
    last_bar: Option<UnixNanos>,
    signal_timestamp: UnixNanos,
    signal_price: Option<Decimal>,
    dry_target: PositionTarget,
    dry_quantity: Decimal,
    orders_today: usize,
    entries_today: usize,
    flatten_session: Option<UnixNanos>,
    awaiting_quote: bool,
    entry_equity: Option<Decimal>,
}

impl IntradayMomentumStrategy {
    /// Creates a validated strategy.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid strategy configuration.
    pub fn new(config: IntradayMomentumConfig) -> anyhow::Result<Self> {
        Self::with_report(
            config,
            Arc::new(Mutex::new(IntradayMomentumReport::default())),
        )
    }

    /// Creates a validated strategy using a caller-owned report handle.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid strategy configuration.
    pub fn with_report(
        config: IntradayMomentumConfig,
        report: SharedIntradayMomentumReport,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        let model = IntradayMomentumModel::new(config.clone())?;
        Ok(Self {
            core: StrategyCore::new(config.base.clone()),
            config,
            model,
            report,
            working_bar: None,
            desired: PositionTarget::Flat,
            desired_open: None,
            desired_leverage: Decimal::ONE,
            sizing_equity: None,
            pending_order: None,
            order_contexts: BTreeMap::new(),
            pending_target: PositionTarget::Flat,
            pending_closes_position: false,
            halted: false,
            last_bar: None,
            signal_timestamp: UnixNanos::default(),
            signal_price: None,
            dry_target: PositionTarget::Flat,
            dry_quantity: Decimal::ZERO,
            orders_today: 0,
            entries_today: 0,
            flatten_session: None,
            awaiting_quote: false,
            entry_equity: None,
        })
    }

    #[must_use]
    pub fn report_handle(&self) -> SharedIntradayMomentumReport {
        Arc::clone(&self.report)
    }

    /// Loads completed one-minute regular-session history before registration.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or malformed history.
    pub fn warmup(&mut self, bars: Vec<Bar>) -> anyhow::Result<()> {
        self.model.warmup(bars)
    }

    /// Replays completed provider VWAP bars before node registration.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or invalid session history.
    pub fn warmup_with_vwap(&mut self, bars: &[BarWithVwap]) -> anyhow::Result<()> {
        for value in bars {
            self.model.on_bar_with_vwap(value.bar, value.vwap)?;
            self.last_bar = Some(value.bar.ts_event);
        }
        Ok(())
    }

    fn bar_type(&self) -> BarType {
        BarType::new(
            self.config.instrument_id,
            BAR_SPEC_1_MINUTE_LAST,
            AggregationSource::External,
        )
    }

    fn accept_bar(&mut self, bar: Bar) -> Option<Bar> {
        if self.config.bars_are_final {
            return Some(bar);
        }
        match self.working_bar {
            None => {
                self.working_bar = Some(bar);
                None
            }
            Some(current) if current.ts_event == bar.ts_event => {
                self.working_bar = Some(bar);
                None
            }
            Some(current) if current.ts_event < bar.ts_event => {
                self.working_bar = Some(bar);
                Some(Bar::new(
                    current.bar_type,
                    current.open,
                    current.high,
                    current.low,
                    current.close,
                    current.volume,
                    UnixNanos::from(current.ts_event.as_u64() + MINUTE),
                    bar.ts_init,
                ))
            }
            Some(_) => {
                log::warn!(
                    "Discarding out-of-order intraday momentum bar for {} at {}",
                    bar.bar_type.instrument_id(),
                    bar.ts_event,
                );
                None
            }
        }
    }

    fn account_equity(&self) -> anyhow::Result<Decimal> {
        if self.config.dry_run {
            return Ok(self.config.dry_run_equity);
        }
        let venue = self.config.instrument_id.venue;
        let currency = Currency::USD();
        self.portfolio()
            .equity(&venue, None)
            .get(&currency)
            .copied()
            .or_else(|| {
                self.cache()
                    .account_for_venue(&venue)
                    .and_then(|account| account.balance_total(Some(currency)))
            })
            .map(|money| money.as_decimal())
            .ok_or_else(|| anyhow::anyhow!("USD account equity unavailable for {venue}"))
    }

    fn position(&self) -> anyhow::Result<Option<PositionSnapshot>> {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let positions = self.cache().positions_open(
            None,
            Some(&self.config.instrument_id),
            Some(&strategy_id),
            None,
            None,
        );
        anyhow::ensure!(
            positions.len() <= 1,
            "netting strategy has multiple open positions"
        );
        Ok(positions.first().map(|position| PositionSnapshot {
            id: position.id,
            side: position.side,
            quantity: position.quantity,
        }))
    }

    fn apply_decision(&mut self, decision: IntradayMomentumDecision) -> anyhow::Result<()> {
        log::info!(
            "Intraday momentum decision: timestamp={} target={:?} close={} vwap={} upper={} lower={} daily_volatility={:.6} leverage={}",
            decision.timestamp,
            decision.target,
            decision.close,
            decision.vwap,
            decision.upper_bound,
            decision.lower_bound,
            decision.daily_volatility,
            decision.leverage,
        );
        self.report
            .lock()
            .map_err(|_| anyhow::anyhow!("intraday momentum report lock poisoned"))?
            .decisions
            .push(decision);
        if self.halted {
            return Ok(());
        }
        if self.flatten_session == Some(decision.session_open) {
            return Ok(());
        }
        if self
            .sizing_equity
            .is_none_or(|(session, _)| session != decision.session_open)
        {
            self.sizing_equity = Some((decision.session_open, self.account_equity()?));
            self.orders_today = 0;
            self.entries_today = 0;
        }
        self.signal_timestamp = decision.timestamp;
        self.signal_price = Some(decision.close.as_decimal());
        self.desired = decision.target;
        if self.config.risk_per_trade.is_some()
            && self
                .model
                .latest_features()
                .is_some_and(|f| f.raw_signal == PositionTarget::Flat)
            && if self.config.dry_run {
                self.dry_target == PositionTarget::Flat
            } else {
                self.position()?.is_none()
            }
        {
            // A safety exit or refused entry cannot turn a model hold into a fresh entry
            self.desired = PositionTarget::Flat;
        }
        self.desired_open = Some(decision.open);
        self.desired_leverage = decision.leverage;
        self.cancel_stale_entry()?;
        self.awaiting_quote = self.config.defer_to_next_quote;
        if self.awaiting_quote {
            return Ok(());
        }
        self.sync_target()
    }

    /// Checks cash risk on observed quotes, independently of the model's decision schedule.
    fn enforce_quote_risk(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let Some((session, start)) = self.sizing_equity else {
            return Ok(());
        };
        if self.halted
            || self.flatten_session == Some(session)
            || self
                .config
                .session(quote.ts_event)
                .is_none_or(|s| s.open != session)
            || (self.config.max_daily_loss == Decimal::ONE && self.config.risk_per_trade.is_none())
        {
            return Ok(());
        }
        let equity = self.account_equity()?;
        let daily_limit = start * self.config.max_daily_loss;
        // ponytail: this equity guard requires a dedicated account; use position PnL for shared accounts
        let trade = self.entry_equity.zip(self.config.risk_per_trade);
        let event = if start - equity >= daily_limit {
            self.flatten_session = Some(session);
            Some(("DAILY_LOSS", daily_limit))
        } else if let Some((entry, risk)) = trade
            && entry - equity >= entry * risk
            && self.position()?.is_some()
            && !self.pending_closes_position
        {
            Some(("TRADE_LOSS", entry * risk))
        } else {
            None
        };
        if let Some((reason, loss_limit)) = event {
            log::warn!(
                "RISK_EXIT instrument={} timestamp={} reason={} equity={} loss_limit={}",
                self.config.instrument_id,
                quote.ts_event,
                reason,
                equity,
                loss_limit
            );
            self.report
                .lock()
                .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
                .risk_events
                .push(RiskEvent {
                    timestamp: quote.ts_event,
                    reason: reason.to_owned(),
                    equity,
                    loss_limit,
                });
            self.desired = PositionTarget::Flat;
            self.signal_timestamp = quote.ts_event;
            self.signal_price = Some(
                (quote.bid_price.as_decimal() + quote.ask_price.as_decimal()) / Decimal::from(2),
            );
            // This quote is already observed; no fictitious wait for a subsequent bar open
            self.awaiting_quote = false;
            self.cancel_stale_entry()?;
            self.sync_target()?;
        }
        Ok(())
    }

    fn cancel_stale_entry(&mut self) -> anyhow::Result<()> {
        let Some(order_id) = self.pending_order else {
            return Ok(());
        };
        if self.pending_closes_position || self.pending_target == self.desired {
            return Ok(());
        }
        if self
            .cache()
            .order(&order_id)
            .is_some_and(|order| !order.is_closed() && !order.is_pending_cancel())
        {
            self.cancel_order(order_id, None, None)?;
        }
        Ok(())
    }

    fn sync_target(&mut self) -> anyhow::Result<()> {
        if self.awaiting_quote {
            return Ok(());
        }
        if self.config.dry_run {
            if self.dry_target != self.desired {
                self.limit_entry_target();
                if self.dry_target == self.desired {
                    return Ok(());
                }
                let quantity = if self.desired == PositionTarget::Flat {
                    self.dry_quantity
                } else {
                    let Some(quantity) = self.entry_quantity()? else {
                        return Ok(());
                    };
                    quantity.as_decimal()
                };
                log::info!(
                    "ORDER_WOULD_SUBMIT instrument={} signal_timestamp={} current_position={:?} target_position={:?} quantity={} signal_price={:?}",
                    self.config.instrument_id,
                    self.signal_timestamp,
                    self.dry_target,
                    self.desired,
                    quantity,
                    self.signal_price
                );
                self.report
                    .lock()
                    .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
                    .theoretical_orders += 1;
                self.dry_quantity = if self.desired == PositionTarget::Flat {
                    Decimal::ZERO
                } else {
                    quantity
                };
                self.dry_target = self.desired;
                if self.desired != PositionTarget::Flat {
                    self.entries_today += 1;
                }
            }
            return Ok(());
        }
        if self.pending_order.is_some() {
            return Ok(());
        }
        let position = self.position()?;
        if let Some(position) = position {
            let matches = matches!(
                (position.side, self.desired),
                (PositionSide::Long, PositionTarget::Long)
                    | (PositionSide::Short, PositionTarget::Short)
            );
            if matches {
                return Ok(());
            }
            let side = match position.side {
                PositionSide::Long => OrderSide::Sell,
                PositionSide::Short => OrderSide::Buy,
                PositionSide::Flat => return Ok(()),
                PositionSide::NoPositionSide => {
                    anyhow::bail!("open position has no position side")
                }
            };
            return self.submit_market(side, position.quantity, Some(position.id), true);
        }
        self.limit_entry_target();
        let side = match self.desired {
            PositionTarget::Long => OrderSide::Buy,
            PositionTarget::Short => OrderSide::Sell,
            PositionTarget::Flat => return Ok(()),
        };
        let Some(quantity) = self.entry_quantity()? else {
            return Ok(());
        };
        self.submit_market(side, quantity, None, false)
    }

    fn limit_entry_target(&mut self) {
        if self.desired != PositionTarget::Flat
            && self
                .config
                .max_entries_per_day
                .is_some_and(|limit| self.entries_today >= limit)
        {
            log::info!(
                "ENTRY_LIMIT instrument={} signal_timestamp={} entries_today={} target={:?}",
                self.config.instrument_id,
                self.signal_timestamp,
                self.entries_today,
                self.desired
            );
            self.desired = PositionTarget::Flat;
        }
    }

    fn entry_quantity(&self) -> anyhow::Result<Option<Quantity>> {
        let (_, equity) = self
            .sizing_equity
            .ok_or_else(|| anyhow::anyhow!("session sizing equity unavailable"))?;
        let open = self
            .desired_open
            .ok_or_else(|| anyhow::anyhow!("session opening price unavailable"))?;
        let instrument = self.cache().try_instrument(&self.config.instrument_id)?;
        let lot_size = instrument
            .lot_size()
            .map_or(Decimal::ONE, |quantity| quantity.as_decimal())
            .max(Decimal::ONE);
        let raw_quantity = sized_quantity(
            equity,
            self.config.capital_fraction,
            self.desired_leverage,
            open,
            lot_size,
        )?;
        let price = self.signal_price.unwrap_or(open.as_decimal());
        let free = if self.config.dry_run {
            self.config.dry_run_equity
        } else {
            let account = self
                .cache()
                .account_for_venue(&self.config.instrument_id.venue)
                .ok_or_else(|| anyhow::anyhow!("account unavailable"))?;
            account
                .balance_free(Some(Currency::USD()))
                .ok_or_else(|| anyhow::anyhow!("buying power unavailable"))?
                .as_decimal()
        };
        let cap = self
            .config
            .max_position_notional
            .min(self.config.max_order_notional)
            .min(free.max(Decimal::ZERO) * self.config.max_leverage);
        let mut raw_quantity = raw_quantity.min((cap / price / lot_size).floor() * lot_size);
        if let Some(risk) = self.config.risk_per_trade {
            let features = self
                .model
                .latest_features()
                .ok_or_else(|| anyhow::anyhow!("risk sizing requires current band/VWAP"))?;
            let vwap = features
                .anchored_vwap
                .context("risk sizing requires VWAP")?;
            let quote = self
                .cache()
                .quote(&self.config.instrument_id)
                .ok_or_else(|| anyhow::anyhow!("risk sizing requires current quote"))?;
            let (entry, stop) = match self.desired {
                PositionTarget::Long => (
                    quote.ask_price.as_decimal(),
                    features
                        .upper_bound
                        .context("upper band missing")?
                        .max(vwap),
                ),
                PositionTarget::Short => (
                    quote.bid_price.as_decimal(),
                    features
                        .lower_bound
                        .context("lower band missing")?
                        .min(vwap),
                ),
                PositionTarget::Flat => return Ok(None),
            };
            let distance = Decimal::from(self.desired.sign()) * (entry - stop);
            if distance <= Decimal::ZERO {
                log::info!(
                    "RISK_ENTRY_SKIPPED instrument={} reason=INVALID_STOP entry={} stop={}",
                    self.config.instrument_id,
                    entry,
                    stop
                );
                return Ok(None);
            }
            let current_equity = self.account_equity()?;
            raw_quantity = raw_quantity
                .min((current_equity * risk / distance / lot_size).floor() * lot_size)
                .min((cap / entry / lot_size).floor() * lot_size);
            if raw_quantity < lot_size {
                return Ok(None);
            }
        }
        anyhow::ensure!(
            raw_quantity >= lot_size,
            "volatility target produced less than one lot"
        );
        Ok(Some(
            instrument.try_make_qty_from_decimal(raw_quantity, Some(true))?,
        ))
    }

    fn submit_market(
        &mut self,
        side: OrderSide,
        quantity: Quantity,
        position_id: Option<PositionId>,
        closes_position: bool,
    ) -> anyhow::Result<()> {
        if !closes_position {
            anyhow::ensure!(!self.halted, "entry refused while halted");
            anyhow::ensure!(
                self.orders_today < self.config.max_orders_per_day,
                "maximum daily orders reached"
            );
        }
        let now = self.clock().timestamp_ns();
        let quote = self.cache().quote(&self.config.instrument_id);
        if !closes_position {
            let quote = quote.ok_or_else(|| anyhow::anyhow!("no quote for entry"))?;
            anyhow::ensure!(
                now.as_u64().saturating_sub(quote.ts_event.as_u64())
                    <= self.config.stale_data_seconds * 1_000_000_000,
                "stale quote for entry"
            );
        }
        let expected_price = quote.map(|q| {
            if side == OrderSide::Buy {
                q.ask_price.as_decimal()
            } else {
                q.bid_price.as_decimal()
            }
        });
        self.orders_today += 1;
        let strategy_id = self
            .strategy_id()
            .ok_or_else(|| anyhow::anyhow!("strategy not registered"))?;
        let id = ClientOrderId::from(format!(
            "{strategy_id}-{}-{}",
            self.signal_timestamp.as_u64(),
            self.orders_today
        ));
        anyhow::ensure!(
            self.cache().order(&id).is_none(),
            "duplicate client order ID {id}"
        );
        let order = self.order().market(
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
        );
        let order_id = order.client_order_id();
        self.order_contexts.insert(
            order_id,
            OrderContext {
                signal_timestamp: self.signal_timestamp,
                submitted_at: now,
                signal_price: self.signal_price,
                expected_price,
            },
        );
        self.pending_order = Some(order_id);
        self.pending_target = self.desired;
        self.pending_closes_position = closes_position;
        if !closes_position {
            // Reserve the opportunity before submission: partial fills/retries cannot add entries.
            self.entries_today += 1;
            self.entry_equity = self
                .config
                .risk_per_trade
                .map(|_| self.account_equity())
                .transpose()?;
        }
        if let Err(e) = self.submit_order(order, position_id, None, None) {
            self.pending_order = None;
            return Err(e);
        }
        Ok(())
    }

    fn flatten(&mut self) -> anyhow::Result<()> {
        if let Some((session, _)) = self.sizing_equity {
            self.flatten_session = Some(session);
        }
        self.desired = PositionTarget::Flat;
        let timestamp = self.clock().timestamp_ns();
        self.signal_timestamp = timestamp;
        self.awaiting_quote = self.config.defer_to_next_quote;
        self.cancel_stale_entry()?;
        self.sync_target()
    }

    fn terminal(&mut self, order_id: ClientOrderId) -> anyhow::Result<()> {
        if self.pending_order != Some(order_id) {
            return Ok(());
        }
        if self.pending_order == Some(order_id) {
            self.pending_order = None;
            self.pending_closes_position = false;
        }
        self.sync_target()
    }

    fn filled_terminal(&mut self, order_id: ClientOrderId) -> anyhow::Result<()> {
        if self.pending_order != Some(order_id) {
            return Ok(());
        }
        let closes_position = self.pending_closes_position;
        self.pending_order = None;
        self.pending_closes_position = false;
        if closes_position {
            Ok(())
        } else {
            self.sync_target()
        }
    }

    fn halt(&mut self, e: &anyhow::Error) {
        self.record_halt(e);
        if let Err(flatten_error) = self.flatten() {
            log::error!("Intraday momentum emergency flatten failed: {flatten_error:#}");
        }
    }

    fn record_halt(&mut self, e: &anyhow::Error) {
        self.halted = true;
        self.desired = PositionTarget::Flat;
        log::error!("Intraday momentum halted: {e:#}");
        if let Ok(mut report) = self.report.lock() {
            report.errors.push(format!("{e:#}"));
        }
    }

    fn handle_terminal_failure(&mut self, order_id: ClientOrderId, reason: &str) {
        if self.pending_order != Some(order_id) {
            return;
        }
        let closes_position = self.pending_closes_position;
        self.pending_order = None;
        self.pending_closes_position = false;
        let e = anyhow::anyhow!("order {order_id} failed: {reason}");
        if closes_position {
            self.record_halt(&e);
            log::error!("ALERT: Exit rejected; manual reconciliation required, no blind retry");
        } else {
            self.halt(&e);
        }
    }
}

nautilus_strategy!(IntradayMomentumStrategy, {
    fn on_order_filled(&mut self, event: &OrderFilled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        let Some(context) = self.order_contexts.get(&event.client_order_id).copied() else {
            self.record_halt(&anyhow::anyhow!(
                "fill without local order context: {}",
                event.client_order_id
            ));
            return;
        };
        let side = if event.order_side == OrderSide::Buy {
            1
        } else {
            -1
        };
        let record = FillRecord {
            timestamp: event.ts_event.as_u64(),
            signal_timestamp: context.signal_timestamp.as_u64(),
            order_id: event.client_order_id.to_string(),
            side: if event.order_side == OrderSide::Buy {
                1
            } else {
                -1
            },
            quantity: event.last_qty.as_decimal(),
            signal_price: context.signal_price,
            expected_price: context.expected_price,
            order_price: None,
            slippage: context
                .expected_price
                .map(|p| Decimal::from(side) * (event.last_px.as_decimal() - p)),
            fill_price: event.last_px.as_decimal(),
            commission: event.commission.map_or(Decimal::ZERO, |m| m.as_decimal()),
        };
        log::info!(
            "FILL {}",
            serde_json::to_string(&record).unwrap_or_default()
        );
        if let Ok(mut report) = self.report.lock() {
            report.fills.push(record);
        }
        let terminal = self
            .cache()
            .order(&event.client_order_id)
            .is_some_and(|order| order.is_closed());
        if terminal && let Err(e) = self.filled_terminal(event.client_order_id) {
            self.halt(&e);
        }
    }

    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if event.instrument_id == self.config.instrument_id
            && let Err(e) = self.terminal(event.client_order_id)
        {
            self.halt(&e);
        }
    }

    fn on_order_rejected(&mut self, event: OrderRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.handle_terminal_failure(event.client_order_id, event.reason.as_str());
        }
    }

    fn on_order_denied(&mut self, event: OrderDenied) {
        if event.instrument_id == self.config.instrument_id {
            self.handle_terminal_failure(event.client_order_id, event.reason.as_str());
        }
    }

    fn on_order_expired(&mut self, event: OrderExpired) {
        if event.instrument_id == self.config.instrument_id {
            self.handle_terminal_failure(event.client_order_id, "order expired");
        }
    }

    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if event.instrument_id == self.config.instrument_id
            && self.pending_order == Some(event.client_order_id)
        {
            self.record_halt(&anyhow::anyhow!(
                "cancel rejected for {}: {}",
                event.client_order_id,
                event.reason,
            ));
        }
    }

    fn on_position_opened(&mut self, event: PositionOpened) {
        if event.instrument_id == self.config.instrument_id
            && let Err(e) = self.sync_target()
        {
            self.halt(&e);
        }
    }

    fn on_position_closed(&mut self, event: PositionClosed) {
        log::info!(
            "POSITION_CLOSED instrument={} realized_pnl={:?} realized_return={}",
            event.instrument_id,
            event.realized_pnl,
            event.realized_return
        );
        if event.instrument_id == self.config.instrument_id {
            self.entry_equity = None;
            if let Err(e) = self.sync_target() {
                self.halt(&e);
            }
        }
    }
});

impl DataActor for IntradayMomentumStrategy {
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if quote.instrument_id == self.config.instrument_id
            && let Err(e) = self.enforce_quote_risk(quote)
        {
            self.halt(&e);
        }
        if quote.instrument_id == self.config.instrument_id
            && self.awaiting_quote
            && quote.ts_event > self.signal_timestamp
        {
            self.awaiting_quote = false;
            if let Err(e) = self.sync_target() {
                self.halt(&e);
            }
        }
        Ok(())
    }
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.cache().try_instrument(&self.config.instrument_id)?;
        anyhow::ensure!(
            self.cache()
                .positions_open(None, Some(&self.config.instrument_id), None, None, None)
                .is_empty(),
            "restart recovery is not implemented; start with no open position in the instrument",
        );
        anyhow::ensure!(
            self.cache()
                .orders_open(None, Some(&self.config.instrument_id), None, None, None)
                .is_empty(),
            "restart recovery is not implemented; start with no open order in the instrument",
        );
        self.subscribe_bars(self.bar_type(), None, None);
        self.subscribe_quotes(self.config.instrument_id, None, None);
        self.subscribe_data(
            DataType::new(BarWithVwap::type_name_static(), None, None),
            None,
            None,
        );
        let now = self.clock().timestamp_ns();
        for session in &self.config.sessions {
            let flatten = UnixNanos::from(
                session.close.as_u64() - self.config.flatten_before_close_minutes * MINUTE,
            );
            if flatten > now {
                self.clock().set_time_alert_ns(
                    &format!("{FLATTEN_ALERT_PREFIX}{}", session.open),
                    flatten,
                    None,
                    None,
                )?;
            }
            if session.close > now {
                self.clock().set_time_alert_ns(
                    &format!("INTRADAY_VERIFY_{}", session.open),
                    UnixNanos::from(session.close.as_u64() + 1),
                    None,
                    None,
                )?;
            }
        }
        if let Some(end) = self.config.sessions.last().map(|s| s.close) {
            self.clock().set_timer_ns(
                "INTRADAY_WATCHDOG",
                MINUTE,
                None,
                Some(end),
                None,
                None,
                None,
            )?;
        }
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        if self.config.dry_run {
            return self.flatten();
        }
        self.halted = true;
        self.flatten()?;
        // A pending entry must reach a terminal event before flatten can size its exit.
        self.awaiting_quote = false;
        self.sync_target()?;
        self.unsubscribe_bars(self.bar_type(), None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if !self.config.allow_ohlc_vwap_approximation {
            return Ok(());
        }
        if bar.bar_type != self.bar_type() {
            return Ok(());
        }
        let Some(bar) = self.accept_bar(*bar) else {
            return Ok(());
        };
        if self.config.session(bar.ts_event).is_none() {
            return Ok(());
        }
        self.last_bar = Some(bar.ts_event);
        match self.model.on_bar(bar) {
            Ok(Some(decision)) => {
                if let Err(e) = self.apply_decision(decision) {
                    self.halt(&e);
                }
            }
            Ok(None) => {}
            Err(e) => self.halt(&e),
        }
        Ok(())
    }

    fn on_data(&mut self, data: &CustomData) -> anyhow::Result<()> {
        let Some(value) = data.data.as_any().downcast_ref::<BarWithVwap>() else {
            return Ok(());
        };
        if self.config.allow_ohlc_vwap_approximation
            || value.bar.instrument_id() != self.config.instrument_id
        {
            return Ok(());
        }
        if self.config.session(value.bar.ts_event).is_none()
            || self.last_bar.is_some_and(|last| value.bar.ts_event <= last)
        {
            return Ok(());
        }
        match self.model.on_bar_with_vwap(value.bar, value.vwap) {
            Ok(decision) => {
                self.last_bar = Some(value.bar.ts_event);
                if let Some(f) = self.model.latest_features()
                    && f.decision_time
                {
                    log::info!(
                        "SIGNAL {}",
                        serde_json::to_string(
                            &serde_json::json!({"instrument":self.config.instrument_id,"bar":value.bar,"features":f})
                        )?
                    );
                }
                if self.config.retain_features
                    && let Some(features) = self.model.latest_features()
                {
                    self.report
                        .lock()
                        .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
                        .features
                        .push(features);
                }
                if let Some(decision) = decision
                    && let Err(e) = self.apply_decision(decision)
                {
                    self.halt(&e);
                }
            }
            Err(e) => self.halt(&e),
        }
        Ok(())
    }

    fn on_time_event(&mut self, event: &TimeEvent) -> anyhow::Result<()> {
        if event.name.as_str() == "INTRADAY_WATCHDOG" {
            let now = self.clock().timestamp_ns();
            if !self.halted
                && let Some(id) = self.pending_order
                && let Some(context) = self.order_contexts.get(&id)
                && now.as_u64().saturating_sub(context.submitted_at.as_u64())
                    > self.config.order_timeout_seconds * 1_000_000_000
            {
                let order = self.cache().order(&id);
                self.record_halt(&anyhow::anyhow!(
                    "order acknowledgement/fill timeout: {id}; reconcile before retry"
                ));
                if let Some(order) = order {
                    self.query_order(&order, None, None)?;
                }
                self.cancel_stale_entry()?;
            }
            if let Some(session) = self.config.session(now) {
                let latest = self.last_bar.unwrap_or(session.open).max(session.open);
                if now.as_u64().saturating_sub(latest.as_u64())
                    > self.config.stale_data_seconds * 1_000_000_000
                    && !self.halted
                {
                    self.halt(&anyhow::anyhow!("stale or disconnected market data"));
                }
            }
        }
        if event.name.as_str().starts_with("INTRADAY_VERIFY_")
            && !self.config.dry_run
            && (self.pending_order.is_some() || self.position()?.is_some())
        {
            self.record_halt(&anyhow::anyhow!("ALERT: EOD position/order remains open"));
        }
        if event.name.as_str().starts_with(FLATTEN_ALERT_PREFIX)
            && let Err(e) = self.flatten()
        {
            self.halt(&e);
        }
        Ok(())
    }
}

impl Debug for IntradayMomentumStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(IntradayMomentumStrategy))
            .field("instrument_id", &self.config.instrument_id)
            .field("warmed_sessions", &self.model.warmed_sessions())
            .field("desired", &self.desired)
            .field("halted", &self.halted)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use nautilus_common::{
        cache::Cache,
        clock::{Clock, TestClock},
    };
    use nautilus_model::identifiers::TraderId;
    use nautilus_portfolio::portfolio::Portfolio;

    use super::*;

    fn strategy() -> IntradayMomentumStrategy {
        let mut value = IntradayMomentumStrategy::new(IntradayMomentumConfig {
            sessions: vec![super::super::IntradayMomentumSession {
                open: 0.into(),
                close: (90 * MINUTE).into(),
                dividend: Decimal::ZERO,
            }],
            ..Default::default()
        })
        .unwrap();
        let clock: Rc<RefCell<dyn Clock>> = Rc::new(RefCell::new(TestClock::new()));
        let cache = Rc::new(RefCell::new(Cache::default()));
        let instrument = nautilus_model::instruments::InstrumentAny::Equity(
            nautilus_model::instruments::Equity::builder()
                .instrument_id("SPY.SIM".into())
                .raw_symbol("SPY".into())
                .currency(Currency::USD())
                .price_precision(2)
                .price_increment(Price::from("0.01"))
                .lot_size(Quantity::from(1))
                .ts_event(0.into())
                .ts_init(0.into())
                .build()
                .unwrap(),
        );
        cache.borrow_mut().add_instrument(instrument).unwrap();
        let portfolio = Rc::new(RefCell::new(Portfolio::new(
            clock.clone(),
            cache.clone(),
            None,
        )));
        value
            .core
            .register(TraderId::from("TEST-001"), clock, cache, portfolio)
            .unwrap();
        value
    }
    #[test]
    fn dry_run_duplicate_target_does_not_create_another_order() {
        let mut value = strategy();
        value.desired = PositionTarget::Long;
        value.desired_open = Some(Price::from("100.00"));
        value.sizing_equity = Some((0.into(), Decimal::from(100_000)));
        value.sync_target().unwrap();
        value.sync_target().unwrap();
        assert_eq!(value.report.lock().unwrap().theoretical_orders, 1);
        assert_eq!(value.orders_today, 0);
        assert!(value.pending_order.is_none());
        assert!(
            value
                .cache()
                .orders_open(None, None, None, None, None)
                .is_empty()
        );
    }
    #[test]
    fn pending_order_blocks_duplicate_submission_and_unrelated_cancel() {
        let mut value = strategy();
        value.config.dry_run = false;
        let id = ClientOrderId::from("ENTRY-1");
        value.pending_order = Some(id);
        value.desired = PositionTarget::Short;
        value.sync_target().unwrap();
        value.terminal(ClientOrderId::from("OTHER-1")).unwrap();
        assert_eq!(value.pending_order, Some(id));
        assert_eq!(value.orders_today, 0);
    }

    #[rstest::rstest]
    fn entry_limit_counts_reversals_preserves_exits_and_resets_next_session() {
        let mut value = strategy();
        value.config.max_entries_per_day = Some(3);
        value.desired_open = Some(Price::from("100.00"));
        value.sizing_equity = Some((0.into(), Decimal::from(100_000)));
        for target in [
            PositionTarget::Long,
            PositionTarget::Short,
            PositionTarget::Long,
        ] {
            value.desired = target;
            value.sync_target().unwrap();
            value.sync_target().unwrap();
        }
        assert_eq!(value.entries_today, 3);
        assert_eq!(value.dry_target, PositionTarget::Long);
        value.desired = PositionTarget::Short;
        value.sync_target().unwrap();
        assert_eq!(value.dry_target, PositionTarget::Flat);
        value.desired = PositionTarget::Long;
        value.sync_target().unwrap();
        assert_eq!(value.dry_target, PositionTarget::Flat);
        assert_eq!(value.entries_today, 3);
        assert_eq!(value.report.lock().unwrap().theoretical_orders, 4);

        value
            .apply_decision(IntradayMomentumDecision {
                session_open: MINUTE.into(),
                timestamp: (31 * MINUTE).into(),
                target: PositionTarget::Long,
                open: Price::from("100.00"),
                close: Price::from("101.00"),
                vwap: Decimal::new(10050, 2),
                sigma_open: Decimal::new(1, 2),
                upper_bound: Decimal::new(10090, 2),
                lower_bound: Decimal::new(9910, 2),
                relative_volume: Some(Decimal::ONE),
                daily_volatility: 0.01,
                leverage: Decimal::ONE,
            })
            .unwrap();
        assert_eq!(value.entries_today, 1);
        assert_eq!(value.dry_target, PositionTarget::Long);
        assert_eq!(
            value.report.lock().unwrap().decisions[0].target,
            PositionTarget::Long
        );
        value.flatten().unwrap();
        assert_eq!(value.dry_target, PositionTarget::Flat);
        assert_eq!(value.entries_today, 1);
    }
    #[test]
    fn rejected_exit_halts_without_blind_resubmission() {
        let mut value = strategy();
        let id = ClientOrderId::from("EXIT-1");
        value.pending_order = Some(id);
        value.pending_closes_position = true;
        value.desired = PositionTarget::Long;
        value.handle_terminal_failure(id, "simulated broker rejection");
        assert!(value.halted);
        assert_eq!(value.desired, PositionTarget::Flat);
        assert_eq!(value.orders_today, 0);
        assert_eq!(value.report.lock().unwrap().errors.len(), 1);
    }
    #[test]
    fn flatten_is_idempotent_and_prevents_reentry_in_same_session() {
        let mut value = strategy();
        value.sizing_equity = Some((0.into(), Decimal::from(100_000)));
        value.dry_target = PositionTarget::Long;
        value.dry_quantity = Decimal::from(10);
        value.flatten().unwrap();
        value.flatten().unwrap();
        assert_eq!(value.flatten_session, Some(0.into()));
        assert_eq!(value.dry_target, PositionTarget::Flat);
        assert_eq!(value.report.lock().unwrap().theoretical_orders, 1);
    }
    #[rstest::rstest]
    fn daily_loss_limit_refuses_further_risk() {
        let mut value = strategy();
        value.sizing_equity = Some((0.into(), Decimal::from(100_000)));
        let quote = QuoteTick::new(
            "SPY.SIM".into(),
            Price::from("100.00"),
            Price::from("100.01"),
            Quantity::from(1000),
            Quantity::from(1000),
            MINUTE.into(),
            MINUTE.into(),
        );
        value.config.dry_run_equity = Decimal::from(99_000);
        value.enforce_quote_risk(&quote).unwrap();
        assert!(value.flatten_session.is_none());
        value.config.dry_run_equity = Decimal::from(96_000);
        value.enforce_quote_risk(&quote).unwrap();
        assert_eq!(value.flatten_session, Some(0.into()));
        assert!(
            !value.halted,
            "a daily loss locks the session, not every future day"
        );
        value.config.dry_run_equity = Decimal::from(99_000);
        value.enforce_quote_risk(&quote).unwrap();
        assert_eq!(
            value.flatten_session,
            Some(0.into()),
            "no same-day unlock after recovery"
        );
        assert_eq!(value.report.lock().unwrap().risk_events.len(), 1);
        assert!(value.report.lock().unwrap().errors.is_empty());
    }
    #[test]
    fn dry_run_respects_the_same_notional_cap_as_execution() {
        let mut value = strategy();
        value.desired = PositionTarget::Long;
        value.desired_open = Some(Price::from("100.00"));
        value.sizing_equity = Some((0.into(), Decimal::from(100_000)));
        value.desired_leverage = Decimal::from(4);
        value.config.max_order_notional = Decimal::from(15_000);
        value.sync_target().unwrap();
        assert_eq!(value.dry_quantity, Decimal::from(150));
    }
}
