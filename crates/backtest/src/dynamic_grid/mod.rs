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

//! Reproducible dynamic-grid research on the native `BacktestEngine`.

pub mod portfolio;
pub mod portfolio_research;
pub mod research;

use std::{
    cell::RefCell,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    rc::Rc,
};

use nautilus_common::{
    actor::{DataActor, registry::try_get_actor_unchecked},
    logging::config::LoggerConfig,
};
use nautilus_execution::models::fill::{FillModelHandle, OneTickSlippageFillModel};
use nautilus_model::{
    data::{Bar, BarType, Data, QuoteTick},
    enums::{AccountType, BookType, OmsType, OrderSide, TimeInForce},
    events::OrderFilled,
    identifiers::{InstrumentId, StrategyId, Symbol},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::{
    examples::strategies::dynamic_grid::{
        DynamicGridConfig, DynamicGridStrategy,
        analytics::{EquityPoint, PerformanceTracker},
        config::{GridConfig, RiskPolicy, SpacingMode, StrategyMode},
        engine::GridLevel,
        orders::OrderManager,
        regime::MarketRegime,
        risk::RiskManager,
    },
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};

/// Research mode changes the strategy/configuration, not the native execution engine.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Benchmark {
    /// Uses the source configuration without changing strategy mode.
    Dynamic,
    /// Original paper-style dynamic reset and inventory model.
    LegacyDgt,
    /// Stock Adaptive Dynamic Grid.
    Sadg,
    /// Fixed initial grid, liquidated on boundary break.
    Fixed,
    /// Full-capital long-only buy and hold with entry fees and marked terminal inventory.
    BuyHold,
}

pub(super) fn set_strategy_mode_preserving_atr_spacing(
    config: &mut GridConfig,
    mode: StrategyMode,
) -> anyhow::Result<()> {
    if config.spacing_mode == SpacingMode::Atr && config.strategy_mode != mode {
        let levels = Decimal::from(config.grid_levels);
        config.atr_multiplier = match (config.strategy_mode, mode) {
            (StrategyMode::LegacyDgt, StrategyMode::StockAdaptive) => config
                .atr_multiplier
                .checked_mul(levels)
                .ok_or_else(|| anyhow::anyhow!("ATR multiplier overflow"))?,
            (StrategyMode::StockAdaptive, StrategyMode::LegacyDgt) => {
                config.atr_multiplier / levels
            }
            _ => config.atr_multiplier,
        };
    }
    config.strategy_mode = mode;
    Ok(())
}

/// Explicit market metadata and economics for a research run.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridBacktestConfig {
    /// Instrument used to label the input data.
    pub instrument_id: InstrumentId,
    /// Quote currency of both price and capital.
    pub currency: Currency,
    /// Venue tick increment.
    pub price_increment: Price,
    /// Venue quantity/lot increment.
    pub lot_size: Quantity,
    /// Shared production strategy parameters.
    pub grid: GridConfig,
    /// Deterministic fill-model seed.
    pub random_seed: u64,
    /// Probability of a one-tick adverse fill, where the matching engine permits it.
    pub slippage_probability: f64,
    /// Inclusive replay start; portfolio daily-regime warmup uses strictly earlier source bars.
    pub start_ns: Option<u64>,
    /// Exclusive end timestamp.
    pub end_ns: Option<u64>,
}

impl Default for GridBacktestConfig {
    fn default() -> Self {
        Self {
            instrument_id: InstrumentId::from("AAPL.SIM"),
            currency: Currency::USD(),
            price_increment: Price::from("0.01"),
            lot_size: Quantity::from("1"),
            grid: GridConfig::default(),
            random_seed: 42,
            slippage_probability: 1.0,
            start_ns: None,
            end_ns: None,
        }
    }
}

impl GridBacktestConfig {
    /// Signal bar type with close timestamps.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid instrument identifier.
    pub fn bar_type(&self) -> anyhow::Result<BarType> {
        Ok(format!("{}-1-MINUTE-LAST-EXTERNAL", self.instrument_id).parse()?)
    }
}

/// Loads plain/gzip OHLCV CSV or the repository intraday schema with optional VWAP.
///
/// # Errors
///
/// Returns an error on schema/OHLC errors, unsorted timestamps, overflow or absent data.
pub fn load_bars(path: &Path, config: &GridBacktestConfig) -> anyhow::Result<Vec<Bar>> {
    config.grid.validate()?;
    let mut lines = csv_reader(path)?.lines();
    let header = lines.next().ok_or_else(|| anyhow::anyhow!("Empty CSV"))??;
    let extended = matches!(
        header.as_str(),
        "timestamp_ns,session_open,session_close,open,high,low,close,volume,vwap"
            | "timestamp_ns,session_open,session_close,open,high,low,close,volume"
    );
    let columns = header.split(',').count();
    anyhow::ensure!(
        extended || header == "timestamp_ns,open,high,low,close,volume",
        "Unsupported bar CSV schema"
    );
    let kind = config.bar_type()?;
    let mut bars = Vec::new();
    let mut previous = None;
    for line in lines {
        let line = line?;
        let fields: Vec<_> = line.split(',').collect();
        anyhow::ensure!(fields.len() == columns, "Invalid CSV field count");
        let ts: u64 = fields[0].parse()?;
        anyhow::ensure!(
            previous.is_none_or(|p| ts > p),
            "CSV must be strictly increasing with unique close timestamps"
        );
        previous = Some(ts);
        if config.start_ns.is_some_and(|start| ts < start)
            || config.end_ns.is_some_and(|end| ts >= end)
        {
            continue;
        }
        let offset = if extended { 3 } else { 1 };
        let values = fields[offset..offset + 4]
            .iter()
            .map(|s| s.parse::<Decimal>())
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            values.iter().all(|p| *p > Decimal::ZERO)
                && values[2] <= values[0]
                && values[2] <= values[3]
                && values[1] >= values[0]
                && values[1] >= values[3],
            "Invalid OHLC"
        );
        let prices = values
            .iter()
            .map(|p| Price::from_decimal_dp(*p, config.price_increment.precision))
            .collect::<Result<Vec<_>, _>>()?;
        let volume: Decimal = fields[offset + 4].parse()?;
        anyhow::ensure!(volume >= Decimal::ZERO, "Negative volume");
        bars.push(Bar::new(
            kind,
            prices[0],
            prices[1],
            prices[2],
            prices[3],
            Quantity::from_decimal_dp(volume.floor(), 0)?,
            ts.into(),
            ts.into(),
        ));
    }
    anyhow::ensure!(!bars.is_empty(), "No bars in requested range");
    Ok(bars)
}

/// Loads real quote replay data; never synthesizes intrabar ticks from OHLC.
///
/// # Errors
///
/// Returns an error on invalid columns, prices, sizes or timestamp order.
pub fn load_quotes(path: &Path, config: &GridBacktestConfig) -> anyhow::Result<Vec<QuoteTick>> {
    let mut lines = csv_reader(path)?.lines();
    anyhow::ensure!(
        lines.next().transpose()?.as_deref() == Some("timestamp_ns,bid,ask,bid_size,ask_size"),
        "Unsupported quote CSV schema"
    );
    let mut quotes = Vec::new();
    let mut previous = None;
    for line in lines {
        let line = line?;
        let f: Vec<_> = line.split(',').collect();
        anyhow::ensure!(f.len() == 5, "Invalid quote row");
        let ts: u64 = f[0].parse()?;
        anyhow::ensure!(
            previous.is_none_or(|p| ts >= p),
            "Quotes must be time ordered"
        );
        previous = Some(ts);
        if config.start_ns.is_some_and(|start| ts < start)
            || config.end_ns.is_some_and(|end| ts >= end)
        {
            continue;
        }
        let bid = Price::from_decimal_dp(f[1].parse()?, config.price_increment.precision)?;
        let ask = Price::from_decimal_dp(f[2].parse()?, config.price_increment.precision)?;
        anyhow::ensure!(
            bid.as_decimal() > Decimal::ZERO && ask >= bid,
            "Invalid quote prices"
        );
        quotes.push(QuoteTick::new(
            config.instrument_id,
            bid,
            ask,
            f[3].parse().map_err(|e: String| anyhow::anyhow!(e))?,
            f[4].parse().map_err(|e: String| anyhow::anyhow!(e))?,
            ts.into(),
            ts.into(),
        ));
    }
    anyhow::ensure!(!quotes.is_empty(), "No quotes in requested range");
    Ok(quotes)
}

fn csv_reader(path: &Path) -> anyhow::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path.extension().is_some_and(|ext| ext == "gz") {
        Ok(Box::new(BufReader::new(flate2::read::MultiGzDecoder::new(
            file,
        ))))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Runs a benchmark through native matching, fees, orders, positions and accounting.
///
/// # Errors
///
/// Returns an error if inputs are invalid, the strategy fails to initialize or the engine fails.
pub fn run_grid_backtest(
    bars: &[Bar],
    quotes: &[QuoteTick],
    config: &GridBacktestConfig,
    benchmark: Benchmark,
) -> anyhow::Result<PerformanceTracker> {
    config.grid.validate()?;
    anyhow::ensure!(!bars.is_empty(), "Backtest requires completed signal bars");
    anyhow::ensure!(
        bars.windows(2).all(|p| p[0].ts_event < p[1].ts_event),
        "Bars must be strictly ordered"
    );
    let kind = config.bar_type()?;
    anyhow::ensure!(bars.iter().all(|b| b.bar_type == kind), "Bar type mismatch");
    let instrument = InstrumentAny::Equity(
        Equity::builder()
            .instrument_id(config.instrument_id)
            .raw_symbol(Symbol::from(config.instrument_id.symbol.as_str()))
            .currency(config.currency)
            .price_precision(config.price_increment.precision)
            .price_increment(config.price_increment)
            .lot_size(config.lot_size)
            .min_quantity(config.lot_size)
            .maker_fee(config.grid.maker_fee + config.grid.commission)
            .taker_fee(config.grid.taker_fee + config.grid.commission)
            .ts_event(0.into())
            .ts_init(0.into())
            .build()?,
    );
    let mut engine = BacktestEngine::new(
        BacktestEngineConfig::builder()
            .logging(LoggerConfig {
                bypass_logging: true,
                ..Default::default()
            })
            .build(),
    )?;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(config.instrument_id.venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Cash)
            .book_type(BookType::L1_MBP)
            .base_currency(config.currency)
            .starting_balances(vec![Money::from_decimal(
                config.grid.capital,
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
    engine.add_instrument(&instrument)?;
    let (report, grid_id) = match benchmark {
        Benchmark::Dynamic | Benchmark::LegacyDgt | Benchmark::Sadg | Benchmark::Fixed => {
            let mut strategy_config =
                DynamicGridConfig::new(config.instrument_id, config.bar_type()?);
            strategy_config.grid = config.grid.clone();
            strategy_config.tick_execution = !quotes.is_empty();
            match benchmark {
                Benchmark::LegacyDgt => {
                    set_strategy_mode_preserving_atr_spacing(
                        &mut strategy_config.grid,
                        StrategyMode::LegacyDgt,
                    )?;
                    strategy_config.grid.enable_dynamic_reset = true;
                }
                Benchmark::Sadg => {
                    set_strategy_mode_preserving_atr_spacing(
                        &mut strategy_config.grid,
                        StrategyMode::StockAdaptive,
                    )?;
                    strategy_config.grid.enable_dynamic_reset = true;
                }
                Benchmark::Fixed => {
                    strategy_config.grid.strategy_mode = StrategyMode::LegacyDgt;
                    set_strategy_mode_preserving_atr_spacing(
                        &mut strategy_config.grid,
                        StrategyMode::LegacyDgt,
                    )?;
                    strategy_config.grid.enable_dynamic_reset = false;
                    strategy_config.grid.spacing_mode = SpacingMode::Percentage;
                    strategy_config.grid.enable_trend_filter = false;
                    strategy_config.grid.enable_volatility_filter = false;
                    strategy_config.grid.risk_policy = RiskPolicy::Flatten;
                }
                Benchmark::Dynamic | Benchmark::BuyHold => {}
            }
            let id = strategy_config.base.strategy_id;
            let strategy = DynamicGridStrategy::new(strategy_config)?;
            let report = strategy.report_handle();
            engine.add_strategy(strategy)?;
            (report, id)
        }
        Benchmark::BuyHold => {
            let strategy = BuyHold::new(config.clone())?;
            let report = Rc::clone(&strategy.report);
            engine.add_strategy(strategy)?;
            (report, None)
        }
    };
    let mut data: Vec<Data> = bars.iter().copied().map(Data::Bar).collect();
    data.extend(quotes.iter().copied().map(Data::Quote));
    engine.add_data(data, None, true, true)?;
    let result = engine.run(None, None, None, false).and_then(|()| {
        if let Some(id) = grid_id {
            let mut strategy = try_get_actor_unchecked::<DynamicGridStrategy>(&id.inner())
                .ok_or_else(|| anyhow::anyhow!("Stopped grid unavailable"))?;
            strategy.finalize_after_stop()?;
        }
        Ok(())
    });
    let report = report.borrow().clone();
    engine.dispose();
    result?;
    anyhow::ensure!(
        !report.equity.is_empty(),
        "Strategy produced no equity observations"
    );
    Ok(report)
}

#[derive(Debug)]
struct BuyHold {
    core: StrategyCore,
    config: GridBacktestConfig,
    orders: OrderManager,
    risk: RiskManager,
    report: Rc<RefCell<PerformanceTracker>>,
    submitted: bool,
}

impl BuyHold {
    fn new(config: GridBacktestConfig) -> anyhow::Result<Self> {
        Ok(Self {
            core: StrategyCore::new_checked(StrategyConfig {
                strategy_id: Some(StrategyId::from("BUY-HOLD-902")),
                order_id_tag: Some("902".to_string()),
                oms_type: Some(OmsType::Netting),
                ..Default::default()
            })?,
            orders: OrderManager::new(config.grid.capital),
            risk: RiskManager::new(config.grid.capital),
            report: Rc::new(RefCell::new(PerformanceTracker::default())),
            config,
            submitted: false,
        })
    }
}

impl DataActor for BuyHold {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.config.bar_type()?, None, None);
        Ok(())
    }
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let price = bar.close.as_decimal();
        if !self.submitted {
            let lot = self.config.lot_size.as_decimal();
            let affordable = self.config.grid.capital
                / (price
                    * (Decimal::ONE
                        + self.config.grid.taker_fee
                        + self.config.grid.commission
                        + self.config.grid.slippage)
                    + self.config.price_increment.as_decimal());
            let quantity = (affordable / lot).floor() * lot;
            if quantity > Decimal::ZERO {
                let pair = GridLevel { level_index: 0, price, side: OrderSide::Buy, exit_price: price * Decimal::from(2), quantity, status: nautilus_trading::examples::strategies::dynamic_grid::engine::LevelStatus::Pending, entry_order_id: None, exit_order_id: None };
                let intent = self.orders.entry(
                    "902",
                    1,
                    &pair,
                    quantity,
                    Some(price),
                    bar.ts_event.as_u64(),
                )?;
                let order = self.order().market(
                    self.config.instrument_id,
                    OrderSide::Buy,
                    Quantity::from_decimal_dp(quantity, 0)?,
                    Some(TimeInForce::Day),
                    Some(false),
                    None,
                    None,
                    None,
                    None,
                    Some(intent.id.as_str().into()),
                );
                self.submitted = true;
                self.submit_order(order, None, None, None)?;
            }
        }
        let exposure = self.orders.inventory() * price;
        let equity = self.orders.cash + exposure;
        self.report.borrow_mut().equity.push(EquityPoint {
            trend_inventory: None,
            cumulative_fees: self.orders.fees,
            cumulative_turnover: self.orders.turnover,
            ts_ns: bar.ts_event.as_u64(),
            price,
            equity,
            exposure,
            position: self.orders.inventory(),
            utilization: if equity > Decimal::ZERO {
                (exposure / equity).to_string().parse().unwrap_or(0.0)
            } else {
                0.0
            },
            regime: MarketRegime::Disabled,
        });
        Ok(())
    }
    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.report
            .borrow_mut()
            .finish(self.config.grid.capital, &self.orders, &self.risk);
        Ok(())
    }
}

nautilus_strategy!(BuyHold, {
    fn on_order_filled(&mut self, event: &OrderFilled) {
        let fee = event.commission.map_or(Decimal::ZERO, |f| f.as_decimal());
        if let Err(e) = self.orders.fill(
            event.client_order_id.as_str(),
            event.trade_id.as_str(),
            event.last_qty.as_decimal(),
            event.last_px.as_decimal(),
            fee,
            false,
            event.ts_event.as_u64(),
        ) {
            log::error!("Buy-and-hold accounting failed: {e}");
        }
    }
});
