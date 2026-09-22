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

//! One native strategy owns isolated instrument engines and a mandatory shared order gate.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    rc::Rc,
};

use indexmap::IndexMap;
use nautilus_common::{
    actor::DataActor, cache::Cache, messages::system::SocketStateChanged, timer::TimeEvent,
};
use nautilus_model::{
    accounts::Account,
    data::{
        Bar, BarType, CustomData, CustomDataTrait, DataType, QuoteTick, TradeTick,
        bar_vwap::BarWithVwap,
    },
    enums::{OmsType, OrderSide, PositionSide},
    events::{
        OrderAccepted, OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired,
        OrderFillVoided, OrderFilled, OrderRejected, OrderSubmitted,
    },
    identifiers::InstrumentId,
    instruments::Instrument,
    orders::{Order, OrderAny},
    position::Position,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    analytics::{EquityPoint, PerformanceTracker, number},
    config::GridConfig,
    orders::{GridOrder, OrderPhase},
    portfolio::{InstrumentExposure, OrderDecision, PortfolioConfig, PortfolioRiskManager},
    regime::MarketRegime,
    strategy::{DynamicGridConfig, GridState, GridStrategyEngine, StrategyState},
};
use crate::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};

#[cfg(test)]
mod tests;

/// Independent signal and sizing configuration; allocation is a fraction of initial shared capital.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentConfig {
    /// Disabled instruments remain recoverable and may reduce existing inventory.
    pub enabled: bool,
    /// Fixed ceiling for this sleeve; unused or reduced budgets remain shared cash reserve.
    pub capital_allocation: Decimal,
    /// Maximum instrument inventory and pending buys / total portfolio equity.
    #[serde(default = "default_position_fraction")]
    pub max_position_pct: Decimal,
    /// Explicit sector label; None belongs to the common Unknown sector.
    pub sector: Option<String>,
    /// This instrument's completed signal stream.
    pub bar_type: BarType,
    /// Local grid and risk parameters. Capital is supplied by the portfolio at construction.
    #[serde(default)]
    pub grid: GridConfig,
    /// Whether the adapter publishes confirmed custom bars.
    #[serde(default)]
    pub confirmed_custom_bars: bool,
    /// Whether quotes/trades, rather than bars, trigger execution decisions.
    #[serde(default)]
    pub tick_execution: bool,
}

/// A single account/quote-currency portfolio, with one native strategy identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiAssetGridConfig {
    /// Native identity, ownership and execution settings.
    pub base: StrategyConfig,
    /// Stable instrument keys; no signal, grid or order ledger is shared between these entries.
    pub instruments: BTreeMap<InstrumentId, InstrumentConfig>,
    /// Mandatory portfolio admission limits.
    #[serde(default)]
    pub portfolio: PortfolioConfig,
    /// One atomic checkpoint for every instrument and the shared risk state.
    pub state_path: Option<PathBuf>,
    /// Runner-owned environment/account identity.
    pub recovery_context: Option<String>,
}

fn default_position_fraction() -> Decimal {
    Decimal::new(20, 2)
}

impl MultiAssetGridConfig {
    /// Validates ownership, instrument keys, cash allocations and both risk layers.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid budgets, inconsistent bar keys, multiple venues or unsafe lifecycle settings.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.base.validate()?;
        self.portfolio.validate()?;
        anyhow::ensure!(
            !self.instruments.is_empty(),
            "At least one instrument is required"
        );
        anyhow::ensure!(
            self.base.strategy_id.is_some()
                && self.base.order_id_tag.is_some()
                && !self.base.manage_stop,
            "Explicit strategy identity required; automatic unmanaged flattening is unsupported"
        );
        let venue = self
            .instruments
            .keys()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty instrument universe"))?
            .venue;
        let mut total = Decimal::ZERO;
        for (id, instrument) in &self.instruments {
            instrument.grid.validate()?;
            anyhow::ensure!(
                id.venue == venue && instrument.bar_type.instrument_id() == *id,
                "Portfolio requires matching signal instruments on one account venue"
            );
            anyhow::ensure!(
                instrument.capital_allocation > Decimal::ZERO
                    && instrument.capital_allocation <= self.portfolio.max_instrument_allocation,
                "Invalid allocation for {id}"
            );
            anyhow::ensure!(
                instrument.max_position_pct > Decimal::ZERO
                    && instrument.max_position_pct <= Decimal::ONE
                    && self.portfolio.capital * instrument.capital_allocation > Decimal::ZERO,
                "Invalid instrument position fraction or funded capital"
            );
            anyhow::ensure!(
                instrument
                    .sector
                    .as_ref()
                    .is_none_or(|s| !s.trim().is_empty()),
                "Empty sector label"
            );
            total += instrument.capital_allocation;
        }
        anyhow::ensure!(
            total <= Decimal::ONE - self.portfolio.min_cash_reserve,
            "Initial allocations consume the portfolio cash reserve"
        );
        Ok(())
    }
}

impl From<DynamicGridConfig> for MultiAssetGridConfig {
    fn from(c: DynamicGridConfig) -> Self {
        let portfolio = PortfolioConfig {
            capital: c.grid.capital,
            max_instrument_allocation: Decimal::ONE,
            max_total_exposure: Decimal::ONE,
            max_total_grid_exposure: Decimal::ONE,
            max_total_equity_exposure: Decimal::ONE,
            min_cash_reserve: Decimal::ZERO,
            max_sector_exposure: Decimal::ONE,
            max_correlated_exposure: Decimal::ONE,
            max_portfolio_drawdown: c.grid.max_drawdown,
            max_portfolio_daily_loss: c.grid.max_daily_loss,
            trend_up_allocation_factor: Decimal::ONE,
            trend_down_allocation_factor: Decimal::ONE,
            allocation_pnl_weight: Decimal::ZERO,
            allocation_volatility_target: Decimal::ONE,
            ..Default::default()
        };
        Self {
            base: c.base,
            instruments: BTreeMap::from([(
                c.instrument_id,
                InstrumentConfig {
                    enabled: true,
                    capital_allocation: Decimal::ONE,
                    max_position_pct: Decimal::ONE,
                    sector: None,
                    bar_type: c.bar_type,
                    grid: c.grid,
                    confirmed_custom_bars: c.confirmed_custom_bars,
                    tick_execution: c.tick_execution,
                },
            )]),
            portfolio,
            state_path: c.state_path,
            recovery_context: c.recovery_context,
        }
    }
}

/// Portfolio and instrument PnL remain separately inspectable, without summing instrument Sharpe ratios.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioReport {
    /// Statistics computed from the shared marked-equity path.
    pub portfolio: PerformanceTracker,
    /// Independently funded attribution sleeves, including their open inventory.
    pub instruments: BTreeMap<InstrumentId, PerformanceTracker>,
    /// Shared loss limits, budget decisions and correlation history.
    pub risk: PortfolioRiskManager,
    /// Trailing completed-day Pearson matrix; None means insufficient aligned observations.
    pub correlations: BTreeMap<InstrumentId, BTreeMap<InstrumentId, Option<f64>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstrumentCheckpoint {
    state: GridState,
    performance: PerformanceTracker,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    config: MultiAssetGridConfig,
    instruments: BTreeMap<InstrumentId, InstrumentCheckpoint>,
    portfolio_risk: PortfolioRiskManager,
    portfolio_performance: PerformanceTracker,
    orders: Vec<OrderAny>,
    positions: Vec<Position>,
}

/// One native `StrategyCore`; instrument engines are state machines, never nested Strategy instances.
#[derive(Debug)]
pub struct MultiAssetGridStrategy {
    core: StrategyCore,
    config: MultiAssetGridConfig,
    engines: BTreeMap<InstrumentId, GridStrategyEngine>,
    portfolio_risk: PortfolioRiskManager,
    portfolio_performance: PerformanceTracker,
    report: Rc<RefCell<PortfolioReport>>,
    loaded: Option<Checkpoint>,
    _state_lock: Option<File>,
    recovering: bool,
    stopped: bool,
    last_account_query: u64,
    entries_resume_ns: u64,
}

impl MultiAssetGridStrategy {
    /// Builds all independent engines and locks their single atomic recovery file.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, incompatible/corrupt state or another checkpoint writer.
    pub fn new(config: impl Into<MultiAssetGridConfig>) -> anyhow::Result<Self> {
        let config = config.into();
        config.validate()?;
        let mut loaded = None;
        let mut state_lock = None;
        if let Some(path) = &config.state_path {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.with_extension("lock"))?;
            file.try_lock()
                .map_err(|e| anyhow::anyhow!("Checkpoint is already in use: {e}"))?;
            state_lock = Some(file);
            if path.exists() {
                let saved: Checkpoint = serde_json::from_reader(File::open(path)?)?;
                saved.validate(&config)?;
                loaded = Some(saved);
            }
        }
        let engines = config
            .instruments
            .iter()
            .map(|(id, c)| {
                let mut single = DynamicGridConfig::new(*id, c.bar_type);
                single.base = config.base.clone();
                single.grid = c.grid.clone();
                single.grid.capital = config.portfolio.capital * c.capital_allocation;
                single.confirmed_custom_bars = c.confirmed_custom_bars;
                single.tick_execution = c.tick_execution;
                single.state_path.clone_from(&config.state_path);
                let mut engine = GridStrategyEngine::new(single);
                if let Some(saved) = &loaded {
                    engine.state = saved.instruments[id].state.clone();
                    *engine.report.borrow_mut() = saved.instruments[id].performance.clone();
                }
                (*id, engine)
            })
            .collect();
        let risk = loaded.as_ref().map_or_else(
            || PortfolioRiskManager::new(config.portfolio.capital),
            |s| s.portfolio_risk.clone(),
        );
        let performance = loaded
            .as_ref()
            .map(|s| s.portfolio_performance.clone())
            .unwrap_or_default();
        let report = Rc::new(RefCell::new(PortfolioReport {
            portfolio: performance.clone(),
            instruments: BTreeMap::new(),
            risk: risk.clone(),
            correlations: BTreeMap::new(),
        }));
        Ok(Self {
            core: StrategyCore::new_checked(config.base.clone())?,
            config,
            engines,
            portfolio_risk: risk,
            portfolio_performance: performance,
            report,
            loaded,
            _state_lock: state_lock,
            recovering: true,
            stopped: false,
            last_account_query: 0,
            entries_resume_ns: 0,
        })
    }

    /// Returns the legacy single-instrument report handle.
    ///
    /// # Panics
    ///
    /// Panics for a portfolio; callers must use `portfolio_report_handle` or `instrument_report_handle`.
    #[must_use]
    pub fn report_handle(&self) -> Rc<RefCell<PerformanceTracker>> {
        assert_eq!(
            self.engines.len(),
            1,
            "Use portfolio_report_handle for multiple instruments"
        );
        Rc::clone(
            &self
                .engines
                .values()
                .next()
                .expect("Nonempty portfolio")
                .report,
        )
    }

    /// Returns one instrument's independent report without changing active instrument context.
    #[must_use]
    pub fn instrument_report_handle(
        &self,
        id: InstrumentId,
    ) -> Option<Rc<RefCell<PerformanceTracker>>> {
        self.engines.get(&id).map(|e| Rc::clone(&e.report))
    }

    /// Returns the complete shared report, finalized after the native stop/drain.
    #[must_use]
    pub fn portfolio_report_handle(&self) -> Rc<RefCell<PortfolioReport>> {
        Rc::clone(&self.report)
    }

    /// Returns the portfolio lifecycle barrier or the single-instrument compatibility state.
    #[must_use]
    pub fn state(&self) -> StrategyState {
        if self.stopped {
            StrategyState::Stopped
        } else if self
            .engines
            .values()
            .all(|e| e.state.state == StrategyState::Initializing)
        {
            StrategyState::Initializing
        } else if self.recovering {
            StrategyState::Recovering
        } else if self.portfolio_risk.risk_off_reason.is_some() {
            StrategyState::RiskOff
        } else if self.engines.len() == 1 {
            self.engines
                .values()
                .next()
                .map_or(StrategyState::Recovering, |e| e.state.state)
        } else {
            StrategyState::GridActive
        }
    }

    /// Returns one engine's lifecycle; another instrument cannot overwrite it.
    #[must_use]
    pub fn instrument_state(&self, id: InstrumentId) -> Option<StrategyState> {
        self.engines.get(&id).map(|e| e.state.state)
    }

    /// Restores all native orders and positions before broker reconciliation begins.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination is not empty or the native cache rejects a snapshot.
    pub fn restore_cache(&self, cache: &mut Cache) -> anyhow::Result<()> {
        if let Some(saved) = &self.loaded {
            for order in &saved.orders {
                anyhow::ensure!(
                    cache.order_ref(&order.client_order_id()).is_none(),
                    "Recovery destination already contains order"
                );
                cache.add_order(order.clone(), order.position_id(), None, false)?;
            }
            for position in &saved.positions {
                cache.add_position(position, OmsType::Netting)?;
            }
        }
        Ok(())
    }

    fn with_engine<T>(
        &mut self,
        id: InstrumentId,
        f: impl FnOnce(&mut GridStrategyEngine, &mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut engine = self
            .engines
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("Unknown instrument {id}"))?;
        let result = f(&mut engine, self);
        engine.report.borrow_mut().diagnostics.observe_stop(
            engine.state.risk.risk_off_reason.as_deref(),
            self.clock().timestamp_ns().as_u64(),
        );
        self.engines.insert(id, engine);
        result
    }

    fn exposures(&self, current: Option<&GridStrategyEngine>) -> Vec<InstrumentExposure> {
        self.engines
            .values()
            .chain(current)
            .map(|e| {
                let c = &self.config.instruments[&e.config.instrument_id];
                let mark = e.state.last_price.unwrap_or(Decimal::ZERO);
                let exposure = e.state.orders.inventory() * mark;
                InstrumentExposure {
                    id: e.config.instrument_id,
                    sector: c.sector.clone(),
                    base_allocation: c.capital_allocation,
                    max_position_pct: c.max_position_pct,
                    enabled: c.enabled,
                    cash_delta: e.state.orders.cash - e.config.grid.capital,
                    exposure,
                    pending: e.state.orders.buy_reservations(&e.config.grid, mark).1,
                    net_pnl: e.state.orders.cash + exposure - e.config.grid.capital,
                    regime: e.state.regime.snapshot.regime,
                    atr_pct: if mark > Decimal::ZERO {
                        Decimal::from_f64_retain(e.state.regime.snapshot.atr)
                            .unwrap_or(Decimal::ZERO)
                            / mark
                    } else {
                        Decimal::ZERO
                    },
                    risk_off: e.state.risk.risk_off_reason.is_some(),
                    mark_ns: e.state.last_market_ns,
                    max_age_secs: e.config.grid.max_signal_age_secs,
                }
            })
            .collect()
    }

    fn account_capacity(&self, current: &GridStrategyEngine) -> anyhow::Result<(Decimal, Decimal)> {
        let sid = self.config.base.strategy_id.expect("Validated identity");

        // Scoped native reads preserve all ownership checks without copying event histories
        let cache = self.core.cache_ref();
        for position in cache.positions_open_refs(None, None, None, None, None) {
            anyhow::ensure!(
                position.strategy_id == sid
                    && position.side == PositionSide::Long
                    && self
                        .config
                        .instruments
                        .contains_key(&position.instrument_id),
                "Unowned account inventory requires reconciliation"
            );
        }
        for order in cache.orders_open(None, None, None, None, None) {
            anyhow::ensure!(
                order.strategy_id() == sid
                    && self.config.instruments.contains_key(&order.instrument_id()),
                "Unowned account orders require reconciliation"
            );
            let owner = if order.instrument_id() == current.config.instrument_id {
                Some(current)
            } else {
                self.engines.get(&order.instrument_id())
            };
            anyhow::ensure!(
                owner.is_some_and(|e| e
                    .state
                    .orders
                    .orders()
                    .contains_key(order.client_order_id().as_str())),
                "Untracked account order requires reconciliation"
            );
        }
        let instrument = current
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let account = cache
            .account_for_venue(&current.config.instrument_id.venue)
            .ok_or_else(|| anyhow::anyhow!("Account unavailable"))?;
        let free = account
            .balance_free(Some(instrument.quote_currency()))
            .ok_or_else(|| anyhow::anyhow!("Account cash unavailable"))?
            .as_decimal();
        let account_id = account.id();
        drop(account);
        drop(cache);
        let equity = self
            .portfolio()
            .equity(&current.config.instrument_id.venue, Some(&account_id))
            .get(&instrument.quote_currency())
            .ok_or_else(|| anyhow::anyhow!("Account equity unavailable"))?
            .as_decimal();
        Ok((free, equity))
    }

    pub(super) fn gate(
        &mut self,
        current: &GridStrategyEngine,
        intent: &GridOrder,
    ) -> anyhow::Result<Decimal> {
        if self.recovering || self.stopped {
            return Ok(Decimal::ZERO);
        }
        if !intent.buy {
            let instrument = current
                .instrument
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
            let lot = instrument
                .lot_size()
                .unwrap_or(instrument.size_increment())
                .as_decimal()
                .max(instrument.size_increment().as_decimal());
            let price = intent
                .reference
                .max(current.state.last_price.unwrap_or(intent.reference));
            anyhow::ensure!(price > Decimal::ZERO, "Invalid reduction reference price");
            return Ok(super::engine::floor_tick(
                intent
                    .quantity
                    .min(self.config.portfolio.max_order_value / price),
                lot,
            ));
        }
        self.buy_quantity(
            current,
            intent.quantity,
            intent.reference,
            intent.limit,
            true,
        )
    }

    pub(super) fn buy_quantity(
        &mut self,
        current: &GridStrategyEngine,
        desired: Decimal,
        reference: Decimal,
        limit: Option<Decimal>,
        includes_intent: bool,
    ) -> anyhow::Result<Decimal> {
        let now = self.clock().timestamp_ns().as_u64();
        if self.recovering || self.stopped || now < self.entries_resume_ns {
            return Ok(Decimal::ZERO);
        }
        let previous_peak = self.portfolio_risk.peak_equity;
        let previous_day = self.portfolio_risk.day;
        let previous_reason = self.portfolio_risk.risk_off_reason.clone();
        let previous_allocations = self.portfolio_risk.allocations.clone();
        let mut views = self.exposures(Some(current));
        let cash =
            self.config.portfolio.capital + views.iter().map(|v| v.cash_delta).sum::<Decimal>();
        let equity = cash + views.iter().map(|v| v.exposure).sum::<Decimal>();
        let (free, broker_equity) = self.account_capacity(current)?;
        self.portfolio_risk
            .observe(&self.config.portfolio, equity.min(broker_equity), now);
        self.portfolio_risk
            .reallocate(&self.config.portfolio, &views, now);
        let c = &current.config.grid;
        let unit = limit.unwrap_or(reference.max(current.state.last_price.unwrap_or(reference)))
            * (Decimal::ONE + c.maker_fee.max(c.taker_fee) + c.commission + c.slippage);
        // This just-created intent is included in the ledger; exclude only itself from prior reservations
        let view = views
            .iter_mut()
            .find(|v| v.id == current.config.instrument_id)
            .expect("Current engine included");
        if includes_intent {
            view.pending -= desired * unit;
        }
        anyhow::ensure!(
            view.pending >= Decimal::ZERO,
            "Portfolio reservation accounting mismatch"
        );
        let instrument = current
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let lot = instrument
            .lot_size()
            .unwrap_or(instrument.size_increment())
            .as_decimal()
            .max(instrument.size_increment().as_decimal());
        let (decision, quantity) = self.portfolio_risk.admit(
            &self.config.portfolio,
            &views,
            current.config.instrument_id,
            equity.min(broker_equity),
            cash,
            free,
            desired,
            unit,
            lot,
            now,
        );
        if decision != OrderDecision::Allow {
            log::info!(
                "PORTFOLIO_ORDER_GATE instrument={} decision={decision:?} quantity={quantity}",
                current.config.instrument_id,
            );
        }
        if !includes_intent
            && (previous_peak != self.portfolio_risk.peak_equity
                || previous_day != self.portfolio_risk.day
                || previous_reason != self.portfolio_risk.risk_off_reason
                || previous_allocations != self.portfolio_risk.allocations)
        {
            self.persist_engine(current, None)?;
        }
        Ok(quantity)
    }

    pub(super) fn portfolio_blocked(&self) -> bool {
        self.recovering || self.portfolio_risk.risk_off_reason.is_some()
    }

    pub(super) fn portfolio_flatten(&self) -> bool {
        !self.recovering
            && self.portfolio_risk.risk_off_reason.is_some()
            && self.config.portfolio.risk_policy == super::config::RiskPolicy::Flatten
    }

    /// Latches an operator halt and immediately requests cancellation of acquisition orders.
    ///
    /// # Errors
    ///
    /// Returns an error if cancellation or durable state persistence fails; the halt stays latched.
    pub fn kill_switch(&mut self, reason: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !reason.trim().is_empty(),
            "Kill switch requires an audit reason"
        );
        self.portfolio_risk
            .trip(format!("Operator kill switch: {reason}"));
        self.persist(None, None)?;
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, |e, r| e.cancel(r, true, "OPERATOR_KILL_SWITCH"))?;
        }
        Ok(())
    }

    pub(super) fn halt_portfolio(&mut self, reason: String) {
        self.recovering = true;
        self.portfolio_risk.trip(reason);
    }

    fn halt_all(&mut self, reason: &str) {
        self.halt_portfolio(reason.to_string());
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            if let Err(e) = self.with_engine(id, |engine, runtime| {
                engine.halt(runtime, anyhow::anyhow!(reason.to_string()));
                Ok(())
            }) {
                log::error!("Portfolio emergency cancellation failed: {e}");
            }
        }
    }

    pub(super) fn observe_close(&mut self, id: InstrumentId, now: u64, price: Decimal) {
        self.portfolio_risk
            .close(&self.config.portfolio, id, now, price);
    }

    fn checkpoint(
        &self,
        current: Option<&GridStrategyEngine>,
        pending: Option<&OrderAny>,
    ) -> Checkpoint {
        let mut orders = self.cache().orders(
            None,
            None,
            self.config.base.strategy_id.as_ref(),
            None,
            None,
        );
        if let Some(order) = pending
            && !orders
                .iter()
                .any(|o| o.client_order_id() == order.client_order_id())
        {
            orders.push(order.clone());
        }
        orders.sort_by_key(Order::client_order_id);
        let instruments = self
            .engines
            .values()
            .chain(current)
            .map(|e| {
                (
                    e.config.instrument_id,
                    InstrumentCheckpoint {
                        state: e.state.clone(),
                        performance: e.report.borrow().clone(),
                    },
                )
            })
            .collect();
        Checkpoint {
            version: 5,
            config: self.config.clone(),
            instruments,
            portfolio_risk: self.portfolio_risk.clone(),
            portfolio_performance: self.portfolio_performance.clone(),
            orders,
            positions: self.cache().positions_open(
                None,
                None,
                self.config.base.strategy_id.as_ref(),
                None,
                None,
            ),
        }
    }

    fn persist(
        &self,
        current: Option<&GridStrategyEngine>,
        pending: Option<&OrderAny>,
    ) -> anyhow::Result<()> {
        let Some(path) = &self.config.state_path else {
            return Ok(());
        };
        let temporary = path.with_extension("next");
        let mut file = File::create(&temporary)?;
        serde_json::to_writer(&mut file, &self.checkpoint(current, pending))?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }

    pub(super) fn persist_engine(
        &self,
        engine: &GridStrategyEngine,
        pending: Option<&OrderAny>,
    ) -> anyhow::Result<()> {
        self.persist(Some(engine), pending)
    }

    fn observe_portfolio(&mut self, record: bool) -> anyhow::Result<()> {
        let now = self.clock().timestamp_ns().as_u64();
        let previous_peak = self.portfolio_risk.peak_equity;
        let previous_day = self.portfolio_risk.day;
        let previous_reason = self.portfolio_risk.risk_off_reason.clone();
        let previous_allocations = self.portfolio_risk.allocations.clone();
        let views = self.exposures(None);
        let cash =
            self.config.portfolio.capital + views.iter().map(|v| v.cash_delta).sum::<Decimal>();
        let exposure = views.iter().map(|v| v.exposure).sum::<Decimal>();
        let equity = cash + exposure;
        let account = self
            .engines
            .values()
            .next()
            .map(|e| self.account_capacity(e))
            .transpose()?;
        let risk_equity = account.map_or(equity, |(_, value)| equity.min(value));
        self.portfolio_risk
            .observe(&self.config.portfolio, risk_equity, now);
        self.portfolio_performance
            .diagnostics
            .observe_stop(self.portfolio_risk.risk_off_reason.as_deref(), now);
        self.portfolio_risk
            .reallocate(&self.config.portfolio, &views, now);
        self.portfolio_performance.observe_mark(
            self.config.portfolio.capital,
            equity,
            exposure,
            Decimal::ZERO,
            now,
        );
        if record {
            let pending: Decimal = views.iter().map(|v| v.pending).sum();
            let point = EquityPoint {
                trend_inventory: Some(
                    views
                        .iter()
                        .filter(|v| {
                            matches!(v.regime, MarketRegime::TrendUp | MarketRegime::TrendDown)
                        })
                        .map(|v| v.exposure)
                        .sum(),
                ),
                cumulative_fees: self.engines.values().map(|e| e.state.orders.fees).sum(),
                cumulative_turnover: self.engines.values().map(|e| e.state.orders.turnover).sum(),
                ts_ns: now,
                price: Decimal::ONE,
                equity,
                exposure,
                position: Decimal::ZERO,
                utilization: if equity > Decimal::ZERO {
                    number((exposure + pending) / equity)
                } else {
                    0.0
                },
                regime: MarketRegime::Disabled,
            };
            if self
                .portfolio_performance
                .equity
                .last()
                .is_some_and(|p| p.ts_ns == now)
            {
                self.portfolio_performance.equity.pop();
            }
            self.portfolio_performance.equity.push(point);
        }
        // Shrinking budgets cancels entries only; cancellation uncertainty keeps every reservation
        let pending: Decimal = views.iter().map(|v| v.pending).sum();
        let global_limit = self
            .config
            .portfolio
            .max_total_exposure
            .min(self.config.portfolio.max_total_grid_exposure)
            .min(self.config.portfolio.max_total_equity_exposure);
        let cancel_all = self.portfolio_blocked()
            || exposure + pending > risk_equity * global_limit
            || cash - pending < risk_equity * self.config.portfolio.min_cash_reserve
            || account.is_some_and(|(free, _)| pending > free);
        let concentration_breaches = self.portfolio_risk.concentration_breaches(
            &self.config.portfolio,
            &views,
            risk_equity,
            now,
        );
        for view in views {
            let allocation = self
                .portfolio_risk
                .allocations
                .get(&view.id)
                .copied()
                .unwrap_or(view.base_allocation)
                .min(view.max_position_pct)
                .min(self.config.portfolio.max_instrument_allocation);
            if cancel_all
                || !view.enabled
                || concentration_breaches.contains(&view.id)
                || view.exposure + view.pending > risk_equity * allocation
            {
                self.with_engine(view.id, |e, runtime| {
                    e.cancel(runtime, true, "PORTFOLIO_CAPACITY")
                })?;
            }
        }
        if record
            || previous_peak != self.portfolio_risk.peak_equity
            || previous_day != self.portfolio_risk.day
            || previous_reason != self.portfolio_risk.risk_off_reason
            || previous_allocations != self.portfolio_risk.allocations
        {
            self.persist(None, None)?;
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        id: InstrumentId,
        record: bool,
        f: impl FnOnce(&mut GridStrategyEngine, &mut Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if !self.engines.contains_key(&id) {
            return Ok(());
        }
        let result = self.with_engine(id, |engine, runtime| {
            if let Err(e) = f(engine, runtime) {
                engine.halt(runtime, e);
            }
            Ok(())
        });
        if result.is_ok()
            && let Err(e) = self.observe_portfolio(record)
        {
            self.halt_all(&e.to_string());
            return Err(e);
        }
        result
    }

    fn order_control(&mut self, id: InstrumentId, order: &str, phase: OrderPhase) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if !self.engines.contains_key(&id) {
            self.halt_all(&format!("Unowned order instrument {id}"));
            return;
        }
        let result = self.dispatch(id, false, |e, runtime| {
            anyhow::ensure!(
                e.state.orders.orders().contains_key(order),
                "Unowned order event"
            );
            e.control(runtime, order, phase);
            Ok(())
        });
        if let Err(e) = result {
            self.portfolio_risk.trip(e.to_string());
        }
    }

    fn finish_reports(&mut self) {
        let mut instruments = BTreeMap::new();
        for (id, engine) in &self.engines {
            let orders = &engine.state.orders;
            engine.report.borrow_mut().finish(
                engine.config.grid.capital,
                orders,
                &engine.state.risk,
            );
            instruments.insert(*id, engine.report.borrow().clone());
        }
        let ledgers: Vec<_> = self
            .engines
            .values()
            .map(|e| (&e.state.orders, &e.state.risk))
            .collect();
        self.portfolio_performance.finish_portfolio(
            self.config.portfolio.capital,
            &ledgers,
            self.portfolio_risk.risk_off_reason.clone(),
        );
        let views = self.exposures(None);
        let now = self.clock().timestamp_ns().as_u64();
        let correlations =
            self.portfolio_risk
                .correlation_matrix(&self.config.portfolio, &views, now);
        *self.report.borrow_mut() = PortfolioReport {
            portfolio: self.portfolio_performance.clone(),
            instruments,
            risk: self.portfolio_risk.clone(),
            correlations,
        };
    }

    /// Reconciles every instrument after native post-stop event draining, without resubmission.
    ///
    /// # Errors
    ///
    /// Returns an error before stop or if any recovered inventory/order history is inconsistent.
    pub fn finalize_after_stop(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.stopped, "Finalization requires a stopped strategy");
        let now = self.clock().timestamp_ns().as_u64();
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, |e, runtime| {
                let result = e.recover(runtime);
                e.state.state = StrategyState::Stopped;
                result?;
                e.record_equity(now);
                Ok(())
            })?;
        }
        self.observe_portfolio(true)?;
        self.finish_reports();
        self.persist(None, None)
    }

    /// Explicitly resets both risk layers only after all orders, inventory and cash reconcile.
    /// Runners never call this operator interface automatically.
    ///
    /// # Errors
    ///
    /// Returns an error for unresolved orders, stale marks or any remaining instrument/portfolio limit.
    pub fn reset_risk(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.stopped
                && self
                    .engines
                    .values()
                    .all(|e| e.state.orders.active_ids().is_empty()),
            "Risk reset requires a running portfolio with terminal orders"
        );
        self.recovering = true;
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, GridStrategyEngine::reset_risk)?;
        }
        let now = self.clock().timestamp_ns().as_u64();
        let views = self.exposures(None);
        let cash =
            self.config.portfolio.capital + views.iter().map(|v| v.cash_delta).sum::<Decimal>();
        let exposure = views.iter().map(|v| v.exposure).sum::<Decimal>();
        let (free, broker_equity) = self.account_capacity(
            self.engines
                .values()
                .next()
                .ok_or_else(|| anyhow::anyhow!("Empty instrument universe"))?,
        )?;
        let equity = (cash + exposure).min(broker_equity);
        let c = &self.config.portfolio;
        anyhow::ensure!(
            views
                .iter()
                .all(|v| now.saturating_sub(v.mark_ns) / 1_000_000_000 <= v.max_age_secs)
                && equity > Decimal::ZERO
                && cash.min(free) >= equity * c.min_cash_reserve
                && exposure
                    <= equity
                        * c.max_total_exposure
                            .min(c.max_total_grid_exposure)
                            .min(c.max_total_equity_exposure)
                && self
                    .portfolio_risk
                    .concentration_breaches(c, &views, equity, now)
                    .is_empty(),
            "Portfolio risk reset requires fresh marks and compliant exposures"
        );
        self.portfolio_risk.peak_equity = equity;
        self.portfolio_risk.last_equity = equity;
        self.portfolio_risk.day_start_equity = equity;
        self.portfolio_risk.day = Some(now / 86_400_000_000_000);
        self.portfolio_risk.risk_off_reason = None;
        self.portfolio_risk.last_reallocation_ns.clear();
        self.recovering = false;
        if let Err(e) = self.persist(None, None) {
            self.halt_portfolio(e.to_string());
            return Err(e);
        }
        Ok(())
    }
}

impl DataActor for MultiAssetGridStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.recovering = true;
        // The native submission limiter is process-local. Wait out its complete window on restart
        // rather than granting a second acquisition budget; covered reductions remain available.
        let now = self.clock().timestamp_ns().as_u64();
        self.entries_resume_ns = now.saturating_add(nautilus_core::datetime::NANOSECONDS_IN_MINUTE);
        let sid = self.config.base.strategy_id.expect("Validated identity");
        for order in self.cache().orders(None, None, None, None, None) {
            anyhow::ensure!(
                order.is_closed()
                    || (order.strategy_id() == sid
                        && self.engines.contains_key(&order.instrument_id())),
                "Unowned account order blocks portfolio recovery"
            );
        }
        for position in self.cache().positions_open(None, None, None, None, None) {
            anyhow::ensure!(
                position.strategy_id == sid
                    && position.side == PositionSide::Long
                    && self.engines.contains_key(&position.instrument_id),
                "Unowned account position blocks portfolio recovery"
            );
        }
        let ids: Vec<_> = self.engines.keys().copied().collect();
        let mut currency = None;
        for id in &ids {
            let instrument = self.cache().try_instrument(id)?;
            anyhow::ensure!(
                currency.is_none_or(|c| c == instrument.quote_currency()),
                "Portfolio requires a single quote currency"
            );
            currency = Some(instrument.quote_currency());
            self.with_engine(*id, GridStrategyEngine::on_start)?;
        }
        if self
            .config
            .instruments
            .values()
            .any(|c| c.confirmed_custom_bars)
        {
            self.subscribe_data(
                DataType::new(BarWithVwap::type_name_static(), None, None),
                None,
                None,
            );
        }
        self.recovering = self
            .engines
            .values()
            .any(|e| e.state.state == StrategyState::Recovering);
        if self.recovering {
            self.portfolio_risk.trip("Instrument recovery incomplete");
        }
        self.loaded = None;
        let interval = self
            .engines
            .values()
            .map(|e| {
                e.config
                    .grid
                    .order_timeout_secs
                    .min(e.config.grid.max_signal_age_secs)
            })
            .min()
            .expect("Nonempty portfolio")
            .max(1)
            * 1_000_000_000;
        self.clock()
            .set_timer_ns("GRID_WATCHDOG", interval, None, None, None, None, None)?;
        self.observe_portfolio(true)
    }
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        self.portfolio_performance.diagnostics.bar_events = self
            .portfolio_performance
            .diagnostics
            .bar_events
            .saturating_add(1);
        self.dispatch(bar.bar_type.instrument_id(), true, |e, r| e.on_bar(r, bar))
    }
    fn on_data(&mut self, data: &CustomData) -> anyhow::Result<()> {
        if let Some(value) = data.data.as_any().downcast_ref::<BarWithVwap>() {
            self.dispatch(value.bar.bar_type.instrument_id(), true, |e, r| {
                e.on_data(r, data)
            })?;
        }
        Ok(())
    }
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        // Invalid quotes cannot even advance portfolio time or marks
        if quote.bid_price.as_decimal() <= Decimal::ZERO
            || quote.ask_price < quote.bid_price
            || quote.bid_size.is_zero()
            || quote.ask_size.is_zero()
        {
            return Ok(());
        }
        self.portfolio_performance.diagnostics.quote_events = self
            .portfolio_performance
            .diagnostics
            .quote_events
            .saturating_add(1);
        self.dispatch(quote.instrument_id, false, |e, r| e.on_quote(r, quote))
    }
    fn on_trade(&mut self, trade: &TradeTick) -> anyhow::Result<()> {
        self.dispatch(trade.instrument_id, false, |e, r| e.on_trade(r, trade))
    }
    fn on_time_event(&mut self, event: &TimeEvent) -> anyhow::Result<()> {
        self.portfolio_performance.diagnostics.timer_events = self
            .portfolio_performance
            .diagnostics
            .timer_events
            .saturating_add(1);
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, |engine, runtime| {
                if let Err(e) = engine.on_time_event(runtime, event) {
                    engine.halt(runtime, e);
                }
                Ok(())
            })?;
        }
        let now = self.clock().timestamp_ns().as_u64();
        let interval = self
            .config
            .instruments
            .values()
            .map(|c| c.grid.order_timeout_secs)
            .min()
            .expect("Nonempty portfolio");
        let venue = self
            .config
            .instruments
            .keys()
            .next()
            .expect("Nonempty portfolio")
            .venue;
        if self.config.state_path.is_some()
            && now.saturating_sub(self.last_account_query) / 1_000_000_000 >= interval
            && let Some(account_id) = self.cache().account_id(&venue)
        {
            self.query_account(account_id, None, None)?;
            self.last_account_query = now;
        }
        self.observe_portfolio(false)
    }
    fn on_socket_state(&mut self, event: &SocketStateChanged) -> anyhow::Result<()> {
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.dispatch(id, false, |e, r| e.on_socket_state(r, event))?;
        }
        Ok(())
    }
    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.stopped = true;
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, GridStrategyEngine::on_stop)?;
        }
        self.observe_portfolio(true)?;
        self.finish_reports();
        self.persist(None, None)
    }
    fn on_save(&self) -> anyhow::Result<IndexMap<String, Vec<u8>>> {
        Ok(IndexMap::from([(
            "dynamic_grid".to_string(),
            serde_json::to_vec(&self.checkpoint(None, None))?,
        )]))
    }
    fn on_load(&mut self, state: IndexMap<String, Vec<u8>>) -> anyhow::Result<()> {
        if let Some(bytes) = state.get("dynamic_grid") {
            let saved: Checkpoint = serde_json::from_slice(bytes)?;
            saved.validate(&self.config)?;
            for (id, entry) in &saved.instruments {
                let engine = self.engines.get_mut(id).expect("Validated instrument map");
                engine.state = entry.state.clone();
                *engine.report.borrow_mut() = entry.performance.clone();
            }
            self.portfolio_risk = saved.portfolio_risk.clone();
            self.portfolio_performance = saved.portfolio_performance.clone();
            self.loaded = Some(saved);
            self.recovering = true;
        }
        Ok(())
    }
}

nautilus_strategy!(MultiAssetGridStrategy, {
    fn on_order_submitted(&mut self, event: OrderSubmitted) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Submitted,
        );
    }
    fn on_order_accepted(&mut self, event: OrderAccepted) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Accepted,
        );
    }
    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Cancelled,
        );
    }
    fn on_order_expired(&mut self, event: OrderExpired) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Expired,
        );
    }
    fn on_order_rejected(&mut self, event: OrderRejected) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if let Err(e) = self.dispatch(event.instrument_id, false, |e, r| {
            e.rejected(
                r,
                event.client_order_id.as_str(),
                format!("Order rejected: {}", event.reason),
            );
            Ok(())
        }) {
            self.portfolio_risk.trip(e.to_string());
        }
    }
    fn on_order_denied(&mut self, event: OrderDenied) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if let Err(e) = self.dispatch(event.instrument_id, false, |e, r| {
            e.rejected(
                r,
                event.client_order_id.as_str(),
                format!("Order denied: {}", event.reason),
            );
            Ok(())
        }) {
            self.portfolio_risk.trip(e.to_string());
        }
    }
    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if let Some(engine) = self.engines.get(&event.instrument_id) {
            engine
                .report
                .borrow_mut()
                .diagnostics
                .cancel_rejections
                .entry(event.client_order_id.to_string())
                .or_insert_with(|| super::diagnostics::RejectionObservation {
                    ts_ns: self.clock().timestamp_ns().as_u64(),
                    reason: event.reason.to_string(),
                });
        }
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Unknown,
        );
        self.portfolio_risk
            .trip(format!("Cancel failed: {}", event.reason));
        if let Err(e) = self.observe_portfolio(false) {
            log::error!("Portfolio cancellation failed: {e}");
        }
    }
    fn on_order_filled(&mut self, event: &OrderFilled) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if !self.engines.contains_key(&event.instrument_id) {
            self.halt_all(&format!("Unowned fill instrument {}", event.instrument_id));
            return;
        }
        let result = self.dispatch(event.instrument_id, false, |e, r| {
            e.apply_fill(event)?;
            r.persist_engine(e, None)?;
            if matches!(
                e.state.state,
                StrategyState::GridActive
                    | StrategyState::WaitingForRange
                    | StrategyState::BreakoutPending
                    | StrategyState::Paused
                    | StrategyState::RiskOff
            ) && !r.portfolio_flatten()
                && !(e.state.risk.risk_off_reason.is_some()
                    && e.config.grid.risk_policy == super::config::RiskPolicy::Flatten)
            {
                let now = r.clock().timestamp_ns().as_u64();
                e.exits(r, now, false)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            self.portfolio_risk.trip(e.to_string());
        }
    }
    fn on_order_fill_voided(&mut self, _event: &OrderFillVoided) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        self.recovering = true;
        self.portfolio_risk
            .trip("Broker voided a fill; portfolio inventory requires audit");
        if let Err(e) = self.observe_portfolio(false) {
            log::error!("Portfolio cancellation failed: {e}");
        }
    }
});

impl Checkpoint {
    fn validate(&self, expected: &MultiAssetGridConfig) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 5
                && serde_json::to_value(&self.config)? == serde_json::to_value(expected)?,
            "Checkpoint version/configuration mismatch; audit before migration"
        );
        anyhow::ensure!(
            self.instruments.keys().eq(expected.instruments.keys()),
            "Recovered instrument set mismatch"
        );
        self.portfolio_risk.validate(
            &expected.portfolio,
            &expected.instruments.keys().copied().collect(),
        )?;
        let mut ids = BTreeSet::new();
        for (id, saved) in &self.instruments {
            saved.state.orders.validate()?;
            saved
                .state
                .regime
                .validate(&expected.instruments[id].grid)?;
            anyhow::ensure!(
                saved.state.completed_cycles <= saved.state.orders.cycles.len(),
                "Invalid cycle cursor"
            );
            for order in saved.state.orders.orders().keys() {
                anyhow::ensure!(
                    ids.insert(order),
                    "Duplicate cross-instrument order identity"
                );
                let prefix = format!(
                    "DG-{}-{id}-",
                    expected
                        .base
                        .order_id_tag
                        .as_deref()
                        .expect("Validated tag")
                );
                anyhow::ensure!(
                    order.starts_with(&prefix),
                    "Order namespace differs from instrument owner"
                );
            }
            if let Some(grid) = &saved.state.grid {
                let config = &expected.instruments[id].grid;
                let count = config.grid_levels * 2;
                anyhow::ensure!(
                    grid.grid_id <= saved.state.generation
                        && grid.center > Decimal::ZERO
                        && grid.spacing > Decimal::ZERO
                        && grid.lower_bound > Decimal::ZERO
                        && grid.upper_bound > grid.lower_bound
                        && grid.lower_bound <= grid.center
                        && grid.center <= grid.upper_bound
                        && grid.levels.len() == count,
                    "Invalid recovered grid"
                );
                let mut indices = BTreeSet::new();
                for level in &grid.levels {
                    let identity_valid = level.level_index != 0
                        && level.level_index.unsigned_abs() as usize <= config.grid_levels;
                    anyhow::ensure!(
                        identity_valid
                            && indices.insert(level.level_index)
                            && level.side == OrderSide::Buy
                            && level.quantity >= Decimal::ZERO
                            && level.price >= grid.lower_bound
                            && level.exit_price > level.price
                            && level.exit_price <= grid.upper_bound,
                        "Invalid recovered grid level for {id}"
                    );
                }
            }
        }
        for order in &self.orders {
            anyhow::ensure!(
                Some(order.strategy_id()) == expected.base.strategy_id
                    && self
                        .instruments
                        .get(&order.instrument_id())
                        .is_some_and(|e| e
                            .state
                            .orders
                            .orders()
                            .contains_key(order.client_order_id().as_str())),
                "Native order snapshot ownership mismatch"
            );
        }
        for position in &self.positions {
            anyhow::ensure!(
                Some(position.strategy_id) == expected.base.strategy_id
                    && self.instruments.contains_key(&position.instrument_id)
                    && position.side == PositionSide::Long,
                "Native position snapshot ownership mismatch"
            );
        }
        for (id, weight) in &self.portfolio_risk.allocations {
            anyhow::ensure!(
                expected
                    .instruments
                    .get(id)
                    .is_some_and(|c| *weight >= Decimal::ZERO && *weight <= c.capital_allocation),
                "Invalid recovered allocation"
            );
        }
        anyhow::ensure!(
            self.portfolio_risk.peak_equity > Decimal::ZERO,
            "Invalid portfolio high-water mark"
        );
        Ok(())
    }
}
