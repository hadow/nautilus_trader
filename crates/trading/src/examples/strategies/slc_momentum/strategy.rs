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

//! 将行情、信号、风控和 Nautilus 原生订单/持仓事件串成完整策略生命周期。
//!
//! 每分钟先冻结并更新全部标的，再统一排名和评估信号；报价事件负责实际入场与持仓管理，
//! 从而避免股票事件到达顺序影响横截面结果。

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    sync::{Arc, Mutex},
};

use nautilus_common::{actor::DataActor, timer::TimeEvent};
use nautilus_core::UnixNanos;
use nautilus_model::{
    accounts::Account,
    data::{Bar, CustomData, QuoteTick},
    enums::{AggregationSource, BarAggregation, OmsType, TimeInForce},
    events::{
        OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired, OrderFilled,
        OrderModifyRejected, OrderRejected, PositionClosed,
    },
    identifiers::{ClientOrderId, InstrumentId, PositionId, TradeId},
    instruments::Instrument,
    orders::{Order, OrderAny},
    types::{Currency, Money, Price, Quantity},
};
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::Serialize;

use super::{
    CrossSectionalMomentumRanker, EntryMode, NoTrade, PortfolioRiskManager, RankSnapshot,
    RiskAllocation, SetupState, SlcMomentumConfig, SlcSignal, Structure, TradeSide,
    data::{MINUTE, MinuteBatch, SymbolFeatures, bar_type, minute_bar_type, validate_bar},
    risk::{rounded_price, trailing_stop},
    signal::{KeyLevelDetector, LevelKind, market_regime},
};
use crate::{
    nautilus_strategy,
    strategy::{Strategy, StrategyCore},
};

#[derive(Clone, Debug, Serialize)]
pub struct SlcTrade {
    pub signal: SlcSignal,
    pub allocation: RiskAllocation,
    pub opened_at: UnixNanos,
    pub closed_at: UnixNanos,
    pub entry_value: Decimal,
    pub exit_value: Decimal,
    pub quantity: Decimal,
    pub fees: Decimal,
    pub pnl: Decimal,
    pub native_booked_pnl: Decimal,
    pub exit_fill_count: usize,
    pub initial_risk: Decimal,
    pub r_multiple: Decimal,
    /// 持仓期间按可执行报价计算的最大有利价格偏移对应毛收益。
    pub mfe: Decimal,
    /// 持仓期间按可执行报价计算的最大不利价格偏移对应毛亏损幅度。
    pub mae: Decimal,
    pub mfe_r_multiple: Decimal,
    pub mae_r_multiple: Decimal,
    /// 净收益占 MFE 的比例；从未有利运行时为空。
    pub profit_capture_ratio: Option<Decimal>,
    pub excursion_state: String,
    pub exit_reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EquityObservation {
    pub timestamp: UnixNanos,
    pub session_open: UnixNanos,
    pub equity: Decimal,
    pub exposure: Decimal,
    pub turnover: Decimal,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SlcMomentumReport {
    pub observations: Vec<SlcObservation>,
    pub rankings: Vec<RankSnapshot>,
    pub signals: Vec<SlcSignal>,
    pub trades: Vec<SlcTrade>,
    pub equity: Vec<EquityObservation>,
    pub rejected: BTreeMap<NoTrade, u64>,
    pub entry_rejected: BTreeMap<NoTrade, u64>,
    pub errors: Vec<String>,
    pub turnover: Decimal,
    pub selections: Vec<super::UniverseSelection>,
    pub retained_symbols: BTreeSet<InstrumentId>,
    pub started: bool,
}

/// Completed five-minute research state; reference levels are not automatic entry zones.
#[derive(Clone, Debug, Serialize)]
pub struct SlcObservation {
    pub symbol: InstrumentId,
    pub timestamp: UnixNanos,
    pub available_at: UnixNanos,
    pub regime: Option<super::MarketRegime>,
    pub structure: Structure,
    pub momentum_percentile: Option<f64>,
    pub vwap: f64,
    pub stochastic_k: f64,
    pub stochastic_d: f64,
    pub demand: Vec<(Price, Price)>,
    pub supply: Vec<(Price, Price)>,
    pub prior_day_high: Option<Price>,
    pub prior_day_low: Option<Price>,
    pub opening_high: Option<Price>,
    pub opening_low: Option<Price>,
    pub swing_high: Option<Price>,
    pub swing_low: Option<Price>,
}

#[derive(Debug)]
struct ManagedTrade {
    signal: SlcSignal,
    allocation: RiskAllocation,
    entry_id: ClientOrderId,
    entry_terminal: bool,
    position_id: Option<PositionId>,
    entered: Decimal,
    exited: Decimal,
    entry_value: Decimal,
    exit_value: Decimal,
    fees: Decimal,
    native_booked_pnl: Decimal,
    exit_fill_count: usize,
    opened_at: Option<UnixNanos>,
    closed_at: Option<UnixNanos>,
    stops: BTreeSet<ClientOrderId>,
    exit_id: Option<ClientOrderId>,
    exit_reason: Option<String>,
    partial_exit: bool,
    partial_done: bool,
    favorable_extreme: Price,
    protective_bound: Price,
    mfe_per_share: Decimal,
    mae_per_share: Decimal,
    seen_fills: BTreeSet<TradeId>,
}

impl ManagedTrade {
    fn observe_excursion(&mut self, executable: Price) {
        if !self.entry_terminal || self.entered <= self.exited {
            return;
        }
        let average = self.entry_value / self.entered;
        let excursion = self.signal.side.sign() * (executable.as_decimal() - average);
        if excursion > self.mfe_per_share {
            self.mfe_per_share = excursion;
        } else if -excursion > self.mae_per_share {
            self.mae_per_share = -excursion;
        }
    }
}

pub(super) fn excursion_state(mfe: Decimal, pnl: Decimal) -> &'static str {
    if mfe == Decimal::ZERO {
        "NEVER_FAVORABLE"
    } else if pnl <= Decimal::ZERO {
        "FAVORABLE_UNCAPTURED"
    } else {
        "FAVORABLE_CAPTURED"
    }
}

pub(super) fn profit_capture_ratio(mfe: Decimal, pnl: Decimal) -> Option<Decimal> {
    if mfe > Decimal::ZERO {
        Some(pnl / mfe)
    } else {
        None
    }
}

/// Coordinates causal signal models with native Nautilus order and position events.
// ponytail: recovery state stays in memory; persist and reconcile fills before allowing restarts with exposure.
pub struct SlcMomentumStrategy {
    core: StrategyCore,
    settings: SlcMomentumConfig,
    features: BTreeMap<InstrumentId, SymbolFeatures>,
    levels: BTreeMap<InstrumentId, KeyLevelDetector>,
    ranker: CrossSectionalMomentumRanker,
    risk: PortfolioRiskManager,
    batch: MinuteBatch,
    ready: BTreeMap<InstrumentId, SlcSignal>,
    trades: BTreeMap<InstrumentId, ManagedTrade>,
    report: Arc<Mutex<SlcMomentumReport>>,
    last_session: Option<UnixNanos>,
    provider_pending: BTreeMap<InstrumentId, Bar>,
    stopped_on_error: bool,
    stopping: bool,
    selection: Option<super::UniverseSelection>,
    quote_subscriptions: BTreeSet<InstrumentId>,
}

impl Debug for SlcMomentumStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlcMomentumStrategy")
            .field("symbols", &self.features.len())
            .field("risk", &self.risk)
            .finish_non_exhaustive()
    }
}

impl SlcMomentumStrategy {
    /// Builds a validated strategy; dry-run is enabled by default.
    ///
    /// # Errors
    /// Returns an error for invalid configuration or unsupported instrument routing.
    pub fn new(mut settings: SlcMomentumConfig) -> anyhow::Result<Self> {
        if settings.ablation == super::Ablation::SlcOnly {
            settings.slc.require_htf_ema = false;
            settings.slc.require_intraday_trend = false;
        }
        settings.validate()?;
        settings.base.oms_type = Some(OmsType::Netting);
        // Shutdown uses the same acknowledged cancel/fill sequence as discretionary exits.
        settings.base.manage_stop = false;
        settings.base.market_exit_reduce_only = !settings.longbridge;
        let ids = settings.instrument_ids();
        let venue = settings.benchmarks[0].venue;
        anyhow::ensure!(
            ids.iter().all(|id| id.venue == venue),
            "one account venue is required"
        );
        let initial_ids = if settings.market.is_some() {
            settings
                .regime_benchmarks()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        } else {
            ids.into_iter().collect()
        };
        let features = initial_ids
            .into_iter()
            .map(|id| (id, SymbolFeatures::new(&settings.slc)))
            .collect();
        Ok(Self {
            core: StrategyCore::new(settings.base.clone()),
            settings,
            features,
            levels: BTreeMap::new(),
            ranker: CrossSectionalMomentumRanker::default(),
            risk: PortfolioRiskManager::default(),
            batch: MinuteBatch::default(),
            ready: BTreeMap::new(),
            trades: BTreeMap::new(),
            report: Arc::new(Mutex::new(SlcMomentumReport::default())),
            last_session: None,
            provider_pending: BTreeMap::new(),
            stopped_on_error: false,
            stopping: false,
            selection: None,
            quote_subscriptions: BTreeSet::new(),
        })
    }

    #[must_use]
    pub fn report_handle(&self) -> Arc<Mutex<SlcMomentumReport>> {
        Arc::clone(&self.report)
    }

    /// Warms completed daily and minute history before the configured trading start.
    ///
    /// # Errors
    /// Returns an error for future, malformed or unordered history.
    pub fn warmup(&mut self, mut bars: Vec<Bar>) -> anyhow::Result<()> {
        anyhow::ensure!(
            bars.iter().all(|b| b.ts_init < self.settings.trading_start),
            "warmup must precede trading_start"
        );
        self.seed_history(&mut bars)
    }

    fn seed_history(&mut self, bars: &mut [Bar]) -> anyhow::Result<()> {
        bars.sort_by_key(|b| (b.ts_event, b.bar_type.instrument_id()));
        for &bar in bars.iter() {
            if bar.bar_type.spec().aggregation == BarAggregation::Day {
                self.ranker.update_daily(bar, bar.ts_init)?;
            } else {
                validate_bar(&bar, bar.ts_init)?;
                anyhow::ensure!(
                    bar.bar_type == minute_bar_type(bar.bar_type.instrument_id()),
                    "warmup accepts only daily and completed one-minute bars"
                );
                if let Some(session) = self.settings.session(bar.ts_event)
                    && let Some(f) = self.features.get_mut(&bar.bar_type.instrument_id())
                {
                    if let Some(previous) = f.last {
                        if previous == bar {
                            continue;
                        }
                        anyhow::ensure!(
                            bar.ts_event > previous.ts_event,
                            "conflicting or late warmup minute"
                        );
                    }
                    let id = bar.bar_type.instrument_id();
                    if self.settings.market.is_some() && f.session != Some(session) {
                        self.levels.remove(&id);
                    }
                    if let Some(ltf) = f.update(bar, session, &self.settings.slc) {
                        if self.settings.market.is_some() {
                            self.levels.entry(id).or_default().update(
                                ltf,
                                f,
                                &self.settings.slc,
                                self.settings.ablation,
                            );
                        }
                        f.record_ltf_volume(ltf);
                    }
                }
            }
        }
        Ok(())
    }

    fn ingest_market(&mut self, update: &super::MarketUpdate) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.settings.market.is_some(),
            "unexpected market update in local ranking mode"
        );
        let now = self.clock().timestamp_ns();
        anyhow::ensure!(update.timestamp <= now, "future market update");
        if let Some(selection) = &update.selection {
            selection.validate(&self.settings, update.timestamp)?;
            anyhow::ensure!(
                self.selection
                    .as_ref()
                    .is_none_or(|old| selection.available_at > old.available_at),
                "selection must advance publication time"
            );
            self.ranker.snapshot = selection.ranking.clone();
            self.selection = Some(selection.clone());
            self.ready
                .retain(|id, signal| selection.permits_side(*id, now, signal.side, &self.settings));
            self.report
                .lock()
                .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
                .selections
                .push(selection.clone());
        }
        let mut active = self
            .settings
            .regime_benchmarks()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if let Some(selection) = &self.selection {
            active.extend(&selection.candidates);
        }
        active.extend(self.trades.keys());
        anyhow::ensure!(
            update.warmup_symbols.is_subset(&active),
            "warmup contains non-candidates"
        );
        for id in &update.warmup_symbols {
            if self.trades.contains_key(id) {
                continue;
            }
            self.features
                .insert(*id, SymbolFeatures::new(&self.settings.slc));
            self.levels.remove(id);
            self.ready.remove(id);
        }
        let mut seed = Vec::new();
        let mut groups = BTreeMap::<UnixNanos, Vec<Bar>>::new();
        for &bar in &update.bars {
            validate_bar(&bar, update.timestamp)?;
            let id = bar.bar_type.instrument_id();
            if bar.bar_type.spec().aggregation == BarAggregation::Day {
                anyhow::ensure!(
                    self.settings
                        .sessions
                        .iter()
                        .any(|s| s.close == bar.ts_event),
                    "invalid daily close"
                );
                seed.push(bar);
            } else {
                anyhow::ensure!(
                    bar.bar_type == minute_bar_type(id),
                    "unexpected market minute"
                );
                if !active.contains(&id) {
                    anyhow::ensure!(
                        self.settings.universe.iter().any(|m| m.instrument_id == id),
                        "unknown retiring symbol"
                    );
                    continue;
                }
                if update.warmup_symbols.contains(&id) && !self.trades.contains_key(&id) {
                    seed.push(bar);
                } else {
                    if update.warmup_symbols.contains(&id)
                        && self
                            .features
                            .get(&id)
                            .and_then(|f| f.last)
                            .is_some_and(|b| bar.ts_event <= b.ts_event)
                    {
                        continue;
                    }
                    anyhow::ensure!(
                        self.features
                            .get(&id)
                            .and_then(|f| f.last)
                            .is_none_or(|b| b.ts_event < bar.ts_event),
                        "late or duplicate market minute"
                    );
                    groups.entry(bar.ts_event).or_default().push(bar);
                }
            }
        }
        self.seed_history(&mut seed)?;
        if let Some(session) = self.settings.session(update.timestamp) {
            self.levels.retain(|id, _| {
                self.features
                    .get(id)
                    .is_some_and(|f| f.session == Some(session))
            });
            self.last_session = Some(session.open);
        }
        self.features.retain(|id, _| active.contains(id));
        self.levels.retain(|id, _| active.contains(id));
        for (_, bars) in groups {
            self.process_bars(bars, update.timestamp)?;
        }
        self.ready.retain(|_, signal| {
            now.as_u64()
                <= signal.available_at.as_u64() + self.settings.entry_timeout_minutes * MINUTE
        });
        let pending =
            self.trades
                .iter()
                .filter(|(id, t)| {
                    !t.entry_terminal
                        && (!self.selection.as_ref().is_some_and(|s| {
                            s.permits_side(**id, now, t.signal.side, &self.settings)
                        }) || now.as_u64()
                            > t.signal.available_at.as_u64()
                                + self.settings.entry_timeout_minutes * MINUTE)
                })
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
        for id in pending {
            self.request_exit(id, "ENTRY_SELECTION_EXPIRED", false)?;
        }
        let retained = self.trades.keys().copied().collect::<BTreeSet<_>>();
        self.report
            .lock()
            .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
            .retained_symbols
            .clone_from(&retained);
        let mut wanted = retained;
        wanted.extend(self.ready.keys());
        for id in std::mem::take(&mut self.quote_subscriptions) {
            if wanted.contains(&id) {
                self.quote_subscriptions.insert(id);
            } else {
                self.unsubscribe_quotes(id, None, None);
            }
        }
        Ok(())
    }

    fn ingest_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let now = self.clock().timestamp_ns();
        let mut bar = *bar;
        if self.settings.longbridge {
            // Longbridge publishes start-stamped revisions; only the next bar finalizes a minute
            if bar.bar_type.spec().aggregation == BarAggregation::Day {
                return Ok(());
            }
            let id = bar.bar_type.instrument_id();
            let previous = self.provider_pending.get(&id).copied();
            if previous.is_some_and(|b| b.ts_event > bar.ts_event) {
                return Ok(());
            }
            self.provider_pending.insert(id, bar);
            let Some(previous) = previous.filter(|b| b.ts_event < bar.ts_event) else {
                return Ok(());
            };
            bar = Bar::new(
                previous.bar_type,
                previous.open,
                previous.high,
                previous.low,
                previous.close,
                previous.volume,
                UnixNanos::from(previous.ts_event.as_u64() + MINUTE),
                now,
            );
        }
        validate_bar(&bar, now)?;
        if bar.bar_type.spec().aggregation == BarAggregation::Day {
            anyhow::ensure!(
                self.settings
                    .sessions
                    .iter()
                    .any(|s| s.close == bar.ts_event),
                "daily bar must be stamped at the actual regular-session close"
            );
            return self.ranker.update_daily(bar, now);
        }
        anyhow::ensure!(
            bar.bar_type == minute_bar_type(bar.bar_type.instrument_id()),
            "only completed one-minute input is accepted"
        );
        if self.settings.session(bar.ts_event).is_none() {
            return Ok(());
        }
        if let Some(previous) = self
            .features
            .get(&bar.bar_type.instrument_id())
            .and_then(|f| f.last)
            && bar.ts_event <= previous.ts_event
        {
            let mut normalized = bar;
            normalized.ts_init = previous.ts_init;
            anyhow::ensure!(
                normalized == previous,
                "late or revised minute cannot amend warmed or frozen features"
            );
            return Ok(());
        }
        self.flush(bar.ts_event)?;
        self.batch.push(bar)
    }

    fn ingest_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if quote.bid_price <= Price::from("0")
            || quote.ask_price < quote.bid_price
            || quote.ts_event > self.clock().timestamp_ns()
        {
            self.reject(quote.instrument_id, quote.ts_event, NoTrade::QuoteStale);
            return Ok(());
        }
        self.flush(quote.ts_event)?;
        if let Some(session) = self.settings.session(quote.ts_event)
            && let Ok((equity, _)) = self.account()
        {
            self.risk
                .mark_equity(session.open, equity.as_decimal(), &self.settings.risk);
        }
        if let Some(trade) = self.trades.get_mut(&quote.instrument_id) {
            let executable = if trade.signal.side == TradeSide::Long {
                quote.bid_price
            } else {
                quote.ask_price
            };
            trade.observe_excursion(executable);
        }
        self.manage(quote)?;
        self.enter(quote)?;
        if self.settings.market.is_some()
            && let Ok(mut report) = self.report.lock()
        {
            report.retained_symbols = self.trades.keys().copied().collect();
        }
        Ok(())
    }

    // Own the identifiers before exit callbacks can mutate the trade map.
    fn trade_symbols(&self) -> Vec<InstrumentId> {
        self.trades.keys().copied().collect()
    }

    fn reject(&self, id: InstrumentId, ts: UnixNanos, reason: NoTrade) {
        log::debug!("SLC NO_TRADE symbol={id} timestamp={ts} reason={reason:?}");
        if let Ok(mut report) = self.report.lock() {
            *report.rejected.entry(reason).or_default() += 1;
            if self.trading_window(ts) {
                *report.entry_rejected.entry(reason).or_default() += 1;
            }
        }
    }

    fn failure(&mut self, e: &anyhow::Error) {
        self.stopped_on_error = true;
        log::error!("SLC halted: {e:#}");
        if let Ok(mut report) = self.report.lock() {
            report.errors.push(format!("{e:#}"));
        }
    }

    fn account(&self) -> anyhow::Result<(Money, Decimal)> {
        let venue = self.settings.benchmarks[0].venue;
        let currency = Currency::USD();
        let equity = self
            .portfolio()
            .equity(&venue, None)
            .get(&currency)
            .copied()
            .or_else(|| {
                self.cache()
                    .account_for_venue(&venue)
                    .and_then(|a| a.balance_total(Some(currency)))
            })
            .ok_or_else(|| anyhow::anyhow!("USD account equity unavailable"))?;
        let free = self
            .cache()
            .account_for_venue(&venue)
            .and_then(|a| a.balance_free(Some(currency)))
            .ok_or_else(|| anyhow::anyhow!("USD free balance unavailable"))?
            .as_decimal();
        Ok((equity, free))
    }

    fn trading_window(&self, ts: UnixNanos) -> bool {
        let Some(session) = self.settings.session(ts) else {
            return false;
        };
        let elapsed = (ts.as_u64() - session.open.as_u64()) / MINUTE;
        ts >= self.settings.trading_start
            && ts < self.settings.trading_end
            && ts.as_u64()
                < session.close.as_u64() - self.settings.flatten_before_close_minutes * MINUTE
            && self
                .settings
                .trading_windows
                .iter()
                .any(|w| w[0] <= elapsed && elapsed < w[1])
    }

    fn flush(&mut self, watermark: UnixNanos) -> anyhow::Result<()> {
        let cutoff = UnixNanos::from(
            watermark
                .as_u64()
                .saturating_sub(self.settings.batch_delay_ms * 1_000_000),
        );
        let bars = self.batch.flush(cutoff);
        self.process_bars(bars, watermark)
    }

    fn process_bars(&mut self, bars: Vec<Bar>, watermark: UnixNanos) -> anyhow::Result<()> {
        let Some(first) = bars.first() else {
            return Ok(());
        };
        let ts = first.ts_event;
        let available = watermark.max(self.clock().timestamp_ns());
        let Some(session) = self.settings.session(ts) else {
            return Ok(());
        };
        if self.last_session != Some(session.open) {
            self.levels.clear();
            self.ready.clear();
            self.last_session = Some(session.open);
        }
        // 第一遍只更新所有标的特征，第二遍才排名和评估，保证同一分钟横截面对齐。
        let mut completed = Vec::new();
        let candidates = self
            .settings
            .universe
            .iter()
            .filter(|m| m.known_at <= ts && m.effective_from <= ts && ts < m.effective_until)
            .map(|m| m.instrument_id)
            .collect::<BTreeSet<_>>();
        for bar in bars {
            if let Some(f) = self.features.get_mut(&bar.bar_type.instrument_id())
                && let Some(ltf) = f.update(bar, session, &self.settings.slc)
            {
                completed.push(ltf);
            }
        }
        if self.settings.market.is_none() {
            self.ranker.refresh(ts, &self.settings, &self.features);
        }
        let regime = market_regime(&self.features, ts, &self.settings);
        let last_rank = self
            .report
            .lock()
            .ok()
            .and_then(|r| r.rankings.last().map(|s| s.timestamp));
        if last_rank != Some(self.ranker.snapshot.timestamp)
            && !self.ranker.snapshot.ranks.is_empty()
            && let Ok(mut report) = self.report.lock()
        {
            report.rankings.push(self.ranker.snapshot.clone());
        }
        for bar in completed {
            let id = bar.bar_type.instrument_id();
            let f = &self.features[&id];
            if candidates.contains(&id)
                && (self.settings.market.is_none()
                    || self
                        .selection
                        .as_ref()
                        .is_some_and(|s| s.permits(id, ts) && s.permits(id, available)))
            {
                let levels = self.levels.entry(id).or_default();
                levels.update(bar, f, &self.settings.slc, self.settings.ablation);
                if ts >= self.settings.trading_start && ts < self.settings.trading_end {
                    let prior = self.ranker.daily.get(&id).and_then(|bars| {
                        bars.iter()
                            .rev()
                            .find(|b| b.ts_event < session.open && b.ts_init <= ts)
                    });
                    let zones = |kind| {
                        levels
                            .levels
                            .iter()
                            .filter(|l| l.kind == kind && l.available())
                            .map(|l| (l.low, l.high))
                            .collect()
                    };
                    if let Ok(mut report) = self.report.lock() {
                        report.observations.push(SlcObservation {
                            symbol: id,
                            timestamp: ts,
                            available_at: available,
                            regime,
                            structure: f.structure.structure,
                            momentum_percentile: self
                                .ranker
                                .snapshot
                                .ranks
                                .get(&id)
                                .map(|r| r.percentile),
                            vwap: f.vwap.value,
                            stochastic_k: f.stochastic.value_k,
                            stochastic_d: f.stochastic.value_d,
                            demand: zones(LevelKind::Demand),
                            supply: zones(LevelKind::Supply),
                            prior_day_high: prior.map(|b| b.high),
                            prior_day_low: prior.map(|b| b.low),
                            opening_high: f.opening_high.filter(|_| f.opening_ready),
                            opening_low: f.opening_low.filter(|_| f.opening_ready),
                            swing_high: f.structure.swing_high,
                            swing_low: f.structure.swing_low,
                        });
                    }
                }
                let unranked = super::MomentumRank::unranked(id, ts);
                let rank = self
                    .ranker
                    .snapshot
                    .ranks
                    .get(&id)
                    .or_else(|| (!self.settings.ablation.momentum()).then_some(&unranked));
                let signal_regime = regime.or_else(|| {
                    (!self.settings.ablation.regime()).then_some(super::MarketRegime::Neutral)
                });
                let result = if let Some(rank) = rank {
                    if let Some(regime) = signal_regime {
                        levels.evaluate(bar, f, rank, regime, &self.settings, available)
                    } else {
                        Err(NoTrade::DataMissing)
                    }
                } else {
                    Err(self
                        .ranker
                        .snapshot
                        .excluded
                        .get(&id)
                        .copied()
                        .unwrap_or(NoTrade::MomentumWeak))
                };
                match result {
                    Ok(signal)
                        if self.trading_window(ts)
                            && (self.settings.market.as_ref().is_none_or(|m| {
                                available.as_u64().saturating_sub(ts.as_u64())
                                    <= m.max_bar_delay_seconds * 1_000_000_000
                            }))
                            && !self.trades.contains_key(&id)
                            && !self.stopping
                            && !self.stopped_on_error =>
                    {
                        // Keep room for held orders and other adapter subscriptions.
                        if self.settings.longbridge
                            && self.quote_subscriptions.len() >= 450
                            && !self.quote_subscriptions.contains(&id)
                        {
                            self.reject(id, ts, NoTrade::MaxExposure);
                            continue;
                        }
                        if let Ok(mut report) = self.report.lock() {
                            report.signals.push(signal.clone());
                        }
                        log::info!("SLC SIGNAL {}", serde_json::to_string(&signal)?);
                        self.ready.entry(id).or_insert(signal);
                        if self.settings.market.is_some() && self.quote_subscriptions.insert(id) {
                            self.subscribe_quotes(id, None, None);
                        }
                    }
                    Ok(_) => self.reject(id, ts, NoTrade::LateSession),
                    Err(reason) => self.reject(id, ts, reason),
                }
            }
            if let Some(f) = self.features.get_mut(&id) {
                f.record_ltf_volume(bar);
            }
        }
        if let Ok((equity, _)) = self.account() {
            self.risk
                .mark_equity(session.open, equity.as_decimal(), &self.settings.risk);
            let exposure = self
                .trades
                .iter()
                .map(|(id, t)| {
                    self.features
                        .get(id)
                        .and_then(|f| f.last)
                        .map_or(t.allocation.entry, |b| b.close)
                        .as_decimal()
                        * (t.entered - t.exited)
                })
                .sum();
            if let Ok(mut report) = self.report.lock() {
                let turnover = report.turnover;
                report.equity.push(EquityObservation {
                    timestamp: watermark,
                    session_open: session.open,
                    equity: equity.as_decimal(),
                    exposure,
                    turnover,
                });
            }
        }
        Ok(())
    }

    fn entry_order(&self, signal: &SlcSignal, allocation: &RiskAllocation) -> OrderAny {
        let id = signal.symbol;
        let q = allocation.quantity;
        let tags = (signal.side == TradeSide::Short).then(|| vec!["SHORT_ENTRY".into()]);
        match self.settings.entry_mode {
            EntryMode::Market => self.order().market(
                id,
                signal.side.entry_side(),
                q,
                Some(TimeInForce::Day),
                None,
                None,
                None,
                None,
                tags,
                None,
            ),
            EntryMode::Limit => self.order().limit(
                id,
                signal.side.entry_side(),
                q,
                allocation.entry,
                Some(TimeInForce::Day),
                None,
                Some(false),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                tags,
                None,
            ),
            EntryMode::Stop => self.order().stop_market(
                id,
                signal.side.entry_side(),
                q,
                signal.entry_price,
                None,
                Some(TimeInForce::Day),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                tags,
                None,
            ),
            EntryMode::StopLimit => self.order().stop_limit(
                id,
                signal.side.entry_side(),
                q,
                allocation.entry,
                signal.entry_price,
                None,
                Some(TimeInForce::Day),
                None,
                Some(false),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                tags,
                None,
            ),
        }
    }

    fn enter(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let id = quote.instrument_id;
        let Some(signal) = self.ready.get(&id).cloned() else {
            return Ok(());
        };
        if quote.ts_event <= signal.available_at {
            return Ok(());
        }
        if self.trades.contains_key(&id) {
            self.ready.remove(&id);
            return Ok(());
        }
        if self.settings.market.is_some()
            && !self.selection.as_ref().is_some_and(|s| {
                s.permits_side(id, quote.ts_event, signal.side, &self.settings)
                    && s.permits_side(id, self.clock().timestamp_ns(), signal.side, &self.settings)
            })
        {
            self.ready.remove(&id);
            self.reject(id, quote.ts_event, NoTrade::MomentumWeak);
            return Ok(());
        }
        if self.stopped_on_error
            || !self.trading_window(quote.ts_event)
            || quote.ts_event.as_u64()
                > signal.available_at.as_u64() + self.settings.entry_timeout_minutes * MINUTE
        {
            self.ready.remove(&id);
            self.reject(id, quote.ts_event, NoTrade::LateSession);
            return Ok(());
        }
        let now = self.clock().timestamp_ns();
        if quote.ts_event > now
            || now.as_u64().saturating_sub(quote.ts_event.as_u64())
                > self.settings.max_quote_age_ms * 1_000_000
            || quote.bid_price <= Price::from("0")
            || quote.ask_price < quote.bid_price
        {
            self.reject(id, now, NoTrade::QuoteStale);
            return Ok(());
        }
        let spread = (quote.ask_price.as_decimal() - quote.bid_price.as_decimal())
            / quote.bid_price.as_decimal()
            * Decimal::from(10_000);
        if spread > self.settings.max_spread_bps {
            self.reject(id, now, NoTrade::SpreadTooWide);
            return Ok(());
        }
        let instrument = self.cache().try_instrument(&id)?;
        let ceiling = signal.entry_price.as_decimal()
            * (Decimal::ONE
                + signal.side.sign() * self.settings.max_slippage_bps / Decimal::from(10_000));
        let executable = if signal.side == TradeSide::Long {
            quote.ask_price
        } else {
            quote.bid_price
        };
        if signal.side.sign() * (executable.as_decimal() - ceiling) > Decimal::ZERO {
            self.ready.remove(&id);
            self.reject(id, now, NoTrade::RiskTooHigh);
            return Ok(());
        }
        let entry = rounded_price(ceiling, &instrument, signal.side == TradeSide::Short)
            .map_err(|r| anyhow::anyhow!("entry price: {r:?}"))?;
        let (equity, free) = self.account()?;
        let session = self
            .settings
            .session(quote.ts_event)
            .ok_or_else(|| anyhow::anyhow!("entry outside session"))?;
        self.risk
            .mark_equity(session.open, equity.as_decimal(), &self.settings.risk);
        let mut correlated = 0;
        for other in self.risk.holdings.keys() {
            let Some(corr) = self.ranker.correlation(
                id,
                *other,
                session.open,
                self.settings.risk.correlation_lookback,
            ) else {
                self.ready.remove(&id);
                self.reject(id, now, NoTrade::CorrelationUnknown);
                return Ok(());
            };
            correlated += usize::from(corr.abs() >= self.settings.risk.correlation_threshold);
        }
        let liquidity = self.features[&id]
            .last
            .map_or(Decimal::ZERO, |b| b.volume.as_decimal());
        let structure_target = self.levels.get(&id).and_then(|l| {
            l.levels
                .iter()
                .filter(|l| l.available())
                .filter_map(|l| match signal.side {
                    TradeSide::Long if l.kind == LevelKind::Supply && l.low > entry => Some(l.low),
                    TradeSide::Short if l.kind == LevelKind::Demand && l.high < entry => {
                        Some(l.high)
                    }
                    _ => None,
                })
                .min_by_key(|p| (p.as_decimal() - entry.as_decimal()).abs())
        });
        let allocation = match self.risk.allocate(
            &signal,
            &instrument,
            entry,
            equity,
            free,
            correlated,
            &self.settings,
            liquidity,
            structure_target,
        ) {
            Ok(a) => a,
            Err(reason) => {
                self.ready.remove(&id);
                self.reject(id, now, reason);
                return Ok(());
            }
        };
        self.ready.remove(&id);
        log::info!(
            "SLC ENTRY signal={} allocation={}",
            serde_json::to_string(&signal)?,
            serde_json::to_string(&allocation)?
        );
        if self.settings.dry_run {
            return Ok(());
        }
        let order = self.entry_order(&signal, &allocation);
        let entry_id = order.client_order_id();
        self.risk
            .reserve(id, signal.sector.clone(), &allocation)
            .map_err(|r| anyhow::anyhow!("risk reservation: {r:?}"))?;
        self.trades.insert(
            id,
            ManagedTrade {
                favorable_extreme: allocation.entry,
                protective_bound: allocation.stop,
                mfe_per_share: Decimal::ZERO,
                mae_per_share: Decimal::ZERO,
                signal,
                allocation,
                entry_id,
                entry_terminal: false,
                position_id: None,
                entered: Decimal::ZERO,
                exited: Decimal::ZERO,
                entry_value: Decimal::ZERO,
                exit_value: Decimal::ZERO,
                fees: Decimal::ZERO,
                native_booked_pnl: Decimal::ZERO,
                exit_fill_count: 0,
                opened_at: None,
                closed_at: None,
                stops: BTreeSet::new(),
                exit_id: None,
                exit_reason: None,
                partial_exit: false,
                partial_done: false,
                seen_fills: BTreeSet::new(),
            },
        );
        if let Some(levels) = self.levels.get_mut(&id) {
            levels.state = SetupState::OrderSubmitted;
        }
        if let Err(e) = self.submit_order(order, None, None, None) {
            // An uncertain dispatch retains its reservation until reconciliation proves terminality
            self.failure(&e);
        }
        Ok(())
    }

    fn protect(&mut self, id: InstrumentId, quantity: Quantity) -> anyhow::Result<()> {
        let Some(t) = self.trades.get(&id) else {
            return Ok(());
        };
        let stop = t.protective_bound;
        let position = t.position_id;
        let order = if self.settings.longbridge {
            self.order().market_if_touched(
                id,
                t.signal.side.exit_side(),
                quantity,
                stop,
                None,
                Some(TimeInForce::Day),
                None,
                Some(false),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        } else {
            self.order().stop_market(
                id,
                t.signal.side.exit_side(),
                quantity,
                stop,
                None,
                Some(TimeInForce::Day),
                None,
                Some(true),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        };
        let stop_id = order.client_order_id();
        if let Some(t) = self.trades.get_mut(&id) {
            t.stops.insert(stop_id);
        }
        self.submit_order(order, position, None, None)
    }

    fn request_exit(
        &mut self,
        id: InstrumentId,
        reason: &str,
        partial: bool,
    ) -> anyhow::Result<()> {
        let Some(t) = self.trades.get_mut(&id) else {
            return Ok(());
        };
        if t.exit_reason.is_none() || !partial {
            t.exit_reason = Some(reason.to_string());
            t.partial_exit = partial;
        }
        let mut cancel = t.stops.iter().copied().collect::<Vec<_>>();
        if !t.entry_terminal {
            cancel.push(t.entry_id);
        }
        for order_id in cancel {
            let order = self.cache().order(&order_id);
            if let Some(order) = order
                && !order.is_closed()
                && !order.is_pending_cancel()
            {
                self.cancel_order(order_id, None, None)?;
            }
        }
        self.advance_exit(id)
    }

    fn advance_exit(&mut self, id: InstrumentId) -> anyhow::Result<()> {
        let Some(t) = self.trades.get(&id) else {
            return Ok(());
        };
        if t.exit_reason.is_none()
            || t.exit_id.is_some()
            || !t.entry_terminal
            || !t.stops.is_empty()
        {
            return Ok(());
        }
        let remaining = t.entered - t.exited;
        if remaining <= Decimal::ZERO {
            return Ok(());
        }
        let instrument = self.cache().try_instrument(&id)?;
        let lot = instrument
            .lot_size()
            .map_or(Decimal::ONE, |q| q.as_decimal())
            .max(Decimal::ONE);
        let partial_size = (remaining * self.settings.exit.partial_fraction / lot).floor() * lot;
        let quantity = if t.partial_exit && partial_size >= lot && remaining - partial_size >= lot {
            partial_size
        } else {
            remaining
        };
        let quantity = instrument.try_make_qty_from_decimal(quantity, Some(true))?;
        let position = t.position_id;
        let order = self.order().market(
            id,
            t.signal.side.exit_side(),
            quantity,
            Some(TimeInForce::Day),
            Some(!self.settings.longbridge),
            None,
            None,
            None,
            None,
            None,
        );
        if let Some(t) = self.trades.get_mut(&id) {
            t.exit_id = Some(order.client_order_id());
        }
        self.submit_order(order, position, None, None)
    }

    fn manage(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let id = quote.instrument_id;
        let Some(t) = self.trades.get(&id) else {
            return Ok(());
        };
        let ts = quote.ts_event;
        let expired = !t.entry_terminal
            && ts.as_u64()
                > t.signal.available_at.as_u64() + self.settings.entry_timeout_minutes * MINUTE;
        let late = self.settings.session(ts).is_none_or(|s| {
            ts.as_u64() >= s.close.as_u64() - self.settings.flatten_before_close_minutes * MINUTE
        }) || ts >= self.settings.trading_end;
        if self.risk.halted || self.stopped_on_error || late {
            return self.request_exit(id, if late { "SESSION_END" } else { "RISK_HALT" }, false);
        }
        if self.settings.market.is_some()
            && !t.entry_terminal
            && !self.selection.as_ref().is_some_and(|s| {
                s.permits_side(
                    id,
                    self.clock().timestamp_ns(),
                    t.signal.side,
                    &self.settings,
                )
            })
        {
            return self.request_exit(id, "MOMENTUM_CANDIDATE_REMOVED", false);
        }
        if expired {
            return self.request_exit(id, "ENTRY_TIMEOUT", false);
        }
        if let Some(reason) = t.exit_reason.clone() {
            return self.request_exit(id, &reason, t.partial_exit);
        }
        if t.entered <= t.exited {
            return Ok(());
        }
        let side = t.signal.side;
        let executable = if side == TradeSide::Long {
            quote.bid_price
        } else {
            quote.ask_price
        };
        let average = t.entry_value / t.entered;
        let initial_risk = side.sign() * (average - t.allocation.stop.as_decimal());
        let partial_price = average + side.sign() * initial_risk * self.settings.exit.partial_r;
        if side.sign() * (executable.as_decimal() - t.allocation.target.as_decimal())
            >= Decimal::ZERO
        {
            return self.request_exit(id, "TARGET", false);
        }
        if !t.partial_done
            && self.settings.exit.partial_fraction > Decimal::ZERO
            && side.sign() * (executable.as_decimal() - partial_price) >= Decimal::ZERO
        {
            return self.request_exit(id, "PARTIAL_R", true);
        }
        let f = &self.features[&id];
        let adverse_vwap = side.aligned(f.vwap.value - executable.as_f64());
        if self.settings.exit.vwap_loss && adverse_vwap {
            return self.request_exit(id, "VWAP_LOSS", false);
        }
        let weak = self.ranker.snapshot.ranks.get(&id).is_none_or(|r| {
            side.strength(t.signal.momentum.percentile) - side.strength(r.percentile)
                >= self.settings.exit.momentum_drop
        });
        let adverse_volume = f.last_ltf.is_some_and(|b| {
            side.aligned(b.open.as_f64() - b.close.as_f64())
                && f.relative_ltf_volume(b)
                    .is_some_and(|v| v >= self.settings.exit.selling_volume)
        });
        if self.settings.ablation.momentum()
            && self.settings.exit.momentum_failure
            && weak
            && adverse_vwap
            && adverse_volume
            && f.structure.structure != side.structure()
        {
            return self.request_exit(id, "MOMENTUM_FAILURE", false);
        }
        if self.settings.exit.trailing_enabled
            && t.entry_terminal
            && let Some(bar) = f.last_ltf
            && side.sign() * (executable.as_decimal() - average) > Decimal::ZERO
        {
            let instrument = self.cache().try_instrument(&id)?;
            // Chandelier extremes must be observed after entry, not inherited from
            // the pre-entry confirmation candle or the slippage budget price.
            let high = side.tighter(t.favorable_extreme, executable);
            let atr =
                Decimal::from_f64(f.atr.value).ok_or_else(|| anyhow::anyhow!("nonfinite ATR"))?;
            let acknowledged = t
                .stops
                .iter()
                .filter_map(|id| self.cache().order(id).and_then(|o| o.trigger_price()))
                .reduce(|a, b| side.tighter(a, b))
                .unwrap_or(t.protective_bound);
            let floor = side.tighter(t.protective_bound, acknowledged);
            let proposed = trailing_stop(
                floor,
                bar.close,
                high,
                atr,
                &instrument,
                &self.settings.exit,
                side,
            )
            .map_err(|r| anyhow::anyhow!("trailing stop: {r:?}"))?;
            let stops = t.stops.iter().copied().collect::<Vec<_>>();
            if let Some(t) = self.trades.get_mut(&id) {
                t.favorable_extreme = high;
                t.protective_bound = floor;
            }
            if side.sign() * (proposed.as_decimal() - executable.as_decimal()) >= Decimal::ZERO {
                return self.request_exit(id, "TRAILING_STOP", false);
            }
            for stop in stops {
                let change = self.cache().order(&stop).is_some_and(|o| {
                    !o.is_closed()
                        && !o.is_pending_update()
                        && o.trigger_price().is_some_and(|p| {
                            side.sign() * (proposed.as_decimal() - p.as_decimal()) > Decimal::ZERO
                        })
                });
                if change {
                    self.modify_order(stop, None, None, Some(proposed), None, None)?;
                }
            }
        }
        Ok(())
    }

    fn filled(&mut self, event: &OrderFilled) -> anyhow::Result<()> {
        let id = event.instrument_id;
        let terminal = self
            .cache()
            .order(&event.client_order_id)
            .is_some_and(|o| o.is_closed());
        let Some(t) = self.trades.get_mut(&id) else {
            return Ok(());
        };
        if !t.seen_fills.insert(event.trade_id) {
            return Ok(());
        }
        let qty = event.last_qty.as_decimal();
        let value = qty * event.last_px.as_decimal();
        t.fees += event.commission.map_or(Decimal::ZERO, |m| m.as_decimal());
        if let Ok(mut report) = self.report.lock() {
            report.turnover += value;
        }
        if event.client_order_id == t.entry_id {
            t.favorable_extreme = if t.entered == Decimal::ZERO {
                event.last_px
            } else {
                t.signal.side.tighter(t.favorable_extreme, event.last_px)
            };
            t.entered += qty;
            t.entry_value += value;
            t.position_id = event.position_id;
            t.opened_at.get_or_insert(event.ts_event);
            t.closed_at = None;
            t.entry_terminal |= terminal;
            let slipped = t.signal.side.sign()
                * (event.last_px.as_decimal() - t.allocation.entry.as_decimal())
                > Decimal::ZERO
                || t.signal.side.sign()
                    * (event.last_px.as_decimal() - t.allocation.stop.as_decimal())
                    <= Decimal::ZERO;
            let exiting = t.exit_reason.is_some();
            self.protect(id, event.last_qty)?;
            if slipped {
                self.failure(&anyhow::anyhow!("fill outside risk budget for {id}"));
                return self.request_exit(id, "FILL_OUTSIDE_RISK_BUDGET", false);
            }
            if exiting {
                return self.request_exit(id, "EXIT_DURING_ENTRY_FILL", false);
            }
            if let Some(levels) = self.levels.get_mut(&id) {
                levels.state = SetupState::PositionOpen;
            }
        } else {
            t.observe_excursion(event.last_px);
            t.exited += qty;
            t.exit_fill_count += 1;
            t.exit_value += value;
            anyhow::ensure!(
                t.exited <= t.entered,
                "exit quantity exceeds entry quantity"
            );
            if terminal {
                t.stops.remove(&event.client_order_id);
            }
            if t.exit_id == Some(event.client_order_id) && terminal {
                t.exit_id = None;
                if t.exited < t.entered {
                    t.partial_done = true;
                    t.partial_exit = false;
                    t.exit_reason = None;
                    let remaining = t.entered - t.exited;
                    let instrument = self.cache().try_instrument(&id)?;
                    self.protect(
                        id,
                        instrument.try_make_qty_from_decimal(remaining, Some(true))?,
                    )?;
                }
            } else if t.stops.contains(&event.client_order_id)
                || t.exit_id != Some(event.client_order_id)
            {
                t.exit_reason = Some("PROTECTIVE_STOP".to_string());
                t.partial_exit = false;
            }
            self.advance_exit(id)?;
        }
        Ok(())
    }

    fn terminal(
        &mut self,
        id: InstrumentId,
        order_id: ClientOrderId,
        failed: bool,
    ) -> anyhow::Result<()> {
        let already_stopped = self.stopped_on_error;
        let acknowledged_stop = self
            .cache()
            .order(&order_id)
            .and_then(|o| o.trigger_price());
        let Some(t) = self.trades.get_mut(&id) else {
            return Ok(());
        };
        if order_id == t.entry_id {
            t.entry_terminal = true;
            if t.entered == Decimal::ZERO {
                self.trades.remove(&id);
                self.risk.holdings.remove(&id);
                return Ok(());
            }
        } else if t.stops.remove(&order_id) {
            if let Some(stop) = acknowledged_stop {
                t.protective_bound = t.signal.side.tighter(t.protective_bound, stop);
            }
            if failed || t.exit_reason.is_none() {
                self.failure(&anyhow::anyhow!("protective order lost: {order_id}"));
                if already_stopped {
                    return Ok(());
                }
                return self.request_exit(id, "PROTECTION_LOST", false);
            }
        } else if t.exit_id == Some(order_id) {
            t.exit_id = None;
            if failed {
                let remaining = t.entered - t.exited;
                self.failure(&anyhow::anyhow!("exit order failed: {order_id}"));
                if !already_stopped && remaining > Decimal::ZERO {
                    let instrument = self.cache().try_instrument(&id)?;
                    self.protect(
                        id,
                        instrument.try_make_qty_from_decimal(remaining, Some(true))?,
                    )?;
                }
                return Ok(());
            }
        }
        self.advance_exit(id)?;
        self.finish_closed(id)
    }

    fn closed(&mut self, event: &PositionClosed) -> anyhow::Result<()> {
        let id = event.instrument_id;
        if let Some(t) = self.trades.get_mut(&id) {
            if t.closed_at.is_none() {
                t.native_booked_pnl += event.realized_pnl.map_or(Decimal::ZERO, |p| p.as_decimal());
            }
            t.closed_at = Some(event.ts_closed.unwrap_or(event.ts_event));
            t.exit_reason
                .get_or_insert_with(|| "PROTECTIVE_STOP".to_string());
        }
        let reason = self
            .trades
            .get(&id)
            .and_then(|t| t.exit_reason.clone())
            .unwrap_or_else(|| "POSITION_CLOSED".to_string());
        self.request_exit(id, &reason, false)?;
        self.finish_closed(id)
    }

    fn finish_closed(&mut self, id: InstrumentId) -> anyhow::Result<()> {
        if self.trades.get(&id).is_none_or(|t| {
            t.closed_at.is_none()
                || !t.entry_terminal
                || !t.stops.is_empty()
                || t.exit_id.is_some()
                || t.entered != t.exited
        }) {
            return Ok(());
        }
        let Some(t) = self.trades.remove(&id) else {
            return Ok(());
        };
        let pnl = t.signal.side.sign() * (t.exit_value - t.entry_value) - t.fees;
        let initial_risk =
            t.signal.side.sign() * (t.entry_value - t.entered * t.allocation.stop.as_decimal());
        let mfe = t.mfe_per_share * t.entered;
        let mae = t.mae_per_share * t.entered;
        let record = SlcTrade {
            signal: t.signal,
            allocation: t.allocation,
            opened_at: t.opened_at.unwrap_or_default(),
            closed_at: t.closed_at.unwrap_or_default(),
            entry_value: t.entry_value,
            exit_value: t.exit_value,
            quantity: t.entered,
            fees: t.fees,
            pnl,
            native_booked_pnl: t.native_booked_pnl,
            exit_fill_count: t.exit_fill_count,
            initial_risk,
            r_multiple: if initial_risk > Decimal::ZERO {
                pnl / initial_risk
            } else {
                Decimal::ZERO
            },
            mfe,
            mae,
            mfe_r_multiple: if initial_risk > Decimal::ZERO {
                mfe / initial_risk
            } else {
                Decimal::ZERO
            },
            mae_r_multiple: if initial_risk > Decimal::ZERO {
                mae / initial_risk
            } else {
                Decimal::ZERO
            },
            profit_capture_ratio: profit_capture_ratio(mfe, pnl),
            excursion_state: excursion_state(mfe, pnl).to_string(),
            exit_reason: t
                .exit_reason
                .unwrap_or_else(|| "PROTECTIVE_STOP".to_string()),
        };
        self.risk.closed(id, pnl, &self.settings.risk);
        log::info!("SLC EXIT {}", serde_json::to_string(&record)?);
        if let Ok(mut report) = self.report.lock() {
            report.trades.push(record);
        }
        if let Some(levels) = self.levels.get_mut(&id) {
            levels.state = SetupState::Exit;
        }
        Ok(())
    }
}

nautilus_strategy!(SlcMomentumStrategy, {
    fn stop(&mut self) -> bool {
        self.ready.clear();
        if self.trades.is_empty() {
            return true;
        }
        if !self.stopping {
            self.stopping = true;
            let scheduled = self.clock().set_timer_ns(
                "SLC_SHUTDOWN",
                1_000_000_000,
                None,
                None,
                None,
                None,
                None,
            );
            if let Err(e) = scheduled {
                self.failure(&e);
            }
        }
        for id in self.trade_symbols() {
            if let Err(e) = self.request_exit(id, "STRATEGY_STOP", false) {
                self.failure(&e);
            }
        }
        false
    }
    fn on_order_filled(&mut self, event: &OrderFilled) {
        if let Err(e) = self.filled(event) {
            self.failure(&e);
            let _ = self.request_exit(event.instrument_id, "FILL_HANDLER_ERROR", false);
        }
    }
    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if let Err(e) = self.terminal(event.instrument_id, event.client_order_id, false) {
            self.failure(&e);
        }
    }
    fn on_order_rejected(&mut self, event: OrderRejected) {
        if let Err(e) = self.terminal(event.instrument_id, event.client_order_id, true) {
            self.failure(&e);
        }
    }
    fn on_order_denied(&mut self, event: OrderDenied) {
        if let Err(e) = self.terminal(event.instrument_id, event.client_order_id, true) {
            self.failure(&e);
        }
    }
    fn on_order_expired(&mut self, event: OrderExpired) {
        if let Err(e) = self.terminal(event.instrument_id, event.client_order_id, false) {
            self.failure(&e);
        }
    }
    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        self.failure(&anyhow::anyhow!(
            "cancel rejected for {}: {}",
            event.client_order_id,
            event.reason
        ));
    }
    fn on_order_modify_rejected(&mut self, event: OrderModifyRejected) {
        self.failure(&anyhow::anyhow!(
            "stop modification rejected for {}: {}",
            event.client_order_id,
            event.reason
        ));
    }
    fn on_position_closed(&mut self, event: PositionClosed) {
        if let Err(e) = self.closed(&event) {
            self.failure(&e);
        }
    }
});

impl DataActor for SlcMomentumStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        let venue = self.settings.benchmarks[0].venue;
        anyhow::ensure!(
            self.cache()
                .positions_open(Some(&venue), None, None, None, None)
                .is_empty(),
            "start requires an isolated flat account; reconcile existing positions first"
        );
        anyhow::ensure!(
            self.cache()
                .orders_open(Some(&venue), None, None, None, None)
                .is_empty(),
            "start requires no pre-existing open orders"
        );
        for id in self.settings.instrument_ids() {
            let instrument = self.cache().try_instrument(&id)?;
            anyhow::ensure!(
                instrument.quote_currency() == Currency::USD(),
                "only USD instruments are supported"
            );
            if self.settings.market.is_some() {
                continue;
            }
            self.subscribe_bars(minute_bar_type(id), None, None);
            self.subscribe_bars(
                bar_type(id, 1, BarAggregation::Day, AggregationSource::External),
                None,
                None,
            );
            self.subscribe_quotes(id, None, None);
        }
        if self.settings.market.is_some() {
            self.subscribe_data(super::MarketUpdate::data_type(), None, None);
        }
        let now = self.clock().timestamp_ns();
        for session in self.settings.sessions.clone() {
            let flatten = UnixNanos::from(
                session.close.as_u64() - self.settings.flatten_before_close_minutes * MINUTE,
            );
            if flatten > now
                && flatten >= self.settings.trading_start
                && flatten <= self.settings.trading_end
            {
                self.clock().set_time_alert_ns(
                    &format!("SLC_FLATTEN_{}", session.open),
                    flatten,
                    None,
                    None,
                )?;
            }
        }
        self.report
            .lock()
            .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
            .started = true;
        Ok(())
    }

    fn on_data(&mut self, data: &CustomData) -> anyhow::Result<()> {
        if let Some(update) = data.data.as_any().downcast_ref::<super::MarketUpdate>()
            && let Err(e) = self.ingest_market(update)
        {
            self.failure(&e);
        }
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if let Err(e) = self.ingest_bar(bar) {
            self.failure(&e);
        }
        Ok(())
    }

    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if let Err(e) = self.ingest_quote(quote) {
            self.failure(&e);
        }
        Ok(())
    }

    fn on_time_event(&mut self, event: &TimeEvent) -> anyhow::Result<()> {
        if event.name.as_str() == "SLC_SHUTDOWN" {
            if self.trades.is_empty() {
                nautilus_common::component::Component::stop(self)?;
            } else {
                for id in self.trade_symbols() {
                    self.request_exit(id, "STRATEGY_STOP", false)?;
                }
            }
            return Ok(());
        }
        if event.name.as_str().starts_with("SLC_FLATTEN_") {
            self.ready.clear();
            for id in self.trade_symbols() {
                self.request_exit(id, "SESSION_END", false)?;
            }
        }
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.report
            .lock()
            .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
            .started = false;
        if self.stopping {
            self.clock().cancel_timer("SLC_SHUTDOWN");
        }
        if self.settings.market.is_some() {
            self.unsubscribe_data(super::MarketUpdate::data_type(), None, None);
            for id in self.quote_subscriptions.clone() {
                self.unsubscribe_quotes(id, None, None);
            }
            self.quote_subscriptions.clear();
            return Ok(());
        }
        for id in self.settings.instrument_ids() {
            self.unsubscribe_quotes(id, None, None);
            self.unsubscribe_bars(minute_bar_type(id), None, None);
            self.unsubscribe_bars(
                bar_type(id, 1, BarAggregation::Day, AggregationSource::External),
                None,
                None,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod terminal_failure_tests {
    use std::{cell::RefCell, rc::Rc};

    use nautilus_common::{
        cache::Cache,
        clock::{Clock, TestClock},
    };
    use nautilus_model::{
        identifiers::{ClientOrderId, TraderId},
        instruments::{InstrumentAny, stubs::equity_aapl},
        types::{Price, Quantity},
    };
    use nautilus_portfolio::portfolio::Portfolio;

    use super::*;

    #[test]
    fn terminal_failure_does_not_recursively_submit_after_halt() {
        let mut config = super::super::tests::config();
        config.benchmarks = ["SPY.XNAS".into(), "QQQ.XNAS".into(), "IWM.XNAS".into()];
        config.universe[0].sector_etf = "XLK.XNAS".into();
        let mut strategy = SlcMomentumStrategy::new(config).unwrap();
        let clock: Rc<RefCell<dyn Clock>> = Rc::new(RefCell::new(TestClock::new()));
        let cache = Rc::new(RefCell::new(Cache::default()));
        cache
            .borrow_mut()
            .add_instrument(InstrumentAny::Equity(equity_aapl()))
            .unwrap();
        let portfolio = Rc::new(RefCell::new(Portfolio::new(
            clock.clone(),
            cache.clone(),
            None,
        )));
        strategy
            .core
            .register(TraderId::from("TEST-001"), clock, cache, portfolio)
            .unwrap();

        let instrument_id = InstrumentId::from("AAPL.XNAS");
        let exit_id = ClientOrderId::from("O-EXIT-1");
        strategy.trades.insert(
            instrument_id,
            ManagedTrade {
                signal: super::super::tests::signal(),
                allocation: RiskAllocation {
                    entry: Price::from("100.00"),
                    stop: Price::from("99.00"),
                    target: Price::from("102.00"),
                    quantity: Quantity::from(1),
                    risk_per_share: Decimal::ONE,
                    reserved_risk: Decimal::ONE,
                    allocated_risk_fraction: Decimal::ONE,
                    notional: Decimal::from(100),
                },
                entry_id: ClientOrderId::from("O-ENTRY-1"),
                entry_terminal: true,
                position_id: None,
                entered: Decimal::ONE,
                exited: Decimal::ZERO,
                entry_value: Decimal::from(100),
                exit_value: Decimal::ZERO,
                fees: Decimal::ZERO,
                native_booked_pnl: Decimal::ZERO,
                exit_fill_count: 0,
                opened_at: Some(UnixNanos::default()),
                closed_at: None,
                stops: BTreeSet::new(),
                exit_id: Some(exit_id),
                exit_reason: Some("PROTECTION_LOST".to_string()),
                partial_exit: false,
                partial_done: false,
                favorable_extreme: Price::from("100.00"),
                protective_bound: Price::from("99.00"),
                mfe_per_share: Decimal::ZERO,
                mae_per_share: Decimal::ZERO,
                seen_fills: BTreeSet::new(),
            },
        );
        strategy.stopped_on_error = true;

        strategy.terminal(instrument_id, exit_id, true).unwrap();

        let trade = &strategy.trades[&instrument_id];
        assert!(trade.exit_id.is_none());
        assert!(trade.stops.is_empty());
    }
}
