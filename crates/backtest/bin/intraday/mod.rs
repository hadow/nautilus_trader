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

//! Native replay and report generation; no Python or broker dependencies.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Context;
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_core::UnixNanos;
use nautilus_execution::models::fee::{FeeModelHandle, PerContractFeeModel};
use nautilus_model::{
    accounts::Account,
    data::{Bar, CustomData, Data, QuoteTick, bar_vwap::BarWithVwap},
    enums::{AccountType, BookType, OmsType},
    identifiers::{InstrumentId, StrategyId},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::{
    examples::strategies::intraday_momentum::{
        IntradayMomentumConfig, IntradayMomentumReport, IntradayMomentumSession,
        IntradayMomentumStrategy,
        reference::{
            MINUTE, MinuteBar, ModelConfig, PositionTarget, ReferenceModel, Session, Timing,
        },
    },
    strategy::StrategyConfig,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Config {
    pub symbol: String,
    pub model: ModelConfig,
    pub initial_equity: Decimal,
    pub commission_per_share: Decimal,
    pub slippage_per_share: Decimal,
    pub realistic: bool,
    pub spread_bps: Decimal,
    pub impact_bps: Decimal,
    pub flatten_minutes: u64,
    pub retain_features: bool,
    pub max_entries_per_day: Option<usize>,
    pub max_daily_loss: Decimal,
    pub risk_per_trade: Option<Decimal>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            symbol: "SPY".to_string(),
            model: ModelConfig::default(),
            initial_equity: Decimal::from(100_000),
            commission_per_share: Decimal::new(35, 4),
            slippage_per_share: Decimal::new(1, 3),
            realistic: false,
            spread_bps: Decimal::ONE,
            impact_bps: Decimal::new(5, 1),
            flatten_minutes: 1,
            retain_features: false,
            max_entries_per_day: None,
            max_daily_loss: Decimal::ONE,
            risk_per_trade: None,
        }
    }
}

pub(super) fn load(path: &Path) -> anyhow::Result<Vec<(Session, MinuteBar)>> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut bars = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if index == 0 {
            anyhow::ensure!(
                line == "timestamp_ns,session_open,session_close,open,high,low,close,volume,vwap",
                "unexpected CSV schema"
            );
            continue;
        }
        let fields = line.split(',').collect::<Vec<_>>();
        anyhow::ensure!(
            fields.len() == 9,
            "line {} has invalid field count",
            index + 1
        );
        let session = Session {
            open: fields[1].parse()?,
            close: fields[2].parse()?,
        };
        let bar = MinuteBar {
            timestamp: fields[0].parse()?,
            open: fields[3].parse()?,
            high: fields[4].parse()?,
            low: fields[5].parse()?,
            close: fields[6].parse()?,
            volume: fields[7].parse()?,
            vwap: fields[8].parse()?,
        };
        if let Some((_, last)) = bars.last() {
            let last: &MinuteBar = last;
            anyhow::ensure!(
                last.timestamp < bar.timestamp,
                "CSV must be strictly ordered and unique"
            );
        }
        bars.push((session, bar));
    }
    anyhow::ensure!(!bars.is_empty(), "empty input");
    Ok(bars)
}

pub(super) fn date(timestamp: u64) -> anyhow::Result<String> {
    Ok(jiff::Timestamp::from_nanosecond(i128::from(timestamp))?
        .to_zoned(nautilus_core::datetime::get_timezone("America/New_York")?)
        .date()
        .to_string())
}

fn native(bar: MinuteBar, id: InstrumentId) -> anyhow::Result<BarWithVwap> {
    let ts = UnixNanos::from(bar.timestamp);
    let kind = format!("{id}-1-MINUTE-LAST-EXTERNAL").parse()?;
    Ok(BarWithVwap {
        bar: Bar::new(
            kind,
            Price::from_decimal(bar.open)?,
            Price::from_decimal(bar.high)?,
            Price::from_decimal(bar.low)?,
            Price::from_decimal(bar.close)?,
            Quantity::from_decimal(bar.volume)?,
            ts,
            ts,
        ),
        vwap: bar.vwap,
    })
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Daily {
    pub date: String,
    pub equity: Decimal,
    pub return_value: f64,
    pub drawdown: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(super) struct Trade {
    pub entry_timestamp: u64,
    pub exit_timestamp: u64,
    pub side: i32,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub pnl: Decimal,
    pub cost: Decimal,
}

#[derive(Debug, Serialize)]
pub(super) struct Outcome {
    pub metrics: serde_json::Value,
    pub daily: Vec<Daily>,
    pub trades: Vec<Trade>,
    pub report: IntradayMomentumReport,
    pub parity: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ReferenceFill {
    timestamp: u64,
    side: i32,
    quantity: Decimal,
    price: Decimal,
    cost: Decimal,
}

/// The oracle receives known features and scheduled opens; it does not simulate an order book.
fn reference_fills(
    bars: &[(Session, MinuteBar)],
    config: &Config,
) -> anyhow::Result<Vec<ReferenceFill>> {
    let mut model = ReferenceModel::new(config.model.clone())?;
    let mut equity = config.initial_equity;
    let mut cash = equity;
    let mut position = PositionTarget::Flat;
    let mut quantity = Decimal::ZERO;
    let mut current_session = None;
    let mut pending: Option<(u64, PositionTarget)> = None;
    let mut result = Vec::new();
    let mut entries_today = 0;
    for (session, bar) in bars {
        if current_session != Some(session.open) {
            anyhow::ensure!(
                position == PositionTarget::Flat,
                "reference retained overnight exposure"
            );
            equity = cash;
            current_session = Some(session.open);
            quantity = Decimal::ZERO;
            entries_today = 0;
        }
        if let Some((timestamp, target)) = pending.take()
            && target != position
        {
            let target = if config
                .max_entries_per_day
                .is_some_and(|limit| entries_today >= limit)
            {
                PositionTarget::Flat
            } else {
                target
            };
            if target != PositionTarget::Flat {
                entries_today += 1;
            }
            for signed in [
                -(Decimal::from(position.sign()) * quantity),
                Decimal::from(target.sign()) * quantity,
            ] {
                if signed == Decimal::ZERO {
                    continue;
                }
                let side = if signed > Decimal::ZERO { 1 } else { -1 };
                let cost = (signed.abs()
                    * (config.commission_per_share + config.slippage_per_share))
                    .round_dp(2);
                cash -= signed * bar.open + cost;
                result.push(ReferenceFill {
                    timestamp,
                    side,
                    quantity: signed.abs(),
                    price: bar.open,
                    cost,
                });
            }
            position = target;
        }
        let f = model.on_bar(*session, *bar)?;
        if f.decision_time && quantity == Decimal::ZERO {
            quantity = (equity * f.leverage / f.daily_open).floor();
        }
        if bar.timestamp == session.close - config.flatten_minutes * MINUTE {
            pending = Some((bar.timestamp, PositionTarget::Flat));
        } else if f.decision_time && bar.timestamp < session.close - config.flatten_minutes * MINUTE
        {
            pending = Some((bar.timestamp, f.target));
        }
    }
    anyhow::ensure!(
        position == PositionTarget::Flat,
        "reference is not flat at end"
    );
    Ok(result)
}

pub(super) fn run(bars: &[(Session, MinuteBar)], config: &Config) -> anyhow::Result<Outcome> {
    anyhow::ensure!(
        !config.symbol.is_empty()
            && config.symbol.len() <= 15
            && config
                .symbol
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-'),
        "invalid symbol"
    );
    config.model.validate()?;
    anyhow::ensure!(
        !config.model.baseline_stop,
        "this runner reproduces the final Notebook refined-stop variant"
    );
    anyhow::ensure!(
        config.initial_equity > Decimal::ZERO
            && config.commission_per_share >= Decimal::ZERO
            && config.slippage_per_share >= Decimal::ZERO,
        "invalid money/cost configuration"
    );
    anyhow::ensure!(
        config.flatten_minutes > 0 && config.flatten_minutes < 30,
        "flatten lead must be 1..29 minutes"
    );
    anyhow::ensure!(
        config.spread_bps >= Decimal::ZERO && config.impact_bps >= Decimal::ZERO,
        "negative spread/impact"
    );
    let sessions = bars
        .iter()
        .map(|(s, _)| (s.open, *s))
        .collect::<BTreeMap<_, _>>();
    let trading_open = sessions
        .values()
        .nth(config.model.lookback)
        .context("insufficient sessions after sigma warmup")?
        .open;
    let warmup_count = bars.partition_point(|(s, _)| s.open < trading_open);
    let id = format!("{}.SIM", config.symbol).parse::<InstrumentId>()?;
    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(id.venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![Money::from_decimal(
                config.initial_equity,
                Currency::USD(),
            )?])
            .base_currency(Currency::USD())
            .default_leverage(Decimal::from(4))
            .bar_execution(false)
            .trade_execution(false)
            .fee_model(FeeModelHandle::new(PerContractFeeModel::from_rate(
                config.commission_per_share
                    + if config.realistic {
                        Decimal::ZERO
                    } else {
                        config.slippage_per_share
                    },
                Currency::USD(),
            )?))
            .build()?,
    )?;
    let instrument = InstrumentAny::Equity(
        Equity::builder()
            .instrument_id(id)
            .raw_symbol(config.symbol.as_str().into())
            .currency(Currency::USD())
            .price_precision(4)
            .price_increment(Price::from("0.0001"))
            .lot_size(Quantity::from(1))
            .margin_init(Decimal::new(25, 2))
            .margin_maint(Decimal::new(25, 2))
            .ts_event(0.into())
            .ts_init(0.into())
            .build()?,
    );
    engine.add_instrument(&instrument)?;
    let report = Arc::new(Mutex::new(IntradayMomentumReport::default()));
    let strategy_config = IntradayMomentumConfig {
        instrument_id: id,
        sessions: sessions
            .values()
            .map(|s| IntradayMomentumSession {
                open: s.open.into(),
                close: s.close.into(),
                dividend: Decimal::ZERO,
            })
            .collect(),
        lookback_days: config.model.lookback,
        volume_lookback_days: config.model.volume_lookback,
        volatility_lookback_days: config.model.volatility_lookback,
        volatility_multiplier: config.model.volatility_multiplier,
        relative_volume_threshold: config.model.relative_volume_threshold,
        target_daily_volatility: config.model.target_volatility,
        max_leverage: config.model.max_leverage,
        timing: config.model.timing,
        directional_stops: config.model.directional_stops,
        decision_interval_minutes: config.model.interval_minutes as u64,
        flatten_before_close_minutes: config.flatten_minutes,
        dry_run: false,
        defer_to_next_quote: true,
        max_position_notional: Decimal::from(1_000_000_000_000_i64),
        max_order_notional: Decimal::from(1_000_000_000_000_i64),
        max_daily_loss: config.max_daily_loss,
        risk_per_trade: config.risk_per_trade,
        retain_features: config.retain_features,
        max_entries_per_day: config.max_entries_per_day,
        base: StrategyConfig {
            strategy_id: Some(StrategyId::from("INTRADAY-001")),
            oms_type: Some(OmsType::Netting),
            market_exit_reduce_only: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut strategy = IntradayMomentumStrategy::with_report(strategy_config, Arc::clone(&report))?;
    let warmup = bars[..warmup_count]
        .iter()
        .map(|(_, bar)| native(*bar, id))
        .collect::<anyhow::Result<Vec<_>>>()?;
    strategy.warmup_with_vwap(&warmup)?;
    engine.add_strategy(strategy)?;
    let mut daily = Vec::new();
    let mut before = config.initial_equity;
    let mut peak = before;
    let mut cursor = warmup_count;
    while cursor < bars.len() {
        let session = bars[cursor].0;
        let end = cursor + bars[cursor..].partition_point(|(s, _)| s.open == session.open);
        let mut events = Vec::with_capacity((end - cursor) * 2 + 1);
        for (_, bar) in &bars[cursor..end] {
            let ts = UnixNanos::from(bar.timestamp - MINUTE + 1);
            let friction = if config.realistic {
                bar.open * (config.spread_bps / Decimal::from(2) + config.impact_bps)
                    / Decimal::from(10_000)
                    + config.slippage_per_share
            } else {
                Decimal::ZERO
            };
            let quote = QuoteTick::new(
                id,
                Price::from_decimal_dp(bar.open - friction, 4)?,
                Price::from_decimal_dp(bar.open + friction, 4)?,
                Quantity::from(1_000_000_000),
                Quantity::from(1_000_000_000),
                ts,
                ts,
            );
            events.push(Data::Quote(quote));
            events.push(Data::Custom(CustomData::from_arc(Arc::new(native(
                *bar, id,
            )?))));
        }
        let last = bars[end - 1].1;
        let ts = UnixNanos::from(session.close + 2);
        events.push(Data::Quote(QuoteTick::new(
            id,
            Price::from_decimal_dp(last.close, 4)?,
            Price::from_decimal_dp(last.close, 4)?,
            Quantity::from(1_000_000_000),
            Quantity::from(1_000_000_000),
            ts,
            ts,
        )));
        engine.add_data(events, None, true, true)?;
        engine.run(None, None, None, end < bars.len())?;
        let cache = engine.kernel().cache.borrow();
        anyhow::ensure!(
            cache
                .positions_open(None, None, None, None, None)
                .is_empty(),
            "EOD residual position"
        );
        anyhow::ensure!(
            cache.orders_open(None, None, None, None, None).is_empty(),
            "EOD residual order"
        );
        let equity = cache
            .account_for_venue(&id.venue)
            .context("account unavailable")?
            .balance_total(Some(Currency::USD()))
            .context("balance unavailable")?
            .as_decimal();
        peak = peak.max(equity);
        daily.push(Daily {
            date: date(session.open)?,
            equity,
            return_value: (equity / before - Decimal::ONE)
                .to_f64()
                .context("return conversion")?,
            drawdown: (Decimal::ONE - equity / peak)
                .to_f64()
                .context("drawdown conversion")?,
        });
        before = equity;
        drop(cache);
        engine.clear_data();
        cursor = end;
    }
    let report = report
        .lock()
        .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
        .clone();
    anyhow::ensure!(
        report.errors.is_empty(),
        "strategy errors: {:?}",
        report.errors
    );
    let mut trades = Vec::new();
    let mut entry: Option<(u64, i32, Decimal, Decimal, Decimal)> = None;
    for fill in &report.fills {
        if let Some((timestamp, side, quantity, price, cost)) = entry.take() {
            anyhow::ensure!(
                fill.side == -side && fill.quantity == quantity,
                "unexpected partial/reversal fill in compatibility replay"
            );
            trades.push(Trade {
                entry_timestamp: timestamp,
                exit_timestamp: fill.timestamp,
                side,
                quantity,
                entry_price: price,
                exit_price: fill.fill_price,
                pnl: Decimal::from(side) * quantity * (fill.fill_price - price)
                    - cost
                    - fill.commission,
                cost: cost + fill.commission,
            });
        } else {
            entry = Some((
                fill.timestamp,
                fill.side,
                fill.quantity,
                fill.fill_price,
                fill.commission,
            ));
        }
    }
    anyhow::ensure!(entry.is_none(), "incomplete trade ledger");
    let parity = parity(bars, config, &report, &daily)?;
    let mut metrics = metrics(&daily, &trades, &report, config)?;
    metrics["symbol"] = serde_json::json!(config.symbol);
    exposure_metrics(bars, &report, config.initial_equity, &mut metrics)?;
    Ok(Outcome {
        metrics,
        daily,
        trades,
        report,
        parity,
    })
}

fn metrics(
    daily: &[Daily],
    trades: &[Trade],
    report: &IntradayMomentumReport,
    config: &Config,
) -> anyhow::Result<serde_json::Value> {
    let returns = daily.iter().map(|d| d.return_value).collect::<Vec<_>>();
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let vol =
        nautilus_trading::examples::strategies::intraday_momentum::reference::sample_std(&returns)
            .unwrap_or(0.0);
    let downside =
        (returns.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / returns.len() as f64).sqrt();
    let total = (daily.last().context("no daily results")?.equity / config.initial_equity
        - Decimal::ONE)
        .to_f64()
        .context("total return conversion")?;
    let annualized = (1.0 + total).powf(252.0 / returns.len() as f64) - 1.0;
    let drawdown = daily.iter().map(|d| d.drawdown).fold(0.0, f64::max);
    let profit = trades
        .iter()
        .filter(|t| t.pnl > Decimal::ZERO)
        .map(|t| t.pnl)
        .sum::<Decimal>();
    let loss = -trades
        .iter()
        .filter(|t| t.pnl < Decimal::ZERO)
        .map(|t| t.pnl)
        .sum::<Decimal>();
    let turnover = report
        .fills
        .iter()
        .map(|f| f.fill_price * f.quantity)
        .sum::<Decimal>();
    let cost = report.fills.iter().map(|f| f.commission).sum::<Decimal>();
    let exposure = trades
        .iter()
        .map(|t| t.quantity * t.entry_price)
        .max()
        .unwrap_or_default();
    let max_leverage = report
        .decisions
        .iter()
        .map(|d| d.leverage)
        .max()
        .unwrap_or_default();
    Ok(
        serde_json::json!({"total_return":total,"annualized_return":annualized,"annualized_volatility":vol*252_f64.sqrt(),"sharpe":if vol>0.0 {Some(mean/vol*252_f64.sqrt())} else {None},"sortino":if downside>0.0 {Some(mean/downside*252_f64.sqrt())} else {None},"max_drawdown":drawdown,"calmar":if drawdown>0.0 {Some(annualized/drawdown)} else {None},"win_rate":if trades.is_empty() {0.0} else {trades.iter().filter(|t|t.pnl>Decimal::ZERO).count() as f64/trades.len() as f64},"profit_factor":if loss>Decimal::ZERO {Some(profit/loss)} else {None},"average_trade":if trades.is_empty() {Decimal::ZERO} else {(profit-loss)/Decimal::from(trades.len() as u64)},"number_of_trades":trades.len(),"turnover_notional":turnover,"turnover":turnover/config.initial_equity,"transaction_costs":cost,"maximum_exposure":exposure,"maximum_leverage":max_leverage,"sessions":daily.len(),"final_equity":daily.last().map(|d|d.equity),"account_pnl":daily.last().map(|d|d.equity-config.initial_equity),"trade_pnl":profit-loss}),
    )
}

// Marks the already observed fills; this never generates a signal or simulated fill.
fn exposure_metrics(
    bars: &[(Session, MinuteBar)],
    report: &IntradayMomentumReport,
    initial: Decimal,
    metrics: &mut serde_json::Value,
) -> anyhow::Result<()> {
    let mut cash = initial;
    let mut quantity = Decimal::ZERO;
    let mut cursor = 0;
    let mut maximum_exposure = Decimal::ZERO;
    let mut maximum_leverage = Decimal::ZERO;
    let mut friction = Decimal::ZERO;
    for (_, bar) in bars {
        while let Some(fill) = report.fills.get(cursor) {
            if fill.timestamp > bar.timestamp {
                break;
            }
            let signed = Decimal::from(fill.side) * fill.quantity;
            cash -= signed * fill.fill_price + fill.commission;
            friction += Decimal::from(fill.side) * fill.quantity * (fill.fill_price - bar.open);
            quantity += signed;
            let equity = cash + quantity * fill.fill_price;
            let exposure = (quantity * fill.fill_price).abs();
            if equity > Decimal::ZERO {
                maximum_leverage = maximum_leverage.max(exposure / equity);
            }
            maximum_exposure = maximum_exposure.max(exposure);
            cursor += 1;
        }
        let equity = cash + quantity * bar.close;
        let exposure = (quantity * bar.close).abs();
        if equity > Decimal::ZERO {
            maximum_leverage = maximum_leverage.max(exposure / equity);
        }
        maximum_exposure = maximum_exposure.max(exposure);
    }
    anyhow::ensure!(
        cursor == report.fills.len() && quantity == Decimal::ZERO,
        "exposure report has unprocessed fills or overnight position"
    );
    metrics["maximum_entry_notional"] = metrics["maximum_entry_notional"].as_str().map_or_else(
        || metrics["maximum_exposure"].clone(),
        |v| serde_json::json!(v),
    );
    metrics["maximum_target_leverage"] = report
        .decisions
        .iter()
        .map(|d| d.leverage)
        .max()
        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
    metrics["maximum_exposure"] = serde_json::json!(maximum_exposure);
    let commission = report.fills.iter().map(|f| f.commission).sum::<Decimal>();
    metrics["cash_commissions"] = serde_json::json!(commission);
    metrics["execution_friction_cost"] = serde_json::json!(friction);
    metrics["transaction_costs"] = serde_json::json!(commission + friction);
    metrics["maximum_leverage"] = serde_json::json!(maximum_leverage);
    metrics["exposure_observation"] = serde_json::json!(
        "actual fills and completed minute closes; ledger equity before account-cent rounding; not intra-minute extrema"
    );
    Ok(())
}

pub(super) fn write(output: &Path, outcome: &Outcome) -> anyhow::Result<()> {
    fs::create_dir_all(output)?;
    fs::write(
        output.join("metrics.json"),
        serde_json::to_vec_pretty(&outcome.metrics)?,
    )?;
    fs::write(
        output.join("parity_report.json"),
        serde_json::to_vec_pretty(&outcome.parity)?,
    )?;
    fs::write(
        output.join("strategy_report.json"),
        serde_json::to_vec_pretty(&outcome.report)?,
    )?;
    let mut trades = fs::File::create(output.join("trades.csv"))?;
    writeln!(
        trades,
        "entry_timestamp,exit_timestamp,side,quantity,entry_price,exit_price,pnl,cost"
    )?;
    for t in &outcome.trades {
        writeln!(
            trades,
            "{},{},{},{},{},{},{},{}",
            t.entry_timestamp,
            t.exit_timestamp,
            t.side,
            t.quantity,
            t.entry_price,
            t.exit_price,
            t.pnl,
            t.cost
        )?;
    }
    for (name, column) in [
        ("daily_returns.csv", "return"),
        ("equity_curve.csv", "equity"),
        ("drawdown.csv", "drawdown"),
    ] {
        let mut file = fs::File::create(output.join(name))?;
        writeln!(file, "date,{column}")?;
        for d in &outcome.daily {
            let value = match column {
                "return" => d.return_value.to_string(),
                "equity" => d.equity.to_string(),
                _ => d.drawdown.to_string(),
            };
            writeln!(file, "{},{}", d.date, value)?;
        }
    }
    Ok(())
}

fn correlation(left: &[f64], right: &[f64]) -> Option<f64> {
    if left.len() != right.len() || left.len() < 2 {
        return None;
    }
    let a = left.iter().sum::<f64>() / left.len() as f64;
    let b = right.iter().sum::<f64>() / right.len() as f64;
    let denominator = (left.iter().map(|v| (v - a).powi(2)).sum::<f64>()
        * right.iter().map(|v| (v - b).powi(2)).sum::<f64>())
    .sqrt();
    (denominator > 0.0).then(|| {
        left.iter()
            .zip(right)
            .map(|(x, y)| (x - a) * (y - b))
            .sum::<f64>()
            / denominator
    })
}

/// Re-audits saved fills and decisions without submitting orders or re-evaluating the OOS engine.
pub(super) fn audit_existing(
    bars: &[(Session, MinuteBar)],
    config: &Config,
    output: &Path,
) -> anyhow::Result<()> {
    let report: IntradayMomentumReport =
        serde_json::from_slice(&fs::read(output.join("strategy_report.json"))?)?;
    let returns = fs::read_to_string(output.join("daily_returns.csv"))?;
    let equities = fs::read_to_string(output.join("equity_curve.csv"))?;
    let mut daily = Vec::new();
    for (r, e) in returns.lines().skip(1).zip(equities.lines().skip(1)) {
        let (date, value) = r.split_once(',').context("invalid return row")?;
        let (equity_date, equity) = e.split_once(',').context("invalid equity row")?;
        anyhow::ensure!(date == equity_date, "daily report dates differ");
        daily.push(Daily {
            date: date.into(),
            equity: equity.parse()?,
            return_value: value.parse()?,
            drawdown: 0.0,
        });
    }
    let value = parity(bars, config, &report, &daily)?;
    let mut metrics: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("metrics.json"))?)?;
    exposure_metrics(bars, &report, config.initial_equity, &mut metrics)?;
    fs::write(
        output.join("metrics.json"),
        serde_json::to_vec_pretty(&metrics)?,
    )?;
    fs::write(
        output.join("parity_report.json"),
        serde_json::to_vec_pretty(&value)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn parity(
    bars: &[(Session, MinuteBar)],
    config: &Config,
    report: &IntradayMomentumReport,
    daily: &[Daily],
) -> anyhow::Result<serde_json::Value> {
    let reference = reference_fills(bars, config)?;
    let mut divergences = Vec::new();
    let mut timing_side = 0;
    let mut prices = 0;
    let mut quantities = 0;
    let mut costs = 0;
    for (index, (expected, actual)) in reference.iter().zip(&report.fills).enumerate() {
        timing_side += usize::from(
            expected.timestamp == actual.signal_timestamp
                && expected.side == actual.side
                && actual.timestamp == expected.timestamp + 1,
        );
        prices += usize::from(expected.price == actual.fill_price);
        quantities += usize::from(expected.quantity == actual.quantity);
        costs += usize::from(expected.cost == actual.commission);
        if (
            expected.timestamp,
            expected.side,
            expected.quantity,
            expected.price,
            expected.cost,
        ) != (
            actual.signal_timestamp,
            actual.side,
            actual.quantity,
            actual.fill_price,
            actual.commission,
        ) && divergences.len() < 20
        {
            let reason = if config.max_daily_loss < Decimal::ONE || config.risk_per_trade.is_some()
            {
                "execution safety sizing/exits differ from the unconstrained signal-only oracle"
            } else if config.realistic {
                "configured spread/impact and subsequent capital path"
            } else if expected.timestamp == actual.signal_timestamp
                && expected.side == actual.side
                && expected.price == actual.fill_price
            {
                "unconstrained Notebook quantity versus execution buying-power cap/current-price sizing and subsequent capital path"
            } else {
                "unexpected signal/timing/price divergence"
            };
            divergences.push(serde_json::json!({"index":index,"timestamp":actual.timestamp,"reference":expected,"actual":actual,"reason":reason}));
        }
    }
    let mut model = ReferenceModel::new(config.model.clone())?;
    let mut decisions = 0;
    let mut feature_matches = 0;
    let mut signal_matches = 0;
    for (session, bar) in bars {
        let f = model.on_bar(*session, *bar)?;
        if !f.decision_time {
            continue;
        }
        if let Some(d) = report.decisions.get(decisions) {
            let same = f.timestamp == d.timestamp.as_u64();
            signal_matches += usize::from(same && f.target == d.target);
            feature_matches += usize::from(
                same && f.daily_open == d.open.as_decimal()
                    && bar.close == d.close.as_decimal()
                    && f.sigma == Some(d.sigma_open)
                    && f.upper_bound == Some(d.upper_bound)
                    && f.lower_bound == Some(d.lower_bound)
                    && f.anchored_vwap == Some(d.vwap)
                    && f.rvol == d.relative_volume
                    && (f.leverage - d.leverage).abs() < Decimal::new(1, 10),
            );
        }
        decisions += 1;
    }
    let mut cash = config.initial_equity;
    let mut before = cash;
    let mut ref_daily = Vec::new();
    let mut ref_equity = Vec::new();
    let mut next_fill = 0;
    for day in daily {
        while let Some(fill) = reference.get(next_fill) {
            if date(fill.timestamp)? > day.date {
                break;
            }
            cash -= Decimal::from(fill.side) * fill.quantity * fill.price + fill.cost;
            next_fill += 1;
        }
        ref_daily.push(
            (cash / before - Decimal::ONE)
                .to_f64()
                .context("reference daily return")?,
        );
        ref_equity.push(cash.to_f64().context("reference equity")?);
        before = cash;
    }
    let actual_daily = daily.iter().map(|d| d.return_value).collect::<Vec<_>>();
    let actual_equity = daily
        .iter()
        .map(|d| d.equity.to_f64().unwrap_or(f64::NAN))
        .collect::<Vec<_>>();
    let count = reference.len();
    let equal_count = count == report.fills.len();
    Ok(serde_json::json!({
        "reference":"causal fixed-share ledger; unconstrained Notebook quantity; production signal clock",
        "feature_rows":decisions,"feature_matching_rows":feature_matches,"feature_match":decisions==report.decisions.len() && feature_matches==decisions,
        "signal_matching_rows":signal_matches,"signal_match":decisions==report.decisions.len() && signal_matches==decisions,
        "reference_fill_count":count,"nautilus_fill_count":report.fills.len(),"reference_trade_count":count/2,"nautilus_trade_count":report.fills.len()/2,
        "position_direction_match":equal_count && timing_side==count,"entry_exit_timestamp_and_side_match":equal_count && timing_side==count,
        "entry_exit_price_match":equal_count && prices==count,"quantity_match":equal_count && quantities==count,"quantity_matching_fills":quantities,"cost_matching_fills":costs,
        "fill_match":equal_count && timing_side==count && prices==count && quantities==count && costs==count,
        "next_open_execution":equal_count && timing_side==count,
        "daily_return_correlation":correlation(&ref_daily,&actual_daily),"equity_correlation":correlation(&ref_equity,&actual_equity),
        "reference_final_equity":cash,"nautilus_final_equity":daily.last().map(|d|d.equity),"reference_account_cent_rounding":false,
        "divergences":divergences,"realistic_costs":config.realistic,
        "execution_safety_active":config.max_daily_loss < Decimal::ONE || config.risk_per_trade.is_some(),
        "fills_never_precede_signal":report.fills.iter().all(|f|f.timestamp>=f.signal_timestamp),
        "notebook_literal_pnl":"separate noncausal audit; never a production execution mode"
    }))
}

pub(super) struct Arguments {
    pub input: PathBuf,
    pub output: PathBuf,
    pub config: Config,
    pub start: Option<String>,
    pub end: Option<String>,
    pub audit_only: bool,
}

pub(super) fn arguments() -> anyhow::Result<Arguments> {
    let mut audit_only = false;
    let mut input = PathBuf::from("tests/data/intraday_momentum/spy_intraday_golden.csv");
    let mut output = PathBuf::from("reports/intraday");
    let mut config = Config::default();
    let mut symbol = None;
    let mut start = None;
    let mut end = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--audit-existing" {
            audit_only = true;
            continue;
        }
        let value = args
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--input" => input = PathBuf::from(value),
            "--output" => output = PathBuf::from(value),
            "--config" => config = serde_json::from_str(&fs::read_to_string(value)?)?,
            "--symbol" => symbol = Some(value),
            "--start" => {
                value.parse::<jiff::civil::Date>()?;
                start = Some(value);
            }
            "--end" => {
                value.parse::<jiff::civil::Date>()?;
                end = Some(value);
            }
            "--timing" => {
                config.model.timing = match value.as_str() {
                    "notebook" => Timing::NotebookLabel,
                    "production" => Timing::SessionClose,
                    _ => anyhow::bail!("timing must be notebook or production"),
                }
            }
            _ => anyhow::bail!("unknown option {flag}"),
        }
    }
    anyhow::ensure!(
        start.as_ref().zip(end.as_ref()).is_none_or(|(s, e)| s <= e),
        "start exceeds end"
    );
    if let Some(symbol) = symbol {
        config.symbol = symbol;
    }
    Ok(Arguments {
        input,
        output,
        config,
        start,
        end,
        audit_only,
    })
}
