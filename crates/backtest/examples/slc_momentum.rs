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

//! Replays normalized completed bars and actual or explicitly synthetic quotes through Nautilus.
//!
//! 中文说明：普通模式读取正规化事件文件；`--history` 按日回放本地历史目录。两者都运行
//! 同一 Rust 策略和 Nautilus 撮合/风控组件，并在结束时拒绝遗留订单、持仓或策略错误。

use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead, BufReader},
    path::Path,
};

use anyhow::Context;
use jiff::civil::Date;
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::throttler::RateLimit;
use nautilus_core::{
    UnixNanos,
    datetime::{NANOSECONDS_IN_MINUTE, NANOSECONDS_IN_SECOND, get_timezone},
};
use nautilus_execution::models::{
    fee::{FeeModelAny, MakerTakerFeeModel},
    fill::{FillModelAny, SizeAwareFillModel},
};
use nautilus_model::{
    data::{Bar, BarSpecification, BarType, Data, QuoteTick},
    enums::{AccountType, AggregationSource, BarAggregation, BookType, OmsType, PriceType},
    identifiers::{InstrumentId, StrategyId},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_risk::engine::config::RiskEngineConfig;
use nautilus_trading::{
    examples::strategies::slc_momentum::{Session, SlcMomentumConfig, SlcMomentumStrategy},
    strategy::StrategyConfig,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

mod slc_momentum_history;

const USAGE: &str = "usage: cargo run -p nautilus-backtest --features examples --example slc-momentum -- INPUT.json OUTPUT.json [--start YYYY-MM-DD --end YYYY-MM-DD]\nDates are inclusive America/New_York trading dates; earlier bars warm the strategy only.\nHistory replay: cargo run -p nautilus-backtest --features examples --example slc-momentum -- --history ROOT OUTPUT.json [--start YYYY-MM-DD --end YYYY-MM-DD]";

#[derive(Debug, PartialEq, Eq, Serialize)]
struct DateRange {
    start: Date,
    end: Date,
}

fn parse_date_range(args: &[String]) -> anyhow::Result<Option<DateRange>> {
    if args.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(args.len() == 4, "{USAGE}");
    let (mut start, mut end) = (None, None);
    for pair in args.as_chunks::<2>().0 {
        let date: Date = pair[1].parse().context("invalid YYYY-MM-DD date")?;
        anyhow::ensure!(date.to_string() == pair[1], "dates must use YYYY-MM-DD");
        let target = match pair[0].as_str() {
            "--start" => &mut start,
            "--end" => &mut end,
            _ => anyhow::bail!("unknown argument {}; {USAGE}", pair[0]),
        };
        anyhow::ensure!(target.replace(date).is_none(), "duplicate {}", pair[0]);
    }
    let range = DateRange {
        start: start.context("--start is required with --end")?,
        end: end.context("--end is required with --start")?,
    };
    anyhow::ensure!(
        range.start <= range.end,
        "start date must not exceed end date"
    );
    Ok(Some(range))
}

fn select_date_range(config: &mut SlcMomentumConfig, range: &DateRange) -> anyhow::Result<()> {
    let timezone = get_timezone("America/New_York")?;
    let date = |s: &Session| s.open.to_datetime_utc().to_zoned(timezone.clone()).date();
    let first = config
        .sessions
        .first()
        .context("session calendar is empty")?;
    let last = config
        .sessions
        .last()
        .context("session calendar is empty")?;
    anyhow::ensure!(
        range.start >= date(first) && range.end <= date(last),
        "requested dates exceed supplied calendar coverage; provide the complete calendar"
    );
    let mut selected = config
        .sessions
        .iter()
        .filter(|s| range.start <= date(s) && date(s) <= range.end);
    let first = selected
        .next()
        .context("date range contains no trading sessions")?;
    let last = selected.next_back().unwrap_or(first);
    config.trading_start = first.open;
    config.trading_end = last.close;
    Ok(())
}

fn validate_quote_coverage(events: &[Data], config: &SlcMomentumConfig) -> anyhow::Result<()> {
    let mut coverage = BTreeMap::<UnixNanos, (UnixNanos, UnixNanos)>::new();
    for event in events {
        let Data::Quote(q) = event else { continue };
        let index = config.sessions.partition_point(|s| s.open <= q.ts_event);
        if let Some(index) = index.checked_sub(1) {
            let session = config.sessions[index];
            if q.ts_event <= session.close {
                let bounds = coverage
                    .entry(session.open)
                    .or_insert((q.ts_event, q.ts_event));
                bounds.0 = bounds.0.min(q.ts_event);
                bounds.1 = bounds.1.max(q.ts_event);
            }
        }
    }
    for session in &config.sessions {
        if session.close < config.trading_start || session.open > config.trading_end {
            continue;
        }
        let (first, last) = coverage
            .get(&session.open)
            .with_context(|| format!("missing quote coverage for session {}", session.open))?;
        let start = session.open.max(config.trading_start).as_u64();
        let end = session.close.min(config.trading_end).as_u64();
        anyhow::ensure!(
            first.as_u64() <= start.saturating_add(NANOSECONDS_IN_MINUTE)
                && last.as_u64() >= end.saturating_sub(NANOSECONDS_IN_MINUTE),
            "incomplete quote coverage for session {}; provide quotes through session end",
            session.open
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    synthetic: bool,
    provenance: String,
    strategy: SlcMomentumConfig,
    starting_equity: Decimal,
    price_increments: BTreeMap<InstrumentId, Price>,
    events_path: String,
    #[serde(default = "one")]
    spread_multiplier: Decimal,
    #[serde(default)]
    slippage_probability: Option<f64>,
}

fn one() -> Decimal {
    Decimal::ONE
}

/// Formats one completed trade using actual fills and net PnL.
fn format_trade_log(trade: &serde_json::Value) -> anyhow::Result<String> {
    fn text<'a>(value: &'a serde_json::Value, field: &str) -> anyhow::Result<&'a str> {
        value
            .as_str()
            .with_context(|| format!("missing trade log field {field}"))
    }
    let signal = &trade["signal"];
    let allocation = &trade["allocation"];
    let symbol = text(&signal["symbol"], "signal.symbol")?;
    let opened_at = trade["opened_at"]
        .as_u64()
        .context("missing trade log field opened_at")?;
    let entry_value: Decimal = text(&trade["entry_value"], "entry_value")?.parse()?;
    let quantity: Decimal = text(&trade["quantity"], "quantity")?.parse()?;
    let pnl: Decimal = text(&trade["pnl"], "pnl")?.parse()?;
    let mfe: Decimal = text(&trade["mfe"], "mfe")?.parse()?;
    let mae: Decimal = text(&trade["mae"], "mae")?.parse()?;
    let mfe_r: Decimal = text(&trade["mfe_r_multiple"], "mfe_r_multiple")?.parse()?;
    let mae_r: Decimal = text(&trade["mae_r_multiple"], "mae_r_multiple")?.parse()?;
    let capture = trade["profit_capture_ratio"]
        .as_str()
        .map(|value| {
            value
                .parse::<Decimal>()
                .map(|value| value.round_dp(4).normalize())
        })
        .transpose()?
        .map_or_else(|| "N/A".to_string(), |value| value.to_string());
    let excursion_state = text(&trade["excursion_state"], "excursion_state")?;
    let confirmation_flags = signal["confirmation_flags"]
        .as_u64()
        .context("missing trade log field signal.confirmation_flags")?;
    let confirmation_enabled = signal["confirmation_enabled"]
        .as_u64()
        .context("missing trade log field signal.confirmation_enabled")?;
    anyhow::ensure!(
        entry_value > Decimal::ZERO && quantity > Decimal::ZERO,
        "completed trade must have positive entry value and quantity"
    );
    let entry_price = (entry_value / quantity).round_dp(6).normalize();
    // 收益率采用已扣费用的净 PnL / 实际入场成交金额。
    let return_pct = (pnl / entry_value * Decimal::ONE_HUNDRED)
        .round_dp(4)
        .normalize();
    let entry_time = UnixNanos::from(opened_at)
        .to_datetime_utc()
        .to_zoned(get_timezone("America/New_York")?);
    Ok(format!(
        "BACKTEST_TRADE symbol={symbol} entry_time_et={entry_time} invested_usd={} entry_price={entry_price} take_profit={} stop_loss={} net_pnl={} return_pct={return_pct}% mfe_usd={} mae_usd={} mfe_r={} mae_r={} profit_capture_ratio={capture} excursion_state={excursion_state} confirmation_flags=0x{confirmation_flags:02X} confirmation_enabled=0x{confirmation_enabled:02X}",
        entry_value.normalize(),
        text(&allocation["target"], "allocation.target")?,
        text(&allocation["stop"], "allocation.stop")?,
        pnl.normalize(),
        mfe.round_dp(4).normalize(),
        mae.round_dp(4).normalize(),
        mfe_r.round_dp(4).normalize(),
        mae_r.round_dp(4).normalize(),
    ))
}

fn print_trade_logs(report: &serde_json::Value) -> anyhow::Result<()> {
    for trade in report["trades"]
        .as_array()
        .context("missing report trades")?
    {
        println!("{}", format_trade_log(trade)?);
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum InputEvent {
    Market {
        update: nautilus_trading::examples::strategies::slc_momentum::MarketUpdate,
    },
    Bar {
        symbol: InstrumentId,
        timestamp: UnixNanos,
        available_at: UnixNanos,
        daily: bool,
        open: Price,
        high: Price,
        low: Price,
        close: Price,
        volume: Quantity,
    },
    Quote {
        symbol: InstrumentId,
        timestamp: UnixNanos,
        available_at: UnixNanos,
        bid: Price,
        ask: Price,
        bid_size: Quantity,
        ask_size: Quantity,
    },
}

impl InputEvent {
    fn into_data(
        self,
        spread_multiplier: Decimal,
        increments: &BTreeMap<InstrumentId, Price>,
    ) -> anyhow::Result<Data> {
        match self {
            Self::Market { update } => Ok(update.into_data()),
            Self::Bar {
                symbol,
                timestamp,
                available_at,
                daily,
                open,
                high,
                low,
                close,
                volume,
            } => {
                anyhow::ensure!(
                    available_at >= timestamp
                        && low > Price::from("0")
                        && low <= open
                        && low <= close
                        && high >= open
                        && high >= close,
                    "invalid or unavailable OHLC input"
                );
                let aggregation = if daily {
                    BarAggregation::Day
                } else {
                    BarAggregation::Minute
                };
                let bar_type = BarType::new(
                    symbol,
                    BarSpecification::new(1, aggregation, PriceType::Last),
                    AggregationSource::External,
                );
                Ok(Data::Bar(Bar::new(
                    bar_type,
                    open,
                    high,
                    low,
                    close,
                    volume,
                    timestamp,
                    available_at,
                )))
            }
            Self::Quote {
                symbol,
                timestamp,
                available_at,
                bid,
                ask,
                bid_size,
                ask_size,
            } => {
                anyhow::ensure!(
                    available_at >= timestamp && bid > Price::from("0") && ask >= bid,
                    "invalid quote"
                );
                let increment = increments.get(&symbol).context("missing quote increment")?;
                let tick = increment.as_decimal();
                let mid = (bid.as_decimal() + ask.as_decimal()) / Decimal::from(2);
                let half =
                    (ask.as_decimal() - bid.as_decimal()) / Decimal::from(2) * spread_multiplier;
                let bid = Price::from_decimal_dp(
                    ((mid - half) / tick).floor() * tick,
                    increment.precision,
                )?;
                let ask = Price::from_decimal_dp(
                    ((mid + half) / tick).ceil() * tick,
                    increment.precision,
                )?;
                anyhow::ensure!(
                    bid > Price::from("0"),
                    "cost stress makes quote nonpositive"
                );
                Ok(Data::Quote(QuoteTick::new(
                    symbol,
                    bid,
                    ask,
                    bid_size,
                    ask_size,
                    timestamp,
                    available_at,
                )))
            }
        }
    }
}

fn run(input_path: &Path, output_path: &Path, dates: Option<&DateRange>) -> anyhow::Result<()> {
    let mut input: RunInput = serde_json::from_str(&fs::read_to_string(input_path)?)?;
    if let Some(dates) = dates {
        select_date_range(&mut input.strategy, dates)?;
    }
    input.strategy.validate()?;
    anyhow::ensure!(
        !input.provenance.trim().is_empty() && input.starting_equity > Decimal::ZERO,
        "data provenance and positive initial equity are required"
    );
    anyhow::ensure!(
        input.spread_multiplier >= Decimal::ONE && input.spread_multiplier <= Decimal::from(10),
        "spread multiplier must be in 1..=10"
    );
    anyhow::ensure!(
        !input.strategy.longbridge,
        "backtest input must already be final and close-stamped"
    );
    input.strategy.base = StrategyConfig {
        strategy_id: Some(StrategyId::from("SLC-MOMENTUM-001")),
        order_id_tag: Some("803".to_string()),
        ..input.strategy.base
    };
    let mut engine_config = BacktestEngineConfig::default();
    engine_config.logging.stdout_level = log::LevelFilter::Warn;
    // ponytail: ten commands per instrument covers minute-batched research; make configurable for denser projections.
    let rate_limit = input
        .strategy
        .instrument_ids()
        .len()
        .saturating_mul(10)
        .max(100);
    engine_config.risk_engine = Some(
        RiskEngineConfig::builder()
            .max_order_submit(RateLimit::new(rate_limit, NANOSECONDS_IN_SECOND))
            .max_order_modify(RateLimit::new(rate_limit, NANOSECONDS_IN_SECOND))
            .build()?,
    );
    let mut engine = BacktestEngine::new(engine_config)?;
    let venue = input.strategy.benchmarks[0].venue;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![Money::from_decimal(
                input.starting_equity,
                Currency::USD(),
            )?])
            .bar_execution(false)
            .trade_execution(false)
            .fill_model(
                FillModelAny::SizeAware(SizeAwareFillModel::new(
                    1.0,
                    input.slippage_probability.unwrap_or(1.0),
                    Some(42),
                )?)
                .into(),
            )
            .fee_model(FeeModelAny::MakerTaker(MakerTakerFeeModel).into())
            .build()?,
    )?;
    for id in input.strategy.instrument_ids() {
        let tick = *input
            .price_increments
            .get(&id)
            .with_context(|| format!("missing price increment for {id}"))?;
        let equity = Equity::builder()
            .instrument_id(id)
            .raw_symbol(id.symbol)
            .currency(Currency::USD())
            .price_precision(tick.precision)
            .price_increment(tick)
            .lot_size(Quantity::from(1))
            .maker_fee(input.strategy.risk.commission_rate)
            .taker_fee(input.strategy.risk.commission_rate)
            .ts_event(UnixNanos::default())
            .ts_init(UnixNanos::default())
            .build()?;
        engine.add_instrument(&InstrumentAny::Equity(equity))?;
    }
    let path = input_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(&input.events_path);
    let reader = BufReader::new(
        fs::File::open(&path).with_context(|| format!("cannot read {}", path.display()))?,
    );
    let mut events = Vec::new();
    let mut history = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("cannot read event line {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: InputEvent = serde_json::from_str(&line)
            .with_context(|| format!("invalid event line {}", index + 1))?;
        let event = event.into_data(input.spread_multiplier, &input.price_increments)?;
        let available = match &event {
            Data::Bar(b) => b.ts_init,
            Data::Quote(q) => q.ts_init,
            Data::Custom(c) => c.data.ts_init(),
            _ => unreachable!("normalized input only contains bars and quotes"),
        };
        if available < input.strategy.trading_start {
            if let Data::Bar(b) = event {
                history.push(b);
            }
        } else if available <= input.strategy.trading_end {
            events.push(event);
        }
    }
    anyhow::ensure!(
        events.iter().any(|e| matches!(e, Data::Quote(_))),
        "quote data is required; bar-only ideal fills are disabled"
    );
    validate_quote_coverage(&events, &input.strategy)?;
    let warmup_bars = history.len();
    let replay_events = events.len();
    let mut strategy = SlcMomentumStrategy::new(input.strategy.clone())?;
    strategy.warmup(history)?;
    let report = strategy.report_handle();
    engine.add_strategy(strategy)?;
    engine.add_data(events, None, true, true)?;
    engine.run(None, None, None, false)?;
    let report = report
        .lock()
        .map_err(|_| anyhow::anyhow!("report lock poisoned"))?;
    let remaining = engine
        .kernel()
        .cache
        .borrow()
        .positions_open(None, None, None, None, None)
        .len();
    let remaining_orders = engine
        .kernel()
        .cache
        .borrow()
        .orders_open(None, None, None, None, None)
        .len();
    let statistics = engine.kernel().portfolio.borrow().statistics();
    let net_pnl: Decimal = report.trades.iter().map(|t| t.pnl).sum();
    let output = serde_json::json!({
        "synthetic": input.synthetic, "provenance": input.provenance,
        "starting_equity": input.starting_equity, "configuration": input.strategy,
        "execution_model": "Quotes only; native size-aware impact, probabilistic one-tick slippage, maker/taker commission",
        "spread_multiplier": input.spread_multiplier,
        "slippage_probability": input.slippage_probability.unwrap_or(1.0),
        "requested_dates": dates, "date_timezone": "America/New_York",
        "warmup_bars": warmup_bars, "replay_events": replay_events,
        "native_statistics": {
            "pnls": statistics.pnls, "returns": statistics.returns,
            "general": statistics.general, "returns_series": statistics.returns_series,
        },
        "net_pnl": net_pnl,
        "remaining_positions": remaining, "remaining_orders": remaining_orders, "report": *report,
    });
    fs::write(output_path, serde_json::to_vec_pretty(&output)?)?;
    anyhow::ensure!(
        remaining == 0 && remaining_orders == 0,
        "backtest ended with open positions or orders; extend data through flatten and fills"
    );
    anyhow::ensure!(
        report.errors.is_empty(),
        "strategy halted; inspect output errors"
    );
    print_trade_logs(&output["report"])?;
    println!(
        "{} trades, {} signals, net PnL {} USD; report={}",
        report.trades.len(),
        report.signals.len(),
        net_pnl,
        output_path.display()
    );
    Ok(())
}

pub(crate) fn main() -> anyhow::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--history") {
        anyhow::ensure!(
            args.len() >= 3,
            "usage: cargo run -p nautilus-backtest --features examples --example slc-momentum -- --history ROOT OUTPUT [--start DATE --end DATE]"
        );
        let dates = parse_date_range(&args[3..])?;
        return slc_momentum_history::replay(
            Path::new(&args[1]),
            Path::new(&args[2]),
            dates.as_ref(),
        );
    }
    anyhow::ensure!(args.len() >= 2, "{USAGE}");
    let dates = parse_date_range(&args[2..])?;
    run(Path::new(&args[0]), Path::new(&args[1]), dates.as_ref())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn session(date: &str, hour: i8) -> Session {
        let date: Date = date.parse().unwrap();
        let zone = get_timezone("America/New_York").unwrap();
        Session {
            open: date
                .at(9, 30, 0, 0)
                .to_zoned(zone.clone())
                .unwrap()
                .timestamp()
                .into(),
            close: date
                .at(hour, 0, 0, 0)
                .to_zoned(zone)
                .unwrap()
                .timestamp()
                .into(),
        }
    }

    #[rstest]
    fn parses_inclusive_dates_and_preserves_legacy_arguments() {
        let args = ["--end", "2025-03-10", "--start", "2025-03-07"].map(String::from);
        assert_eq!(
            parse_date_range(&args).unwrap(),
            Some(DateRange {
                start: "2025-03-07".parse().unwrap(),
                end: "2025-03-10".parse().unwrap()
            })
        );
        assert_eq!(parse_date_range(&[]).unwrap(), None);
    }

    #[rstest]
    fn completed_trade_log_contains_fill_prices_and_net_return() {
        let trade = serde_json::json!({
            "signal": {"symbol": "SPY.US.LONGBRIDGE", "confirmation_flags": 223, "confirmation_enabled": 255},
            "allocation": {"target": "102.00", "stop": "99.00"},
            "opened_at": 1_757_682_000_000_000_000_u64,
            "entry_value": "10000.00",
            "quantity": "100",
            "pnl": "125.00",
            "mfe": "300.00",
            "mae": "75.00",
            "mfe_r_multiple": "1.5",
            "mae_r_multiple": "0.375",
            "profit_capture_ratio": "0.4166666667",
            "excursion_state": "FAVORABLE_CAPTURED"
        });
        let line = format_trade_log(&trade).unwrap();
        assert!(line.contains("symbol=SPY.US.LONGBRIDGE"));
        assert!(line.contains("invested_usd=10000"));
        assert!(line.contains("entry_price=100"));
        assert!(line.contains("take_profit=102.00 stop_loss=99.00"));
        assert!(line.contains("return_pct=1.25%"));
        assert!(line.contains("mfe_usd=300 mae_usd=75"));
        assert!(line.contains("excursion_state=FAVORABLE_CAPTURED"));
        assert!(line.contains("confirmation_flags=0xDF confirmation_enabled=0xFF"));
    }

    #[rstest]
    #[case(vec!["--start", "2025-03-07"])]
    #[case(vec!["--start", "2025-03-11", "--end", "2025-03-07"])]
    #[case(vec!["--start", "2025-02-30", "--end", "2025-03-07"])]
    #[case(vec!["--start", "2025-03-07T00:00:00Z", "--end", "2025-03-10"])]
    #[case(vec!["--start", "2025-03-07", "--start", "2025-03-10"])]
    #[case(vec!["--star", "2025-03-07", "--end", "2025-03-10"])]
    fn rejects_invalid_date_arguments(#[case] args: Vec<&str>) {
        let args = args.into_iter().map(String::from).collect::<Vec<_>>();
        assert!(parse_date_range(&args).is_err());
    }

    #[rstest]
    fn dates_use_calendar_dst_and_keep_prior_warmup_sessions() {
        let mut c = SlcMomentumConfig {
            sessions: vec![
                session("2025-03-06", 16),
                session("2025-03-07", 16),
                session("2025-03-10", 16),
            ],
            ..Default::default()
        };
        let range = DateRange {
            start: "2025-03-07".parse().unwrap(),
            end: "2025-03-10".parse().unwrap(),
        };
        select_date_range(&mut c, &range).unwrap();
        assert_eq!(
            c.trading_start,
            "2025-03-07T14:30:00Z".parse::<UnixNanos>().unwrap()
        );
        assert_eq!(
            c.trading_end,
            "2025-03-10T20:00:00Z".parse::<UnixNanos>().unwrap()
        );
        assert_eq!(c.sessions[0].open, session("2025-03-06", 16).open);
        assert_eq!(c.sessions[0].close, session("2025-03-06", 16).close);
    }

    #[rstest]
    fn a_single_half_day_uses_its_actual_close() {
        let mut c = SlcMomentumConfig {
            sessions: vec![
                session("2025-11-26", 16),
                session("2025-11-28", 13),
                session("2025-12-01", 16),
            ],
            ..Default::default()
        };
        let range = DateRange {
            start: "2025-11-27".parse().unwrap(),
            end: "2025-11-30".parse().unwrap(),
        };
        select_date_range(&mut c, &range).unwrap();
        assert_eq!(c.trading_start, c.sessions[1].open);
        assert_eq!(c.trading_end, c.sessions[1].close);
    }

    #[rstest]
    #[case("2025-03-08", "2025-03-09", "no trading sessions")]
    #[case("2025-03-06", "2025-03-10", "calendar coverage")]
    #[case("2025-03-07", "2025-03-11", "calendar coverage")]
    fn rejects_empty_or_uncovered_ranges(
        #[case] start: &str,
        #[case] end: &str,
        #[case] message: &str,
    ) {
        let mut c = SlcMomentumConfig {
            sessions: vec![session("2025-03-07", 16), session("2025-03-10", 16)],
            ..Default::default()
        };
        let range = DateRange {
            start: start.parse().unwrap(),
            end: end.parse().unwrap(),
        };
        assert!(
            select_date_range(&mut c, &range)
                .unwrap_err()
                .to_string()
                .contains(message)
        );
    }

    fn quote(at: UnixNanos) -> Data {
        Data::Quote(QuoteTick::new(
            "SPY.SIM".into(),
            Price::from("100.00"),
            Price::from("100.01"),
            Quantity::from(100),
            Quantity::from(100),
            at,
            at,
        ))
    }

    #[rstest]
    fn incomplete_data_cannot_silently_shorten_a_requested_backtest() {
        let first = session("2025-03-07", 16);
        let second = session("2025-03-10", 16);
        let c = SlcMomentumConfig {
            sessions: vec![first, second],
            trading_start: first.open,
            trading_end: second.close,
            ..Default::default()
        };
        let mut events = vec![quote(first.open), quote(first.close), quote(second.open)];
        assert!(validate_quote_coverage(&events, &c).is_err());
        events.push(quote(second.close));
        assert!(validate_quote_coverage(&events, &c).is_ok());
        assert!(validate_quote_coverage(&events[2..], &c).is_err());
    }
}
