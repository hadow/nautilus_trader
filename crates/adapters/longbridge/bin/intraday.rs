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

//! Notebook strategy runner. Only an explicitly confirmed live mode installs broker execution.

#[path = "../examples/intraday_calendar.rs"]
mod intraday_calendar;

use std::{fs, path::PathBuf, time::Duration};

use anyhow::Context;
use jiff::Timestamp;
use longbridge::quote::{AdjustType, Period, QuoteContext, TradeSessions};
use nautilus_common::{enums::Environment, live::get_runtime};
use nautilus_core::UnixNanos;
use nautilus_execution::models::fee::{FeeModelAny, PerContractFeeModel};
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{parse::parse_completed_minute_bar, rate_limit::history_api_call_with_retry},
};
use nautilus_model::{
    data::{BarType, bar_vwap::BarWithVwap},
    enums::{AccountType, OmsType},
    identifiers::{AccountId, InstrumentId, StrategyId, TraderId},
    types::{Currency, Money},
};
use nautilus_sandbox::{SandboxExecutionClientConfig, SandboxExecutionClientFactory};
use nautilus_trading::examples::strategies::{
    IntradayMomentumConfig, IntradayMomentumSession, IntradayMomentumStrategy,
};
use rust_decimal::Decimal;
use serde::Deserialize;

const MINUTE: u64 = 60_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    DryRun,
    Paper,
    Live,
}
impl Mode {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "dry-run" => Ok(Self::DryRun),
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            _ => anyhow::bail!("mode must be dry-run, paper or live"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    account_id: String,
    cache: PathBuf,
    report: PathBuf,
    data: LongbridgeDataClientConfig,
    strategy: IntradayMomentumConfig,
    paper_cost_per_share: Decimal,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            paper_cost_per_share: Decimal::new(45, 4),
            account_id: "LONGBRIDGE-001".into(),
            cache: "test_data/local/intraday_momentum/warmup".into(),
            report: "reports/intraday-live.json".into(),
            data: LongbridgeDataClientConfig {
                instrument_price_increments: [(
                    "SPY.US.LONGBRIDGE".to_string(),
                    "0.01".to_string(),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            strategy: IntradayMomentumConfig {
                instrument_id: InstrumentId::from("SPY.US.LONGBRIDGE"),
                flatten_before_close_minutes: 1,
                ..Default::default()
            },
        }
    }
}

async fn history(
    context: &QuoteContext,
    config: &Config,
    session: IntradayMomentumSession,
    now: UnixNanos,
) -> anyhow::Result<Vec<BarWithVwap>> {
    let local = session
        .open
        .to_datetime_utc()
        .to_zoned(nautilus_core::datetime::get_timezone("America/New_York")?)
        .date();
    let folder = config
        .cache
        .join(config.strategy.instrument_id.symbol.as_str());
    fs::create_dir_all(&folder)?;
    let path = folder.join(format!("{local}.json"));
    if session.close < now && path.exists() {
        return Ok(serde_json::from_slice(&fs::read(path)?)?);
    }
    let date = time::Date::from_calendar_date(
        i32::from(local.year()),
        time::Month::try_from(local.month() as u8)?,
        local.day() as u8,
    )?;
    let rows = history_api_call_with_retry(|| {
        context.history_candlesticks_by_date(
            config.strategy.instrument_id.symbol.as_str(),
            Period::OneMinute,
            AdjustType::NoAdjust,
            Some(date),
            Some(date),
            TradeSessions::Intraday,
        )
    })
    .await?;
    let kind: BarType =
        format!("{}-1-MINUTE-LAST-EXTERNAL", config.strategy.instrument_id).parse()?;
    let mut bars = Vec::new();
    for row in rows {
        let end = row.timestamp.unix_timestamp_nanos() + i128::from(MINUTE);
        if end > i128::from(now.as_u64()) {
            continue;
        }
        let value = parse_completed_minute_bar(kind, row, now)?;
        if value.bar.ts_event <= session.open || value.bar.ts_event > session.close {
            continue;
        }
        bars.push(value);
    }
    bars.sort_unstable_by_key(|b| b.bar.ts_event);
    if session.close < now {
        let expected = (session.close.as_u64() - session.open.as_u64()) / MINUTE;
        anyhow::ensure!(
            bars.len() as u64 == expected
                && bars.iter().enumerate().all(|(i, b)| b.bar.ts_event.as_u64()
                    == session.open.as_u64() + (i as u64 + 1) * MINUTE),
            "incomplete history for {local}"
        );
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, serde_json::to_vec(&bars)?)?;
        fs::rename(temporary, path)?;
    }
    Ok(bars)
}

fn check_mode(mode: Mode, confirmation: Option<&str>, config: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.strategy.instrument_id.symbol.as_str() == "SPY.US"
            && config.strategy.instrument_id.venue.as_str() == "LONGBRIDGE",
        "Notebook runner is limited to SPY.US.LONGBRIDGE"
    );
    anyhow::ensure!(
        config.strategy.flatten_before_close_minutes > 0,
        "must flatten before close"
    );
    anyhow::ensure!(
        !config.strategy.allow_ohlc_vwap_approximation,
        "provider VWAP required"
    );
    anyhow::ensure!(
        !config.strategy.defer_to_next_quote,
        "live uses actual post-close market execution"
    );
    if mode == Mode::Live {
        anyhow::ensure!(
            confirmation == Some(config.account_id.as_str()),
            "LIVE TRADING ENABLED requires --confirm-live {} after reviewing symbol SPY and leverage {}",
            config.account_id,
            config.strategy.max_leverage
        );
    } else {
        anyhow::ensure!(
            confirmation.is_none(),
            "live confirmation supplied for a non-live mode"
        );
    }
    Ok(())
}

async fn run(config: Config, mode: Mode, duration: Option<u64>) -> anyhow::Result<()> {
    config.data.validate()?;
    let now = Timestamp::now();
    let now_ns: UnixNanos = now.into();
    let (context, mut receiver) = QuoteContext::new(config.data.sdk_config().await?);
    let required = config
        .strategy
        .lookback_days
        .max(config.strategy.volume_lookback_days)
        .max(config.strategy.volatility_lookback_days + 1);
    let sessions =
        intraday_calendar::calendar(&context, now, i64::try_from(required * 2 + 30)?).await?;
    let current = sessions
        .iter()
        .copied()
        .find(|s| s.close > now_ns)
        .context("no upcoming session")?;
    let completed = sessions
        .iter()
        .copied()
        .filter(|s| s.close < now_ns)
        .collect::<Vec<_>>();
    anyhow::ensure!(completed.len() >= required, "insufficient calendar warmup");
    let mut history_bars = Vec::new();
    for session in &completed[completed.len() - required..] {
        history_bars.extend(history(&context, &config, *session, now_ns).await?);
    }
    if current.open < now_ns {
        history_bars.extend(history(&context, &config, current, now_ns).await?);
    }
    let mut strategy_config = config.strategy.clone();
    strategy_config.sessions = sessions;
    strategy_config.dry_run = mode == Mode::DryRun;
    strategy_config.base.strategy_id = Some(StrategyId::from("INTRADAY-001"));
    strategy_config.base.oms_type = Some(OmsType::Netting);
    strategy_config.base.external_order_claims = Some(vec![strategy_config.instrument_id]);
    strategy_config.base.market_exit_reduce_only = false;
    strategy_config.retain_features = false;
    let mut strategy = IntradayMomentumStrategy::new(strategy_config)?;
    strategy.warmup_with_vwap(&history_bars)?;
    let report = strategy.report_handle();
    drop(context);
    tokio::time::timeout(Duration::from_secs(10), async {
        while receiver.recv().await.is_some() {}
    })
    .await
    .context("warmup context did not close")?;
    let trader_id = TraderId::from("INTRADAY-001");
    let account_id = AccountId::from(config.account_id.as_str());
    let venue = config.strategy.instrument_id.venue;
    let mut builder = LiveNode::builder(
        trader_id,
        if mode == Mode::Live {
            Environment::Live
        } else {
            Environment::Sandbox
        },
    )?
    .with_name("INTRADAY")
    .with_load_state(false)
    .with_save_state(false)
    .with_reconciliation(mode == Mode::Live)
    .with_exec_engine_config(LiveExecEngineConfig {
        reconciliation_instrument_ids: Some(vec![config.strategy.instrument_id.to_string()]),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(30.0),
        ..Default::default()
    })
    .with_risk_engine_config(LiveRiskEngineConfig {
        bypass: false,
        max_notional_per_order: [(
            config.strategy.instrument_id.to_string(),
            config.strategy.max_order_notional.to_string(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    })
    .with_delay_post_stop_secs(10)
    .add_data_client(
        None,
        Box::new(LongbridgeDataClientFactory::new()),
        Box::new(config.data.clone()),
    )?;
    match mode {
        Mode::Live => {
            println!(
                "LIVE TRADING ENABLED ACCOUNT={} SYMBOL=SPY MAX LEVERAGE={}",
                config.account_id, config.strategy.max_leverage
            );
            builder = builder.add_exec_client(
                None,
                Box::new(LongbridgeExecutionClientFactory::new(trader_id, account_id)),
                Box::new(LongbridgeExecClientConfig {
                    account_type: AccountType::Margin,
                    papertrading: false,
                    outside_rth: false,
                    ..Default::default()
                }),
            )?;
        }
        Mode::Paper => {
            builder = builder.add_simulated_exec_client(
                Some(venue.to_string()),
                Box::new(SandboxExecutionClientFactory::new()),
                Box::new(SandboxExecutionClientConfig {
                    trader_id,
                    account_id,
                    venue,
                    starting_balances: vec![Money::from_decimal(
                        config.strategy.dry_run_equity,
                        Currency::USD(),
                    )?],
                    base_currency: Some(Currency::USD()),
                    oms_type: OmsType::Netting,
                    account_type: AccountType::Margin,
                    default_leverage: config.strategy.max_leverage,
                    fee_model: Some(FeeModelAny::PerContract(PerContractFeeModel::from_rate(
                        config.paper_cost_per_share,
                        Currency::USD(),
                    )?)),
                    bar_execution: false,
                    trade_execution: false,
                    ..Default::default()
                }),
            )?;
        }
        Mode::DryRun => {}
    }
    let mut node = builder.build()?;
    node.add_strategy(strategy)?;
    let stop = node.handle();
    let seconds = duration.unwrap_or_else(|| {
        (current.close.as_u64().saturating_sub(now_ns.as_u64()) / 1_000_000_000) + 5
    });
    get_runtime().spawn(async move {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        stop.stop();
    });
    let result = node.run().await;
    if let Some(parent) = config.report.parent() {
        fs::create_dir_all(parent)?;
    }
    let cache = node.kernel().cache.borrow();
    let residual = !cache
        .positions_open(None, Some(&config.strategy.instrument_id), None, None, None)
        .is_empty()
        || !cache
            .orders_open(None, Some(&config.strategy.instrument_id), None, None, None)
            .is_empty();
    drop(cache);
    let mut report = report
        .lock()
        .map_err(|_| anyhow::anyhow!("report lock poisoned"))?;
    if residual {
        report.errors.push(
            "ALERT: shutdown left an open position/order; manual reconciliation required".into(),
        );
    }
    fs::write(&config.report, serde_json::to_vec_pretty(&*report)?)?;
    result?;
    anyhow::ensure!(
        report.errors.is_empty(),
        "strategy halted: {:?}",
        report.errors
    );
    println!(
        "mode={mode:?} signals={} fills={} report={}",
        report.decisions.len(),
        report.fills.len(),
        config.report.display()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut mode = Mode::DryRun;
    let mut path = None;
    let mut confirmation = None;
    let mut duration = None;
    let mut validate = false;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--validate" {
            validate = true;
            continue;
        }
        let value = args
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--mode" => mode = Mode::parse(&value)?,
            "--config" => path = Some(PathBuf::from(value)),
            "--confirm-live" => confirmation = Some(value),
            "--duration-seconds" => duration = Some(value.parse::<u64>()?),
            _ => anyhow::bail!("unknown option {flag}"),
        }
    }
    let config = match path {
        Some(path) => serde_json::from_slice(&fs::read(path)?)?,
        None => Config::default(),
    };
    check_mode(mode, confirmation.as_deref(), &config)?;
    if validate {
        println!(
            "Mode {mode:?} validated; broker execution={}",
            mode == Mode::Live
        );
        return Ok(());
    }
    nautilus_common::logging::ensure_logging_initialized();
    run(config, mode, duration).await
}
