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

//! Native multi-instrument replay and a genuinely shared-account equal-weight benchmark.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use nautilus_common::{
    actor::{DataActor, registry::try_get_actor_unchecked},
    logging::config::LoggerConfig,
    msgbus::{self, TypedHandler},
    throttler::RateLimit,
};
use nautilus_execution::models::fill::{FillModelHandle, OneTickSlippageFillModel};
use nautilus_model::{
    data::{Bar, Data, QuoteTick},
    enums::{AccountType, BookType, OmsType, OrderSide, TimeInForce},
    events::{OrderDenied, OrderFilled, OrderRejected},
    identifiers::{InstrumentId, StrategyId, Symbol},
    instruments::{Equity, InstrumentAny},
    types::{Money, Quantity},
};
use nautilus_risk::engine::config::RiskEngineConfig;
pub use nautilus_trading::examples::strategies::dynamic_grid::files::{
    GridInstrumentFile as PortfolioBacktestInstrument,
    GridPortfolioFile as PortfolioBacktestConfig, load_portfolio_config,
};
use nautilus_trading::{
    examples::strategies::dynamic_grid::{
        MultiAssetGridStrategy, PortfolioReport,
        analytics::{EquityPoint, PerformanceTracker},
        config::StrategyMode,
        engine::{GridLevel, LevelStatus},
        orders::{OrderManager, OrderPhase},
        portfolio::PortfolioRiskManager,
        regime::MarketRegime,
        risk::RiskManager,
    },
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};

use super::{GridBacktestConfig, load_bars, load_quotes, set_strategy_mode_preserving_atr_spacing};
use crate::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};

/// Loads each CSV with its own metadata, then stably merges events by time and instrument.
///
/// # Errors
///
/// Returns an error on any invalid/missing instrument stream; data is never copied across symbols.
pub fn load_portfolio_data(
    config: &PortfolioBacktestConfig,
) -> anyhow::Result<(Vec<Bar>, Vec<QuoteTick>)> {
    config.strategy_config()?;
    let mut bars = Vec::new();
    let mut quotes = Vec::new();
    for (id, c) in &config.instruments {
        let single = GridBacktestConfig {
            instrument_id: *id,
            currency: config.currency,
            price_increment: c.price_increment,
            lot_size: c.lot_size,
            grid: c.strategy.grid.clone(),
            random_seed: config.random_seed,
            slippage_probability: config.slippage_probability,
            start_ns: config.start_ns,
            end_ns: config.end_ns,
        };
        bars.extend(load_bars(&c.bars_path, &single)?);
        if let Some(path) = &c.quotes_path {
            quotes.extend(load_quotes(path, &single)?);
        }
    }
    bars.sort_by_key(|b| (b.ts_event, b.bar_type.instrument_id()));
    quotes.sort_by_key(|q| (q.ts_event, q.instrument_id));
    Ok((bars, quotes))
}

/// Portfolio comparison mode, always using a single native account and strategy instance.
#[derive(Clone, Copy, Debug)]
pub enum PortfolioBenchmark {
    /// Uses the source configuration without changing strategy mode.
    Dynamic,
    /// Original paper-style dynamic reset and inventory model.
    LegacyDgt,
    /// Stock-adapted dynamic reset with target-position sleeves and stock gates.
    Sadg,
    /// Original geometric grid with a fixed anchor.
    Fixed,
    /// Equal initial allocations, buy once at each instrument's first available completed bar.
    EqualWeightBuyHold,
}

/// Resolves controlled comparisons without changing capital, costs, filters or execution metadata.
///
/// # Errors
///
/// Returns an error if either the source or effective configuration is invalid.
pub fn portfolio_benchmark_config(
    config: &PortfolioBacktestConfig,
    benchmark: PortfolioBenchmark,
) -> anyhow::Result<PortfolioBacktestConfig> {
    config.strategy_config()?.validate()?;
    let mut effective = config.clone();
    for asset in effective.instruments.values_mut() {
        match benchmark {
            PortfolioBenchmark::LegacyDgt => {
                set_strategy_mode_preserving_atr_spacing(
                    &mut asset.strategy.grid,
                    StrategyMode::LegacyDgt,
                )?;
                asset.strategy.grid.enable_dynamic_reset = true;
            }
            PortfolioBenchmark::Sadg => {
                set_strategy_mode_preserving_atr_spacing(
                    &mut asset.strategy.grid,
                    StrategyMode::StockAdaptive,
                )?;
                asset.strategy.grid.enable_dynamic_reset = true;
            }
            PortfolioBenchmark::Fixed => {
                asset.strategy.grid.strategy_mode = StrategyMode::LegacyDgt;
                set_strategy_mode_preserving_atr_spacing(
                    &mut asset.strategy.grid,
                    StrategyMode::LegacyDgt,
                )?;
                asset.strategy.grid.enable_dynamic_reset = false;
            }
            PortfolioBenchmark::Dynamic | PortfolioBenchmark::EqualWeightBuyHold => {}
        }
    }
    effective.strategy_config()?.validate()?;
    Ok(effective)
}

/// Replays an asynchronous multi-instrument history through Nautilus, not separate backtests.
///
/// # Errors
///
/// Returns an error for unknown instruments, mixed tick/bar execution, inconsistent clocks or engine failures.
pub fn run_portfolio_backtest(
    bars: &[Bar],
    quotes: &[QuoteTick],
    config: &PortfolioBacktestConfig,
    benchmark: PortfolioBenchmark,
) -> anyhow::Result<PortfolioReport> {
    run_portfolio_backtest_with_progress(bars, quotes, config, benchmark, |_, _, _| {})
}

/// Replays the portfolio while reporting received events, total input events and replay timestamp.
///
/// The callback observes market data only; it does not change execution or advance the clock.
/// It runs once per published input event, so callers should throttle expensive display updates.
/// Completion of the final event does not include strategy finalization or report writing.
///
/// # Errors
///
/// Returns the same validation and execution errors as [`run_portfolio_backtest`].
pub fn run_portfolio_backtest_with_progress(
    bars: &[Bar],
    quotes: &[QuoteTick],
    config: &PortfolioBacktestConfig,
    benchmark: PortfolioBenchmark,
    on_progress: impl FnMut(usize, usize, u64) + 'static,
) -> anyhow::Result<PortfolioReport> {
    let effective = portfolio_benchmark_config(config, benchmark)?;
    let config = &effective;
    let mut strategy_config = config.strategy_config()?;
    anyhow::ensure!(
        !bars.is_empty(),
        "Portfolio backtest requires completed bars"
    );
    let ids: Vec<InstrumentId> = config.instruments.keys().copied().collect();
    let mut previous = BTreeMap::new();
    for bar in bars {
        let id = bar.bar_type.instrument_id();
        let c = config
            .instruments
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("Unknown bar instrument {id}"))?;
        anyhow::ensure!(
            c.strategy.bar_type == bar.bar_type
                && previous
                    .insert(id, bar.ts_event)
                    .is_none_or(|p| p < bar.ts_event),
            "Invalid per-instrument signal stream"
        );
    }
    anyhow::ensure!(
        ids.iter().all(|id| previous.contains_key(id)),
        "Every configured instrument needs its own bars"
    );
    for quote in quotes {
        anyhow::ensure!(
            config.instruments.contains_key(&quote.instrument_id),
            "Unknown quote instrument"
        );
        anyhow::ensure!(
            quote.bid_price.as_decimal() > Decimal::ZERO
                && quote.ask_price >= quote.bid_price
                && !quote.bid_size.is_zero()
                && !quote.ask_size.is_zero()
                && quote.ts_event <= quote.ts_init,
            "Invalid portfolio quote"
        );
    }
    if !quotes.is_empty() {
        anyhow::ensure!(
            ids.iter()
                .all(|id| quotes.iter().any(|q| q.instrument_id == *id)),
            "Tick mode requires quotes for every instrument"
        );
    }
    for instrument in strategy_config.instruments.values_mut() {
        instrument.tick_execution = !quotes.is_empty();
    }
    let venue = ids[0].venue;
    let mut engine = BacktestEngine::new(
        BacktestEngineConfig::builder()
            .risk_engine(RiskEngineConfig {
                max_order_submit: RateLimit::new_checked(
                    config.portfolio.max_orders_per_minute,
                    60_000_000_000,
                )?,
                max_notional_per_order: ids
                    .iter()
                    .map(|id| (*id, config.portfolio.max_order_value))
                    .collect(),
                ..Default::default()
            })
            .logging(LoggerConfig {
                bypass_logging: true,
                ..Default::default()
            })
            .build(),
    )?;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Cash)
            .book_type(BookType::L1_MBP)
            .base_currency(config.currency)
            .starting_balances(vec![Money::from_decimal(
                config.portfolio.capital,
                config.currency,
            )?])
            .fill_model(FillModelHandle::new(OneTickSlippageFillModel::new(
                1.0,
                config.slippage_probability,
                Some(config.random_seed),
            )?))
            .bar_execution(quotes.is_empty())
            .trade_execution(true)
            .liquidity_consumption(!quotes.is_empty())
            .use_reduce_only(false)
            .build()?,
    )?;
    for (id, c) in &config.instruments {
        let instrument = InstrumentAny::Equity(
            Equity::builder()
                .instrument_id(*id)
                .raw_symbol(Symbol::from(id.symbol.as_str()))
                .currency(config.currency)
                .price_precision(c.price_increment.precision)
                .price_increment(c.price_increment)
                .lot_size(c.lot_size)
                .min_quantity(c.lot_size)
                .maker_fee(c.strategy.grid.maker_fee + c.strategy.grid.commission)
                .taker_fee(c.strategy.grid.taker_fee + c.strategy.grid.commission)
                .ts_event(0.into())
                .ts_init(0.into())
                .build()?,
        );
        engine.add_instrument(&instrument)?;
    }
    let (report, grid_id) = match benchmark {
        PortfolioBenchmark::Dynamic
        | PortfolioBenchmark::LegacyDgt
        | PortfolioBenchmark::Sadg
        | PortfolioBenchmark::Fixed => {
            let id = strategy_config.base.strategy_id;
            let strategy = MultiAssetGridStrategy::new(strategy_config)?;
            let report = strategy.portfolio_report_handle();
            engine.add_strategy(strategy)?;
            (report, id)
        }
        PortfolioBenchmark::EqualWeightBuyHold => {
            let strategy = PortfolioBuyHold::new(config.clone())?;
            let report = Rc::clone(&strategy.report);
            engine.add_strategy(strategy)?;
            (report, None)
        }
    };
    let mut data: Vec<Data> = bars.iter().copied().map(Data::Bar).collect();
    data.extend(quotes.iter().copied().map(Data::Quote));
    // Stable tie ordering is explicit: the same-time bar completes before a quote consumes its signal
    data.sort_by_key(|d| match d {
        Data::Bar(b) => (b.ts_event, 0, b.bar_type.instrument_id()),
        Data::Quote(q) => (q.ts_event, 1, q.instrument_id),
        _ => unreachable!("Only bars and quotes"),
    });
    let total = data.len();
    engine.add_data(data, None, true, true)?;
    let progress = RefCell::new((0_usize, on_progress));
    let on_event = Rc::new(move |ts_ns| {
        let mut progress = progress.borrow_mut();
        let (received, callback) = &mut *progress;
        *received += 1;
        callback(*received, total, ts_ns);
    });
    let on_bar = Rc::clone(&on_event);
    let bar_handler = TypedHandler::from_with_id("grid-replay-progress-bars", move |bar: &Bar| {
        on_bar(bar.ts_init.as_u64());
    });
    let quote_handler =
        TypedHandler::from_with_id("grid-replay-progress-quotes", move |quote: &QuoteTick| {
            on_event(quote.ts_init.as_u64());
        });
    msgbus::subscribe_bars("data.bars.*".into(), bar_handler.clone(), None);
    if !quotes.is_empty() {
        msgbus::subscribe_quotes("data.quotes.*".into(), quote_handler.clone(), None);
    }
    let result = engine.run(None, None, None, false).and_then(|()| {
        if let Some(id) = grid_id {
            try_get_actor_unchecked::<MultiAssetGridStrategy>(&id.inner())
                .ok_or_else(|| anyhow::anyhow!("Stopped portfolio unavailable"))?
                .finalize_after_stop()?;
        }
        Ok(())
    });
    msgbus::unsubscribe_bars("data.bars.*".into(), &bar_handler);
    msgbus::unsubscribe_quotes("data.quotes.*".into(), &quote_handler);
    let report = report.borrow().clone();
    engine.dispose();
    result?;
    if matches!(benchmark, PortfolioBenchmark::EqualWeightBuyHold) {
        anyhow::ensure!(
            report
                .instruments
                .values()
                .all(|r| r.risk_off_reason.is_none()),
            "Buy-and-hold benchmark order rejected; compare only successful executions"
        );
    }
    anyhow::ensure!(
        !report.portfolio.equity.is_empty() && report.instruments.len() == config.instruments.len(),
        "Portfolio strategy did not produce a complete report"
    );
    Ok(report)
}

#[derive(Debug)]
struct PortfolioBuyHold {
    core: StrategyCore,
    config: PortfolioBacktestConfig,
    orders: BTreeMap<InstrumentId, OrderManager>,
    risk: BTreeMap<InstrumentId, RiskManager>,
    reports: BTreeMap<InstrumentId, PerformanceTracker>,
    marks: BTreeMap<InstrumentId, Decimal>,
    submitted: BTreeSet<InstrumentId>,
    sleeve_capital: Decimal,
    report: Rc<RefCell<PortfolioReport>>,
}

impl PortfolioBuyHold {
    fn new(config: PortfolioBacktestConfig) -> anyhow::Result<Self> {
        let capital = config.portfolio.capital;
        let share = (capital / Decimal::from(config.instruments.len())).round_dp_with_strategy(
            u32::from(config.currency.precision),
            RoundingStrategy::ToZero,
        );
        let orders = config
            .instruments
            .keys()
            .map(|id| (*id, OrderManager::new(share)))
            .collect();
        let risk = config
            .instruments
            .keys()
            .map(|id| (*id, RiskManager::new(share)))
            .collect();
        let reports = config
            .instruments
            .keys()
            .map(|id| (*id, PerformanceTracker::default()))
            .collect();
        Ok(Self {
            core: StrategyCore::new_checked(StrategyConfig {
                strategy_id: Some(StrategyId::from("PORTFOLIO-BUY-HOLD-903")),
                order_id_tag: Some("903".to_string()),
                oms_type: Some(OmsType::Netting),
                ..Default::default()
            })?,
            config,
            orders,
            risk,
            reports,
            marks: BTreeMap::new(),
            submitted: BTreeSet::new(),
            sleeve_capital: share,
            report: Rc::new(RefCell::new(PortfolioReport {
                portfolio: PerformanceTracker::default(),
                instruments: BTreeMap::new(),
                risk: PortfolioRiskManager::new(capital),
                correlations: BTreeMap::new(),
            })),
        })
    }

    fn record(&mut self, now: u64) {
        let share = self.sleeve_capital;
        let mut total_equity =
            self.config.portfolio.capital - share * Decimal::from(self.orders.len());
        let mut total_exposure = Decimal::ZERO;
        let mut total_position = Decimal::ZERO;
        for (id, orders) in &self.orders {
            let price = self.marks.get(id).copied().unwrap_or(Decimal::ZERO);
            let exposure = orders.inventory() * price;
            let equity = orders.cash + exposure;
            total_equity += equity;
            total_exposure += exposure;
            total_position += orders.inventory();
            let report = self.reports.get_mut(id).expect("Configured report");
            if report.equity.last().is_some_and(|p| p.ts_ns == now) {
                report.equity.pop();
            }
            report.equity.push(EquityPoint {
                trend_inventory: None,
                cumulative_fees: orders.fees,
                cumulative_turnover: orders.turnover,
                ts_ns: now,
                price,
                equity,
                exposure,
                position: orders.inventory(),
                utilization: if equity > Decimal::ZERO {
                    (exposure / equity).to_f64().unwrap_or(0.0)
                } else {
                    0.0
                },
                regime: MarketRegime::Disabled,
            });
        }
        let report = &mut self.report.borrow_mut().portfolio;
        report.observe_mark(
            self.config.portfolio.capital,
            total_equity,
            total_exposure,
            total_position,
            now,
        );
        if report.equity.last().is_some_and(|p| p.ts_ns == now) {
            report.equity.pop();
        }
        report.equity.push(EquityPoint {
            trend_inventory: None,
            cumulative_fees: self.orders.values().map(|orders| orders.fees).sum(),
            cumulative_turnover: self.orders.values().map(|orders| orders.turnover).sum(),
            ts_ns: now,
            price: Decimal::ONE,
            equity: total_equity,
            exposure: total_exposure,
            position: total_position,
            utilization: if total_equity > Decimal::ZERO {
                (total_exposure / total_equity).to_f64().unwrap_or(0.0)
            } else {
                0.0
            },
            regime: MarketRegime::Disabled,
        });
    }
}

impl DataActor for PortfolioBuyHold {
    fn on_start(&mut self) -> anyhow::Result<()> {
        let bars: Vec<_> = self
            .config
            .instruments
            .values()
            .map(|c| c.strategy.bar_type)
            .collect();
        for bar in bars {
            self.subscribe_bars(bar, None, None);
        }
        Ok(())
    }
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let id = bar.bar_type.instrument_id();
        let c = &self.config.instruments[&id];
        let price = bar.close.as_decimal();
        self.marks.insert(id, price);
        if !self.submitted.contains(&id) {
            let lot = c.lot_size.as_decimal();
            let cost = price
                * (Decimal::ONE
                    + c.strategy.grid.taker_fee
                    + c.strategy.grid.commission
                    + c.strategy.grid.slippage)
                + c.price_increment.as_decimal();
            let mut remaining = (self.orders[&id].cash / cost / lot).floor() * lot;
            let chunk = (self.config.portfolio.max_order_value / cost / lot).floor() * lot;
            anyhow::ensure!(
                remaining <= Decimal::ZERO || chunk > Decimal::ZERO,
                "Buy-and-hold order cap cannot fund one lot for {id}"
            );
            // Split the original allocation without changing capital, entry time or fee model.
            let precision = c.lot_size.precision;
            while remaining > Decimal::ZERO {
                let quantity = remaining.min(chunk);
                let pair = GridLevel {
                    level_index: 0,
                    price,
                    side: OrderSide::Buy,
                    exit_price: price * Decimal::from(2),
                    quantity,
                    status: LevelStatus::Pending,
                    entry_order_id: None,
                    exit_order_id: None,
                };
                let generation = self.orders[&id].orders().len() as u64 + 1;
                let intent = self.orders.get_mut(&id).expect("Configured ledger").entry(
                    &format!("903-{id}"),
                    generation,
                    &pair,
                    quantity,
                    Some(price),
                    bar.ts_event.as_u64(),
                )?;
                let order = self.order().market(
                    id,
                    OrderSide::Buy,
                    Quantity::from_decimal_dp(quantity, precision)?,
                    Some(TimeInForce::Day),
                    Some(false),
                    None,
                    None,
                    None,
                    None,
                    Some(intent.id.as_str().into()),
                );
                self.submitted.insert(id);
                self.submit_order(order, None, None, None)?;
                remaining -= quantity;
            }
        }
        self.record(bar.ts_event.as_u64());
        Ok(())
    }
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if let Some(mark) = self.marks.get_mut(&quote.instrument_id) {
            *mark =
                (quote.bid_price.as_decimal() + quote.ask_price.as_decimal()) / Decimal::from(2);
            self.record(quote.ts_event.as_u64());
        }
        Ok(())
    }
    fn on_stop(&mut self) -> anyhow::Result<()> {
        let now = self.clock().timestamp_ns().as_u64();
        self.record(now);
        let share = self.sleeve_capital;
        for (id, report) in &mut self.reports {
            report.finish(share, &self.orders[id], &self.risk[id]);
        }
        let ledgers: Vec<_> = self
            .orders
            .iter()
            .map(|(id, o)| (o, &self.risk[id]))
            .collect();
        let instrument_reports: Vec<_> = self.reports.values().collect();
        let mut report = self.report.borrow_mut();
        report.portfolio.finish_portfolio(
            self.config.portfolio.capital,
            &ledgers,
            &instrument_reports,
            None,
        );
        report.instruments = self.reports.clone();
        Ok(())
    }
}

nautilus_strategy!(PortfolioBuyHold, {
    fn on_order_denied(&mut self, event: OrderDenied) {
        if let Some(orders) = self.orders.get_mut(&event.instrument_id) {
            orders.transition(
                event.client_order_id.as_str(),
                OrderPhase::Rejected,
                event.ts_event.as_u64(),
            );
        }
        if let Some(risk) = self.risk.get_mut(&event.instrument_id) {
            risk.trip(format!("Benchmark order denied: {}", event.reason));
        }
    }
    fn on_order_rejected(&mut self, event: OrderRejected) {
        if let Some(orders) = self.orders.get_mut(&event.instrument_id) {
            orders.transition(
                event.client_order_id.as_str(),
                OrderPhase::Rejected,
                event.ts_event.as_u64(),
            );
        }
        if let Some(risk) = self.risk.get_mut(&event.instrument_id) {
            risk.trip(format!("Benchmark order rejected: {}", event.reason));
        }
    }
    fn on_order_filled(&mut self, event: &OrderFilled) {
        let Some(orders) = self.orders.get_mut(&event.instrument_id) else {
            return;
        };
        if let Err(e) = orders.fill(
            event.client_order_id.as_str(),
            event.trade_id.as_str(),
            event.last_qty.as_decimal(),
            event.last_px.as_decimal(),
            event.commission.map_or(Decimal::ZERO, |f| f.as_decimal()),
            false,
            event.ts_event.as_u64(),
        ) {
            log::error!("Portfolio benchmark accounting failed: {e}");
        }
    }
});
