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

//! Local-catalog Daily OHLCV backtest for `MomentumPullbackStrategy`.
//!
//! This runner never downloads data. It converts each Daily bar into a regular-session opening
//! quote followed by a completed 16:00 New York bar. A signal calculated at a close therefore
//! cannot execute before the next session open. Resting stop orders see the opening quote first,
//! so a gap through a stop fills at the available open-side quote rather than the stop price.

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use jiff::{Timestamp, civil::Time as CivilTime, tz::TimeZone};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
    result::BacktestResult,
};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_execution::models::{
    fee::{FeeModelAny, MakerTakerFeeModel},
    fill::{FillModelAny, OneTickSlippageFillModel},
};
use nautilus_model::{
    data::{Bar, BarType, Data, QuoteTick},
    enums::{AccountType, AggregationSource, BookType, OmsType},
    identifiers::{InstrumentId, StrategyId},
    instruments::{Instrument, InstrumentAny},
    orders::Order,
    types::{Money, Quantity},
};
use nautilus_persistence::backend::catalog::ParquetDataCatalog;
use nautilus_trading::{
    examples::strategies::{
        EntryConfirmationMode, MarketRegime, MomentumPullbackConfig, MomentumPullbackReport,
        MomentumPullbackStrategy, TradeRecord,
    },
    strategy::StrategyConfig,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Deserialize;

const DEFAULT_CONFIG_PATH: &str = "crates/backtest/examples/momentum_pullback.toml";
const STRATEGY_ID: &str = "MOMENTUM-PULLBACK-001";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppConfig {
    catalog_path: String,
    start: Option<String>,
    end: Option<String>,
    starting_balance: String,
    spread_bps: Decimal,
    commission_rate: Decimal,
    slippage_probability: f64,
    random_seed: u64,
    run_sensitivity: bool,
    strategy: MomentumPullbackConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            catalog_path: String::new(),
            start: None,
            end: None,
            starting_balance: "100000 USD".to_string(),
            spread_bps: Decimal::from_str("2.0").expect("valid default"),
            commission_rate: Decimal::from_str("0.0001").expect("valid default"),
            slippage_probability: 1.0,
            random_seed: 42,
            run_sensitivity: false,
            strategy: MomentumPullbackConfig::default(),
        }
    }
}

impl AppConfig {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading backtest config {}", path.display()))?;
        let mut config: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid backtest config {}", path.display()))?;
        anyhow::ensure!(
            !config.catalog_path.trim().is_empty(),
            "catalog_path is required"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&config.slippage_probability),
            "slippage_probability must be in [0, 1]",
        );
        anyhow::ensure!(
            config.spread_bps >= Decimal::ZERO,
            "spread_bps must be non-negative"
        );
        anyhow::ensure!(
            config.commission_rate >= Decimal::ZERO,
            "commission_rate must be non-negative",
        );
        config.strategy.commission_rate = config.commission_rate;
        config.strategy.bars_are_final = true;
        config.strategy.protective_stop_uses_market_if_touched = false;
        config.strategy.base = StrategyConfig {
            strategy_id: Some(StrategyId::from(STRATEGY_ID)),
            order_id_tag: Some("501".to_string()),
            oms_type: Some(OmsType::Netting),
            ..config.strategy.base
        };
        config.strategy.validate()?;
        Ok(config)
    }

    fn start_ns(&self) -> anyhow::Result<Option<UnixNanos>> {
        self.start
            .as_deref()
            .map(Timestamp::from_str)
            .transpose()
            .map(|value| value.map(Into::into))
            .map_err(Into::into)
    }

    fn end_ns(&self) -> anyhow::Result<Option<UnixNanos>> {
        self.end
            .as_deref()
            .map(Timestamp::from_str)
            .transpose()
            .map(|value| value.map(Into::into))
            .map_err(Into::into)
    }

    fn instrument_ids(&self) -> Vec<InstrumentId> {
        let mut ids = self.strategy.universe.clone();
        ids.extend([
            self.strategy.market_regime_instrument_id,
            self.strategy.secondary_market_instrument_id,
            self.strategy.relative_strength_instrument_id,
        ]);
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

#[derive(Clone)]
struct LoadedData {
    instruments: Vec<InstrumentAny>,
    events: Vec<Data>,
    benchmark: Vec<(UnixNanos, f64)>,
}

fn load_data(config: &AppConfig) -> anyhow::Result<LoadedData> {
    let mut catalog = ParquetDataCatalog::from_uri(&config.catalog_path, None, None, None, None)?;
    let ids = config.instrument_ids();
    let id_strings: Vec<_> = ids.iter().map(ToString::to_string).collect();
    let mut instruments = catalog.instruments(Some(&id_strings), None, config.end_ns()?)?;
    instruments.sort_unstable_by_key(Instrument::id);
    instruments.dedup_by_key(|instrument| instrument.id());
    for id in &ids {
        anyhow::ensure!(
            instruments.iter().any(|instrument| instrument.id() == *id),
            "catalog is missing instrument metadata for {id}",
        );
    }
    for instrument in &mut instruments {
        let InstrumentAny::Equity(equity) = instrument else {
            anyhow::bail!(
                "MomentumPullbackStrategy V1 only accepts equities: {}",
                instrument.id()
            );
        };
        equity.maker_fee = config.commission_rate;
        equity.taker_fee = config.commission_rate;
    }
    let bar_types: Vec<_> = ids
        .iter()
        .map(|id| {
            BarType::new(
                *id,
                config.strategy.bar_specification,
                AggregationSource::External,
            )
            .to_string()
        })
        .collect();
    let bars = catalog.query_typed_data::<Bar>(
        Some(bar_types),
        config.start_ns()?,
        config.end_ns()?,
        None,
        None,
        true,
    )?;
    anyhow::ensure!(!bars.is_empty(), "catalog query returned no Daily bars");
    let instruments_by_id: HashMap<_, _> = instruments
        .iter()
        .map(|instrument| (instrument.id(), instrument))
        .collect();
    normalize_daily_events(config, bars, &instruments_by_id).map(|(events, benchmark)| LoadedData {
        instruments,
        events,
        benchmark,
    })
}

fn regular_session_timestamp(
    timezone: &TimeZone,
    source: UnixNanos,
    hour: i8,
    minute: i8,
) -> anyhow::Result<UnixNanos> {
    let date = source.to_datetime_utc().to_zoned(timezone.clone()).date();
    let time = CivilTime::new(hour, minute, 0, 0)?;
    Ok(timezone
        .to_ambiguous_timestamp(date.to_datetime(time))
        .unambiguous()
        .context("US regular-session timestamp is ambiguous")?
        .into())
}

fn normalize_daily_events(
    config: &AppConfig,
    bars: Vec<Bar>,
    instruments: &HashMap<InstrumentId, &InstrumentAny>,
) -> anyhow::Result<(Vec<Data>, Vec<(UnixNanos, f64)>)> {
    let timezone = get_timezone(&config.strategy.timezone)?;
    let half_spread = config.spread_bps / Decimal::from(20_000);
    let mut events = Vec::with_capacity(bars.len() * 3);
    let mut benchmark = Vec::new();
    for bar in bars {
        let instrument_id = bar.instrument_id();
        let instrument = instruments
            .get(&instrument_id)
            .with_context(|| format!("instrument metadata unavailable for {instrument_id}"))?;
        let open_ts = regular_session_timestamp(&timezone, bar.ts_event, 9, 30)?;
        let close_ts = regular_session_timestamp(&timezone, bar.ts_event, 16, 0)?;
        anyhow::ensure!(open_ts < close_ts, "invalid US regular-session timestamps");
        let bid = instrument
            .try_make_price_from_decimal(bar.open.as_decimal() * (Decimal::ONE - half_spread))?;
        let ask = instrument
            .try_make_price_from_decimal(bar.open.as_decimal() * (Decimal::ONE + half_spread))?;
        let quote_size = if bar.volume.as_decimal() > Decimal::ZERO {
            bar.volume
        } else {
            instrument
                .min_quantity()
                .unwrap_or_else(|| Quantity::from(1))
        };
        let opening_quote = QuoteTick::new(
            instrument_id,
            bid,
            ask.max(bid),
            quote_size,
            quote_size,
            open_ts,
            open_ts,
        );
        events.push(Data::Quote(opening_quote));
        let second_ts = open_ts
            .checked_add(1_u64)
            .context("opening quote timestamp overflowed")?;
        events.push(Data::Quote(QuoteTick {
            ts_event: second_ts,
            ts_init: second_ts,
            ..opening_quote
        }));
        let normalized = Bar::new(
            BarType::new(
                instrument_id,
                config.strategy.bar_specification,
                AggregationSource::External,
            ),
            bar.open,
            bar.high,
            bar.low,
            bar.close,
            bar.volume,
            close_ts,
            close_ts,
        );
        if instrument_id == config.strategy.relative_strength_instrument_id {
            benchmark.push((close_ts, bar.close.as_f64()));
        }
        events.push(Data::Bar(normalized));
    }
    benchmark.sort_unstable_by_key(|(ts, _)| *ts);
    benchmark.dedup_by_key(|(ts, _)| *ts);
    Ok((events, benchmark))
}

fn run_once(
    label: &str,
    app: &AppConfig,
    strategy_config: MomentumPullbackConfig,
    data: &LoadedData,
) -> anyhow::Result<()> {
    let venue = strategy_config.market_regime_instrument_id.venue;
    anyhow::ensure!(
        app.instrument_ids().iter().all(|id| id.venue == venue),
        "all configured instruments must share one venue",
    );
    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    let fill_model = FillModelAny::OneTickSlippage(OneTickSlippageFillModel::new(
        1.0,
        app.slippage_probability,
        Some(app.random_seed),
    )?);
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![
                Money::from_str(&app.starting_balance).map_err(anyhow::Error::msg)?,
            ])
            .fill_model(fill_model.into())
            .fee_model(FeeModelAny::MakerTaker(MakerTakerFeeModel).into())
            .bar_adaptive_high_low_ordering(true)
            .build()?,
    )?;
    for instrument in &data.instruments {
        engine.add_instrument(instrument)?;
    }
    let report = Arc::new(Mutex::new(MomentumPullbackReport::default()));
    engine.add_strategy(MomentumPullbackStrategy::with_report(
        strategy_config,
        Arc::clone(&report),
    )?)?;
    engine.add_data(data.events.clone(), None, true, true)?;
    engine.run(None, None, None, false)?;
    let report = report
        .lock()
        .map_err(|_| anyhow::anyhow!("strategy report lock poisoned"))?;
    print_report(
        label,
        app,
        &engine,
        &engine.get_result(),
        &report,
        &data.benchmark,
    );
    Ok(())
}

fn compounded_return(returns: &[f64]) -> f64 {
    returns
        .iter()
        .fold(1.0, |wealth, value| wealth * (1.0 + value))
        - 1.0
}

fn sample_std(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64)
        .sqrt()
}

fn sharpe(returns: &[f64]) -> f64 {
    let deviation = sample_std(returns);
    if deviation == 0.0 {
        return 0.0;
    }
    returns.iter().sum::<f64>() / returns.len() as f64 / deviation * 252.0_f64.sqrt()
}

fn sortino(returns: &[f64]) -> f64 {
    if returns.is_empty() {
        return 0.0;
    }
    let downside_deviation = (returns
        .iter()
        .map(|value| value.min(0.0).powi(2))
        .sum::<f64>()
        / returns.len() as f64)
        .sqrt();
    if downside_deviation == 0.0 {
        0.0
    } else {
        returns.iter().sum::<f64>() / returns.len() as f64 / downside_deviation * 252.0_f64.sqrt()
    }
}

fn max_drawdown(returns: &[f64]) -> f64 {
    let mut wealth: f64 = 1.0;
    let mut peak: f64 = 1.0;
    let mut drawdown: f64 = 0.0;
    for value in returns {
        wealth *= 1.0 + value;
        peak = peak.max(wealth);
        drawdown = drawdown.min(wealth / peak - 1.0);
    }
    drawdown
}

fn trade_pnl(trade: &TradeRecord) -> f64 {
    trade.realized_pnl.map_or(0.0, |money| money.as_f64())
}

fn profit_factor(gross_win: f64, gross_loss: f64) -> f64 {
    if gross_loss > 0.0 {
        gross_win / gross_loss
    } else if gross_win > 0.0 {
        f64::INFINITY
    } else {
        f64::NAN
    }
}

fn print_trade_group(label: &str, trades: &[&TradeRecord], starting_equity: f64) {
    let returns: Vec<_> = trades.iter().map(|trade| trade.realized_return).collect();
    let pnls: Vec<_> = trades.iter().map(|trade| trade_pnl(trade)).collect();
    let wins = pnls.iter().filter(|pnl| **pnl > 0.0).count();
    let gross_win = pnls.iter().filter(|pnl| **pnl > 0.0).sum::<f64>();
    let gross_loss = pnls.iter().filter(|pnl| **pnl < 0.0).sum::<f64>().abs();
    println!(
        "{label}: Return={:.2}% Sharpe={:.3} WinRate={:.2}% ProfitFactor={:.3} MaxDrawdown={:.2}% Trades={}",
        pnls.iter().sum::<f64>() / starting_equity * 100.0,
        sharpe(&returns),
        if trades.is_empty() {
            0.0
        } else {
            wins as f64 / trades.len() as f64 * 100.0
        },
        profit_factor(gross_win, gross_loss),
        max_drawdown(&returns) * 100.0,
        trades.len(),
    );
}

fn print_entry_gate_diagnostics(report: &MomentumPullbackReport) {
    let count = |name: &str| report.event_counts.get(name).copied().unwrap_or_default();
    let rate = |value: u64, total: u64| {
        if total == 0 {
            0.0
        } else {
            value as f64 / total as f64 * 100.0
        }
    };
    let evaluated = count("READY_EVALUATED");

    println!("\nREADY entry-gate diagnostics (daily close):");
    println!("Evaluations outside BEAR regime: {evaluated}");
    println!(
        "Volume failed: {} ({:.2}%)",
        count("READY_VOLUME_FAILED"),
        rate(count("READY_VOLUME_FAILED"), evaluated),
    );
    println!(
        "Breakout failed: {} ({:.2}%)",
        count("READY_BREAKOUT_FAILED"),
        rate(count("READY_BREAKOUT_FAILED"), evaluated),
    );
    println!(
        "Extension failed: {} ({:.2}%)",
        count("READY_EXTENSION_FAILED"),
        rate(count("READY_EXTENSION_FAILED"), evaluated),
    );
    println!(
        "Trend support failed: {} ({:.2}%)",
        count("READY_TREND_FAILED"),
        rate(count("READY_TREND_FAILED"), evaluated),
    );
    println!(
        "Blocked by BEAR regime before gate evaluation: {}",
        count("READY_REGIME_BLOCKED"),
    );
    println!("Sole blockers (relaxing only this gate would pass the close-time check):");
    for (label, name) in [
        ("Volume", "READY_SOLE_VOLUME_FAILED"),
        ("Breakout", "READY_SOLE_BREAKOUT_FAILED"),
        ("Extension", "READY_SOLE_EXTENSION_FAILED"),
        ("Trend support", "READY_SOLE_TREND_FAILED"),
    ] {
        println!(
            "{label}: {} ({:.2}%)",
            count(name),
            rate(count(name), evaluated)
        );
    }

    let next_session = count("NEXT_SESSION_EVALUATED");
    println!("Next-session entry validations: {next_session}");
    println!(
        "Next-session extension failed: {} ({:.2}%)",
        count("NEXT_SESSION_EXTENSION_FAILED"),
        rate(count("NEXT_SESSION_EXTENSION_FAILED"), next_session),
    );
    println!(
        "Next-session support failed: {} ({:.2}%)",
        count("NEXT_SESSION_SUPPORT_FAILED"),
        rate(count("NEXT_SESSION_SUPPORT_FAILED"), next_session),
    );
    println!("Gate failure rates may overlap; sole-blocker rates are mutually exclusive.");
}

fn print_report(
    label: &str,
    app: &AppConfig,
    engine: &BacktestEngine,
    result: &BacktestResult,
    report: &MomentumPullbackReport,
    benchmark: &[(UnixNanos, f64)],
) {
    let starting_equity = Money::from_str(&app.starting_balance)
        .expect("validated starting balance")
        .as_f64();
    let daily_returns: Vec<_> = result.returns_series.values().copied().collect();
    let pnl_values: Vec<_> = report.trades.iter().map(trade_pnl).collect();
    let wins: Vec<_> = pnl_values
        .iter()
        .copied()
        .filter(|value| *value > 0.0)
        .collect();
    let losses: Vec<_> = pnl_values
        .iter()
        .copied()
        .filter(|value| *value < 0.0)
        .collect();
    let total_return: f64 = if daily_returns.is_empty() {
        pnl_values.iter().sum::<f64>() / starting_equity
    } else {
        compounded_return(&daily_returns)
    };
    let years = benchmark
        .first()
        .zip(benchmark.last())
        .map_or(1.0 / 252.0, |(first, last)| {
            last.0.as_u64().saturating_sub(first.0.as_u64()) as f64
                / 1_000_000_000.0
                / (365.25 * 24.0 * 60.0 * 60.0)
        })
        .max(1.0 / 252.0);
    let cagr = (1.0 + total_return).max(0.0).powf(1.0 / years) - 1.0;
    let drawdown = max_drawdown(&daily_returns);
    let profit_factor = profit_factor(wins.iter().sum::<f64>(), losses.iter().sum::<f64>().abs());
    let average = |values: &[f64]| {
        if values.is_empty() {
            0.0
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        }
    };
    let mut holding: Vec<_> = report
        .trades
        .iter()
        .map(|trade| trade.holding_days)
        .collect();
    holding.sort_unstable();
    let median_holding = holding.get(holding.len() / 2).copied().unwrap_or_default();
    let (turnover, commission, slippage) = {
        let cache = engine.kernel().cache.borrow();
        let orders = cache.orders(None, None, None, None, None);
        let turnover = orders
            .iter()
            .filter_map(|order| {
                let instrument = cache.instrument(&order.instrument_id())?;
                Some(
                    order.avg_px()?
                        * order.filled_qty().as_decimal()
                        * instrument.multiplier().as_decimal(),
                )
            })
            .sum::<Decimal>()
            .to_f64()
            .unwrap_or_default()
            / starting_equity;
        let commission = orders
            .iter()
            .flat_map(|order| order.commissions().values())
            .map(|money| money.as_f64())
            .sum::<f64>();
        let slippage = orders
            .iter()
            .filter_map(|order| {
                order
                    .slippage()
                    .map(|value| value.abs() * order.filled_qty().as_decimal())
            })
            .sum::<Decimal>()
            .to_f64()
            .unwrap_or_default();
        (turnover, commission, slippage)
    };
    let benchmark_return = benchmark
        .first()
        .zip(benchmark.last())
        .map_or(0.0, |(first, last)| last.1 / first.1 - 1.0);

    println!("\n=== Momentum Pullback Backtest: {label} ===");
    println!("Total Return: {:.2}%", total_return * 100.0);
    println!("CAGR: {:.2}%", cagr * 100.0);
    println!("Sharpe Ratio: {:.3}", sharpe(&daily_returns));
    println!("Sortino Ratio: {:.3}", sortino(&daily_returns));
    println!("Max Drawdown: {:.2}%", drawdown * 100.0);
    println!(
        "Calmar Ratio: {:.3}",
        if drawdown == 0.0 {
            0.0
        } else {
            cagr / drawdown.abs()
        }
    );
    println!(
        "Win Rate: {:.2}%",
        if pnl_values.is_empty() {
            0.0
        } else {
            wins.len() as f64 / pnl_values.len() as f64 * 100.0
        }
    );
    println!("Profit Factor: {profit_factor:.3}");
    println!("Average Win: {:.2}", average(&wins));
    println!("Average Loss: {:.2}", average(&losses));
    println!("Expectancy: {:.2}", average(&pnl_values));
    println!(
        "Average Holding Period: {:.2} trading days",
        average(
            &holding
                .iter()
                .map(|value| *value as f64)
                .collect::<Vec<_>>()
        )
    );
    println!("Median Holding Period: {median_holding} trading days");
    println!("Number of Trades: {}", report.trades.len());
    println!("Turnover: {turnover:.3}x starting equity");
    println!("Commission: {commission:.2}");
    println!("Slippage: {slippage:.2}");
    println!("Long-only benchmark SPY: {:.2}%", benchmark_return * 100.0);
    println!(
        "Strategy vs SPY: {:+.2}%",
        (total_return - benchmark_return) * 100.0
    );
    println!(
        "Potential survivorship bias exists: true (the runner uses a static configured universe)"
    );
    print_entry_gate_diagnostics(report);
    println!("Built-in PnL metrics: {:?}", result.stats_pnls);
    println!("Built-in return metrics: {:?}", result.stats_returns);

    println!("\nRegime analysis:");
    for regime in [
        MarketRegime::Bull,
        MarketRegime::Neutral,
        MarketRegime::Bear,
    ] {
        let trades: Vec<_> = report
            .trades
            .iter()
            .filter(|trade| trade.entry_regime == regime)
            .collect();
        print_trade_group(&format!("{regime:?}"), &trades, starting_equity);
    }
    let mut ranked: Vec<_> = report.trades.iter().collect();
    ranked.sort_by(|left, right| trade_pnl(right).total_cmp(&trade_pnl(left)));
    println!("\nTop 10 winners:");
    for trade in ranked
        .iter()
        .filter(|trade| trade_pnl(trade) > 0.0)
        .take(10)
    {
        println!(
            "{} pnl={:.2} return={:.2}%",
            trade.symbol,
            trade_pnl(trade),
            trade.realized_return * 100.0
        );
    }
    println!("Top 10 losers:");
    for trade in ranked
        .iter()
        .rev()
        .filter(|trade| trade_pnl(trade) < 0.0)
        .take(10)
    {
        println!(
            "{} pnl={:.2} return={:.2}%",
            trade.symbol,
            trade_pnl(trade),
            trade.realized_return * 100.0
        );
    }
    let net_profit = pnl_values.iter().sum::<f64>();
    let top_five = ranked
        .iter()
        .filter(|trade| trade_pnl(trade) > 0.0)
        .take(5)
        .map(|trade| trade_pnl(trade))
        .sum::<f64>();
    println!(
        "Top 5 contribution to net profit: {:.2}%",
        if net_profit > 0.0 {
            top_five / net_profit * 100.0
        } else {
            0.0
        }
    );
    let mut distribution = pnl_values.clone();
    distribution.sort_by(f64::total_cmp);
    let percentile = |fraction: f64| {
        distribution
            .get(((distribution.len().saturating_sub(1)) as f64 * fraction).round() as usize)
            .copied()
            .unwrap_or_default()
    };
    println!(
        "PnL distribution: min={:.2} p25={:.2} median={:.2} p75={:.2} max={:.2}",
        percentile(0.0),
        percentile(0.25),
        percentile(0.5),
        percentile(0.75),
        percentile(1.0),
    );
    println!("Signal events: {:?}", report.event_counts);
}

fn sensitivity_configs(base: &MomentumPullbackConfig) -> Vec<(String, MomentumPullbackConfig)> {
    let mut variants = Vec::new();
    for (label, min, shallow, normal, max) in [
        ("pullback_3_8", 0.03, 0.04, 0.07, 0.08),
        ("pullback_5_12", 0.05, 0.06, 0.10, 0.12),
    ] {
        let mut config = base.clone();
        config.pullback_min_pct = min;
        config.pullback_shallow_max_pct = shallow;
        config.pullback_normal_max_pct = normal;
        config.pullback_max_pct = max;
        variants.push((label.to_string(), config));
    }
    for (label, multiple) in [("atr_2_0", "2.0"), ("atr_2_5", "2.5")] {
        let mut config = base.clone();
        config.atr_stop_multiple = Decimal::from_str(multiple).expect("valid sensitivity");
        variants.push((label.to_string(), config));
    }
    for lookback in [40, 60] {
        let mut config = base.clone();
        config.rs_lookback_short = lookback;
        variants.push((format!("rs_{lookback}"), config));
    }
    variants
}

fn main() -> anyhow::Result<()> {
    nautilus_common::logging::ensure_logging_initialized();
    let path = env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    let app = AppConfig::load(&path)?;
    let data = load_data(&app)?;
    run_once("baseline", &app, app.strategy.clone(), &data)?;
    if app.run_sensitivity {
        for (label, config) in sensitivity_configs(&app.strategy) {
            run_once(&label, &app, config, &data)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use nautilus_model::types::Price;
    use nautilus_testkit::common::itch_aapl_equity;

    use super::*;

    #[test]
    fn performance_math_handles_known_returns() {
        let returns = [0.10, -0.05, 0.02];
        assert!((compounded_return(&returns) - 0.0659).abs() < 1e-12);
        assert!(max_drawdown(&returns) < 0.0);
        assert!(sharpe(&returns).is_finite());
        assert!(sortino(&returns).is_finite());
        assert_eq!(profit_factor(20.0, 10.0), 2.0);
        assert!(profit_factor(20.0, 0.0).is_infinite());
        assert!(profit_factor(0.0, 0.0).is_nan());
    }

    #[test]
    fn sensitivity_is_one_factor_at_a_time() {
        let mut base = MomentumPullbackConfig::default();
        base.universe = vec![InstrumentId::from("AAPL.US.SIM")];
        let variants = sensitivity_configs(&base);
        assert_eq!(variants.len(), 6);
        assert_eq!(
            variants[0].1.atr_stop_multiple,
            Decimal::from_str("1.5").unwrap()
        );
        assert_eq!(variants[2].1.pullback_min_pct, 0.03);
        for (_, config) in variants {
            config.validate().unwrap();
        }
    }

    #[test]
    fn example_configuration_parses() {
        let config: AppConfig = toml::from_str(include_str!("momentum_pullback.toml")).unwrap();

        assert_eq!(config.strategy.universe.len(), 300);
        assert_eq!(config.strategy.min_return_short, 0.03);
        assert_eq!(config.strategy.min_return_medium, 0.06);
        assert_eq!(config.strategy.min_rs_short, 0.01);
        assert_eq!(config.strategy.min_rs_medium, 0.02);
        assert_eq!(config.strategy.minimum_momentum_score, 60.0);
        assert_eq!(config.strategy.pullback_min_pct, 0.02);
        assert_eq!(config.strategy.pullback_max_pct, 0.15);
        assert_eq!(config.strategy.min_pullback_quality_score, 45.0);
        assert_eq!(config.strategy.maximum_pullback_volume_ratio, 0.95);
        assert_eq!(config.strategy.entry_volume_multiplier, 1.0);
        assert_eq!(
            config.strategy.entry_confirmation_mode,
            EntryConfirmationMode::Relaxed,
        );
        assert_eq!(config.strategy.max_entry_extension_atr, 2.5);
        assert_eq!(
            config.strategy.max_stop_distance_pct,
            Decimal::from_str("0.10").unwrap(),
        );
        config.strategy.validate().unwrap();
    }

    #[test]
    fn daily_normalization_preserves_gap_open_before_signal_bar_close() {
        let instrument = itch_aapl_equity();
        let instrument_id = instrument.id();
        let mut config = AppConfig::default();
        config.spread_bps = Decimal::ZERO;
        config.strategy.universe = vec![instrument_id];
        config.strategy.market_regime_instrument_id = instrument_id;
        config.strategy.secondary_market_instrument_id = instrument_id;
        config.strategy.relative_strength_instrument_id = instrument_id;
        let ts: UnixNanos = Timestamp::from_str("2024-01-03T21:00:00Z").unwrap().into();
        let bar = Bar::new(
            BarType::new(
                instrument_id,
                config.strategy.bar_specification,
                AggregationSource::External,
            ),
            Price::from("90.0000"),
            Price::from("96.0000"),
            Price::from("85.0000"),
            Price::from("92.0000"),
            Quantity::from(1_000),
            ts,
            ts,
        );
        let instruments = HashMap::from([(instrument_id, &instrument)]);

        let (events, _) = normalize_daily_events(&config, vec![bar], &instruments).unwrap();

        let Data::Quote(opening_quote) = &events[0] else {
            panic!("first normalized event must be the opening quote");
        };
        let Data::Bar(close_bar) = &events[2] else {
            panic!("third normalized event must be the completed close bar");
        };
        assert_eq!(opening_quote.bid_price, Price::from("90.0000"));
        assert!(opening_quote.ts_event < close_bar.ts_event);
    }
}
