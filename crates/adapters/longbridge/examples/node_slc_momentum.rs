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

//! Read-only API probe and paper-only LiveNode for the shared intraday strategy.
//!
//! 中文说明：`--dry-run` 连接实时行情但不发策略订单；`--paper` 向 Longbridge 券商
//! 模拟账户发单。节点固定使用 `Environment::Sandbox` 和 `papertrading: true`，没有真实资金入口。

use std::{collections::HashMap, env, fs, path::Path};

use anyhow::Context;
use jiff::{Timestamp, civil::Time};
use longbridge::{
    Market,
    quote::{AdjustType, Period, QuoteContext, TradeSessions},
};
use nautilus_common::enums::Environment;
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{
        parse::parse_bar_with_price_precision,
        rate_limit::{history_api_call_with_retry, quote_api_call_with_retry},
    },
};
use nautilus_model::{
    data::{Bar, BarType},
    enums::AccountType,
    identifiers::{AccountId, StrategyId, TraderId},
    types::Price,
};
use nautilus_trading::examples::strategies::slc_momentum::{
    Session, SlcMomentumConfig, SlcMomentumStrategy,
};
use serde::{Deserialize, Serialize};

mod slc_momentum_history;
mod slc_momentum_market;
use time::{Date, Month};

const MINUTE: u64 = 60_000_000_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct App {
    instrument_price_increments: HashMap<String, String>,
    strategy: SlcMomentumConfig,
    warmup_minute_batches: usize,
    #[serde(default)]
    scanner: Option<slc_momentum_market::ScannerConfig>,
}

fn sdk_date(timestamp: Timestamp) -> anyhow::Result<Date> {
    let date = timestamp.to_zoned(get_timezone("America/New_York")?).date();
    Ok(Date::from_calendar_date(
        i32::from(date.year()),
        Month::try_from(date.month() as u8)?,
        date.day() as u8,
    )?)
}

async fn calendar(context: &QuoteContext, now: Timestamp) -> anyhow::Result<Vec<Session>> {
    let today = sdk_date(now)?;
    let end = today + time::Duration::days(7);
    let start = today - time::Duration::days(100);
    let timezone = get_timezone("America/New_York")?;
    let mut sessions = Vec::new();
    let mut cursor = start;
    let mut dates = std::collections::BTreeSet::new();
    let mut half_days = std::collections::BTreeSet::new();
    while cursor <= end {
        let boundary = (cursor + time::Duration::days(27)).min(end);
        let days = quote_api_call_with_retry(|| context.trading_days(Market::US, cursor, boundary))
            .await?;
        dates.extend(days.trading_days);
        dates.extend(days.half_trading_days.iter().copied());
        half_days.extend(days.half_trading_days);
        cursor = boundary + time::Duration::days(1);
    }
    for day in dates {
        let date = jiff::civil::Date::new(
            day.year() as i16,
            u8::from(day.month()) as i8,
            day.day() as i8,
        )?;
        let open = date
            .to_datetime(Time::new(9, 30, 0, 0)?)
            .to_zoned(timezone.clone())?
            .timestamp();
        let hour = if half_days.contains(&day) { 13 } else { 16 };
        let close = date
            .to_datetime(Time::new(hour, 0, 0, 0)?)
            .to_zoned(timezone.clone())?
            .timestamp();
        sessions.push(Session {
            open: open.into(),
            close: close.into(),
        });
    }
    sessions.sort_by_key(|s| s.open);
    Ok(sessions)
}

async fn probe(output: &Path) -> anyhow::Result<()> {
    let config = LongbridgeDataClientConfig::default();
    let sdk = config.sdk_config().await?;
    let (context, _receiver) = QuoteContext::new(sdk);
    let now = Timestamp::now();
    let sessions = calendar(&context, now).await?;
    let bars = quote_api_call_with_retry(|| {
        context.candlesticks(
            "SPY.US",
            Period::OneMinute,
            20,
            AdjustType::NoAdjust,
            TradeSessions::Intraday,
        )
    })
    .await?;
    let daily = quote_api_call_with_retry(|| {
        context.candlesticks(
            "SPY.US",
            Period::Day,
            40,
            AdjustType::NoAdjust,
            TradeSessions::Intraday,
        )
    })
    .await?;
    let record = serde_json::json!({
        "observed_at": now.to_string(), "symbol": "SPY.US", "orders_submitted": 0,
        "sessions": sessions, "minute_bars": bars.len(), "daily_bars": daily.len(),
        "first_minute_start": bars.first().map(|b| b.timestamp.to_string()),
        "last_minute_start": bars.last().map(|b| b.timestamp.to_string()),
        "valid_positive_ohlc": !bars.is_empty() && bars.iter().all(|b| b.low > rust_decimal::Decimal::ZERO && b.high >= b.close && b.high >= b.open && b.low <= b.close && b.low <= b.open),
        "request_policy": "Six sequential bounded calls, calendar chunks <=28 days, via adapter quote_api_call_with_retry and SDK endpoint throttles; one QuoteContext; no execution client",
    });
    fs::write(output, serde_json::to_vec_pretty(&record)?)?;
    println!(
        "Read-only probe: {} minute bars, {} daily bars; {}",
        bars.len(),
        daily.len(),
        output.display()
    );
    Ok(())
}

async fn warmup(
    context: &QuoteContext,
    config: &App,
    ids: &[nautilus_model::identifiers::InstrumentId],
    daily: bool,
    minute_batches: usize,
) -> anyhow::Result<Vec<Bar>> {
    let mut result = Vec::new();
    let now: UnixNanos = Timestamp::now().into();
    for id in ids {
        let tick: Price = config
            .instrument_price_increments
            .get(&id.to_string())
            .context("missing tick")?
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid tick: {e}"))?;
        if daily {
            let daily_type: BarType = format!("{id}-1-DAY-LAST-EXTERNAL").parse()?;
            let rows = quote_api_call_with_retry(|| {
                context.candlesticks(
                    id.symbol.as_str(),
                    Period::Day,
                    70,
                    AdjustType::NoAdjust,
                    TradeSessions::Intraday,
                )
            })
            .await?;
            let observed: UnixNanos = Timestamp::now().into();
            for row in rows {
                let raw =
                    parse_bar_with_price_precision(daily_type, row, observed, tick.precision)?;
                let date = raw
                    .ts_event
                    .to_datetime_utc()
                    .to_zoned(get_timezone("America/New_York")?)
                    .date();
                let session = config.strategy.sessions.iter().find(|s| {
                    s.open
                        .to_datetime_utc()
                        .to_zoned(get_timezone("America/New_York").expect("valid timezone"))
                        .date()
                        == date
                });
                if let Some(session) = session.filter(|s| s.close < now) {
                    result.push(Bar::new(
                        raw.bar_type,
                        raw.open,
                        raw.high,
                        raw.low,
                        raw.close,
                        raw.volume,
                        session.close,
                        observed,
                    ));
                }
            }
        }
        let minute_type: BarType = format!("{id}-1-MINUTE-LAST-EXTERNAL").parse()?;
        let mut before = None;
        for _ in 0..minute_batches {
            let rows = history_api_call_with_retry(|| {
                context.history_candlesticks_by_offset(
                    id.symbol.as_str(),
                    Period::OneMinute,
                    AdjustType::NoAdjust,
                    false,
                    before,
                    1000,
                    TradeSessions::Intraday,
                )
            })
            .await?;
            if rows.is_empty() {
                break;
            }
            let first = rows
                .iter()
                .map(|b| b.timestamp)
                .min()
                .context("missing oldest bar")?;
            let local = Timestamp::from_nanosecond(first.unix_timestamp_nanos())?
                .to_zoned(get_timezone("America/New_York")?)
                .checked_sub(jiff::SignedDuration::from_mins(1))?;
            before = Some(
                Date::from_calendar_date(
                    i32::from(local.year()),
                    Month::try_from(local.month() as u8)?,
                    local.day() as u8,
                )?
                .with_time(time::Time::from_hms(
                    local.hour() as u8,
                    local.minute() as u8,
                    local.second() as u8,
                )?),
            );
            let observed: UnixNanos = Timestamp::now().into();
            for row in rows {
                let raw =
                    parse_bar_with_price_precision(minute_type, row, observed, tick.precision)?;
                let end = UnixNanos::from(raw.ts_event.as_u64() + MINUTE);
                if end < now
                    && config
                        .strategy
                        .sessions
                        .iter()
                        .any(|s| s.open < end && end <= s.close)
                {
                    result.push(Bar::new(
                        raw.bar_type,
                        raw.open,
                        raw.high,
                        raw.low,
                        raw.close,
                        raw.volume,
                        end,
                        observed,
                    ));
                }
            }
        }
    }
    result.sort_by_key(|b| (b.ts_event, b.bar_type));
    result.dedup_by_key(|b| (b.ts_event, b.bar_type));
    Ok(result)
}

async fn paper(path: &Path, submit: bool) -> anyhow::Result<()> {
    let mut app: App = serde_json::from_str(&fs::read_to_string(path)?)?;
    let ids = app.strategy.instrument_ids();
    if app.strategy.market.is_some() {
        anyhow::ensure!(
            app.scanner.is_some()
                && app.strategy.ablation
                    == nautilus_trading::examples::strategies::slc_momentum::Ablation::F
                && app.strategy.slc.ltf_minutes == 5,
            "full-market paper requires scanner, full pipeline F and 5-minute SLC"
        );
    }
    anyhow::ensure!(
        app.strategy
            .universe
            .iter()
            .all(|m| m.market_cap > rust_decimal::Decimal::ZERO),
        "replace template market caps with timestamped security-master observations before connecting"
    );
    anyhow::ensure!(
        (app.strategy.market.is_some() || ids.len() <= 20)
            && (1..=8).contains(&app.warmup_minute_batches),
        "this bounded paper runner supports at most 20 symbols and 1..=8 warmup batches"
    );
    anyhow::ensure!(
        ids.iter().all(|id| id.venue.as_str() == "LONGBRIDGE"
            && app
                .instrument_price_increments
                .contains_key(&id.to_string())),
        "Longbridge IDs and exact price increments are required"
    );
    let data = LongbridgeDataClientConfig {
        instrument_price_increments: app.instrument_price_increments.clone(),
        ..Default::default()
    };
    {
        let (context, mut receiver) = QuoteContext::new(data.sdk_config().await?);
        app.strategy.sessions = calendar(&context, Timestamp::now()).await?;
        let start: UnixNanos = Timestamp::now().into();
        let current = app
            .strategy
            .sessions
            .iter()
            .find(|s| s.open <= start && start < s.close)
            .copied()
            .context("paper runner requires the current regular US session")?;
        if let Some(market) = &app.strategy.market {
            let active = app
                .strategy
                .universe
                .iter()
                .filter(|m| {
                    m.known_at <= start
                        && m.effective_from <= start
                        && current.close <= m.effective_until
                })
                .count();
            anyhow::ensure!(
                active == market.universe_size,
                "dated universe is expired or incomplete; run --prepare-market for this session"
            );
        }
        app.strategy.trading_start = UnixNanos::from(start.as_u64() + 1_000_000_000);
        app.strategy.trading_end = current.close;
        app.strategy.longbridge = true;
        app.strategy.dry_run = !submit;
        app.strategy.base.strategy_id = Some(StrategyId::from("SLC-MOMENTUM-001"));
        app.strategy.base.order_id_tag = Some("804".to_string());
        app.strategy.validate()?;
        let history = if app.strategy.market.is_some() {
            slc_momentum_market::daily_history(&context, &app).await?
        } else {
            warmup(&context, &app, &ids, true, app.warmup_minute_batches).await?
        };
        app.strategy.trading_start =
            UnixNanos::from(UnixNanos::from(Timestamp::now()).as_u64() + 1_000_000_000);
        let mut strategy = SlcMomentumStrategy::new(app.strategy.clone())?;
        let report = strategy.report_handle();
        strategy.warmup(history.clone())?;
        drop(context);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while receiver.recv().await.is_some() {}
        })
        .await
        .context("warmup quote connection did not shut down; refusing a second context")?;
        let (context_sender, context_receiver) = tokio::sync::watch::channel(None);
        let trader_id = TraderId::from("SLC-MOMENTUM-001");
        let mut node = LiveNode::builder(trader_id, Environment::Sandbox)?
            .with_name("SLC-MOMENTUM-PAPER".to_string())
            .with_load_state(false)
            .with_save_state(false)
            .with_reconciliation(true)
            .with_exec_engine_config(LiveExecEngineConfig {
                reconciliation_lookback_mins: Some(60 * 24),
                open_check_interval_secs: Some(10.0),
                position_check_interval_secs: Some(30.0),
                ..Default::default()
            })
            .with_risk_engine_config(LiveRiskEngineConfig {
                bypass: false,
                max_order_submit_rate: "5/00:00:01".to_string(),
                max_order_modify_rate: "5/00:00:01".to_string(),
                max_notional_per_order: ids
                    .iter()
                    .map(|id| {
                        (
                            id.to_string(),
                            app.strategy.risk.max_position_value.to_string(),
                        )
                    })
                    .collect(),
                ..Default::default()
            })
            .add_data_client(
                None,
                Box::new(LongbridgeDataClientFactory::new().with_quote_context(context_sender)),
                Box::new(data),
            )?
            .add_exec_client(
                None,
                Box::new(LongbridgeExecutionClientFactory::new(
                    trader_id,
                    AccountId::from("LONGBRIDGE-001"),
                )),
                Box::new(LongbridgeExecClientConfig {
                    papertrading: true,
                    account_type: AccountType::Margin,
                    outside_rth: false,
                    ..Default::default()
                }),
            )?
            .build()?;
        node.add_strategy(strategy)?;
        let worker = if app.strategy.market.is_some() {
            let report = report.clone();
            let sender = nautilus_common::live::runner::get_data_event_sender();
            let app = app.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = slc_momentum_market::collect(
                    app,
                    context_receiver,
                    history,
                    report.clone(),
                    sender,
                )
                .await
                {
                    log::error!("Full-market collector stopped: {e:#}");
                    if let Ok(mut r) = report.lock() {
                        r.errors.push(format!("Full-market collector: {e:#}"));
                    }
                }
            }))
        } else {
            None
        };
        let result = node.run().await;
        if let Some(worker) = worker {
            worker.abort();
        }
        result?;
        let report_path =
            path.with_extension(format!("{}.report.json", Timestamp::now().as_second()));
        fs::write(
            &report_path,
            serde_json::to_vec_pretty(
                &*report
                    .lock()
                    .map_err(|_| anyhow::anyhow!("report lock poisoned"))?,
            )?,
        )?;
        println!("Paper session report: {}", report_path.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nautilus_common::logging::ensure_logging_initialized();
    let args = env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [mode, app, root, start, end] if mode == "--history-fill-ranking" => {
            slc_momentum_history::fill_ranking(
                Path::new(app),
                Path::new(root),
                start.parse()?,
                end.parse()?,
            )
            .await
        }
        [mode, app, root, start] if mode == "--history-fill-ranking-cli" => {
            slc_momentum_history::fill_ranking_cli(Path::new(app), Path::new(root), start.parse()?)
                .await
        }
        [mode, app, root, start, end] if mode == "--history-plan-exploratory" => {
            slc_momentum_history::plan(
                Path::new(app),
                Path::new(root),
                start.parse()?,
                end.parse()?,
            )
        }
        [mode, root] if mode == "--history-minutes" => {
            slc_momentum_history::minutes(Path::new(root)).await
        }
        [mode, root] if mode == "--history-minutes-cli" => {
            slc_momentum_history::minutes_cli(Path::new(root))
        }
        [mode, template, output] if mode == "--prepare-market" => {
            slc_momentum_market::prepare(Path::new(template), Path::new(output)).await
        }
        [mode, path] if mode == "--prefetch-market" => {
            slc_momentum_market::prefetch(Path::new(path)).await
        }
        [mode, path] if mode == "--probe" => probe(Path::new(path)).await,
        [mode, path] if mode == "--dry-run" => paper(Path::new(path), false).await,
        [mode, path] if mode == "--paper" => paper(Path::new(path), true).await,
        _ => anyhow::bail!(
            "usage: cargo run -p nautilus-longbridge --features examples --example longbridge-slc-momentum -- --prepare-market TEMPLATE.json OUTPUT.json | --prefetch-market INPUT.json | --probe OUTPUT.json | --dry-run INPUT.json | --paper INPUT.json | --history-fill-ranking APP ROOT START END | --history-fill-ranking-cli APP ROOT START | --history-plan-exploratory APP ROOT START END | --history-minutes ROOT | --history-minutes-cli ROOT"
        ),
    }
}
