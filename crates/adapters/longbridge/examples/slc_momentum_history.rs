// Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
// Licensed under the GNU Lesser General Public License Version 3.0.

//! Read-only, resumable history collection for explicit exploratory historical replays.
//!
//! 中文说明：先按交易日冻结排名计划，再仅下载候选股和基准的分钟线；分页文件与完成标记
//! 支持断点续传。该流程只读，不会创建订单。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context;
use futures_util::{StreamExt, stream};
use jiff::{Timestamp, civil::Date};
use longbridge::quote::{AdjustType, Candlestick, Period, QuoteContext, TradeSessions};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, common::rate_limit::history_api_call_with_retry,
};
use nautilus_model::{
    data::Bar,
    identifiers::InstrumentId,
    types::{Price, Quantity},
};
use nautilus_trading::examples::strategies::slc_momentum::{
    CrossSectionalMomentumRanker, MarketObservation, Session, SlcMomentumConfig, TradeSide,
    UniverseSelection,
};
use serde::{Deserialize, Serialize};

use super::{App, calendar};

#[derive(Serialize, Deserialize)]
struct HistoryPlan {
    provenance: String,
    starting_equity: rust_decimal::Decimal,
    spread_bps: rust_decimal::Decimal,
    opening_spread_multiplier: rust_decimal::Decimal,
    quote_depth: nautilus_model::types::Quantity,
    slippage_probability: f64,
    daily_cache_dir: std::path::PathBuf,
    fetched_at: UnixNanos,
    strategy: SlcMomentumConfig,
    price_increments: BTreeMap<InstrumentId, Price>,
    selections: Vec<UniverseSelection>,
    ranges: BTreeMap<InstrumentId, Session>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct CliCandlestick {
    close: String,
    high: String,
    low: String,
    open: String,
    time: String,
    volume: String,
}

#[derive(Clone, Debug, PartialEq)]
struct CliMinute {
    timestamp: i64,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
}

fn disk_guard(path: &Path) -> anyhow::Result<()> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()?;
    anyhow::ensure!(output.status.success(), "cannot inspect free disk space");
    let text = String::from_utf8(output.stdout)?;
    let free: u64 = text
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(3))
        .context("invalid df output")?
        .parse()?;
    anyhow::ensure!(
        free >= 3 * 1024 * 1024,
        "history collection stopped: less than 3 GiB free disk"
    );
    Ok(())
}

fn write_rows(path: &Path, rows: &[Candlestick]) -> anyhow::Result<()> {
    let temporary = path.with_extension("partial");
    let mut output = std::io::BufWriter::new(fs::File::create(&temporary)?);
    for row in rows {
        writeln!(
            output,
            "{},{},{},{},{},{}",
            row.timestamp.unix_timestamp(),
            row.open,
            row.high,
            row.low,
            row.close,
            row.volume
        )?;
    }
    output.flush()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn write_cli_rows(path: &Path, rows: &[CliMinute]) -> anyhow::Result<()> {
    let temporary = path.with_extension("partial");
    let mut output = std::io::BufWriter::new(fs::File::create(&temporary)?);
    for row in rows {
        writeln!(
            output,
            "{},{},{},{},{},{}",
            row.timestamp, row.open, row.high, row.low, row.close, row.volume
        )?;
    }
    output.flush()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn parse_cli_minutes(output: &[u8], sessions: &[Session]) -> anyhow::Result<Vec<CliMinute>> {
    let mut rows = BTreeMap::new();
    for row in serde_json::from_slice::<Vec<CliCandlestick>>(output)? {
        let timestamp = row.time.parse::<Timestamp>()?;
        let event: UnixNanos = timestamp.into();
        anyhow::ensure!(
            sessions
                .iter()
                .any(|session| session.open <= event && event < session.close),
            "Longbridge CLI returned an out-of-range minute at {timestamp}"
        );
        for (field, value) in [
            ("open", &row.open),
            ("high", &row.high),
            ("low", &row.low),
            ("close", &row.close),
        ] {
            value
                .parse::<rust_decimal::Decimal>()
                .with_context(|| format!("invalid CLI {field} at {timestamp}"))?;
        }
        row.volume
            .parse::<u64>()
            .with_context(|| format!("invalid CLI volume at {timestamp}"))?;
        let minute = CliMinute {
            timestamp: timestamp.as_second(),
            open: row.open,
            high: row.high,
            low: row.low,
            close: row.close,
            volume: row.volume,
        };
        if let Some(previous) = rows.insert(minute.timestamp, minute.clone()) {
            anyhow::ensure!(previous == minute, "conflicting CLI minute at {timestamp}");
        }
    }
    Ok(rows.into_values().collect())
}

fn existing_minute_seconds(directory: &Path) -> anyhow::Result<BTreeSet<i64>> {
    let mut seconds = BTreeSet::new();
    if !directory.exists() {
        return Ok(seconds);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "csv") {
            continue;
        }
        for line in fs::read_to_string(path)?.lines() {
            let timestamp = line
                .split(',')
                .next()
                .context("invalid minute CSV")?
                .parse()?;
            seconds.insert(timestamp);
        }
    }
    Ok(seconds)
}

fn session_minute_count(seconds: &BTreeSet<i64>, session: Session) -> usize {
    let open = (session.open.as_u64() / 1_000_000_000) as i64;
    let close = (session.close.as_u64() / 1_000_000_000) as i64;
    seconds.range(open..close).count()
}

fn cli_history(symbol: &str, period: &str, start: Date, end: Date) -> anyhow::Result<Vec<u8>> {
    let start = start.to_string();
    let end = end.to_string();
    let output = std::process::Command::new("longbridge")
        .args([
            "kline",
            "history",
            symbol,
            "--period",
            period,
            "--start",
            &start,
            "--end",
            &end,
            "--session",
            "intraday",
            "--format",
            "json",
        ])
        .output()
        .with_context(
            || "failed to execute Longbridge CLI; install and authenticate `longbridge`",
        )?;
    anyhow::ensure!(
        output.status.success(),
        "Longbridge CLI failed for {symbol} {start}..{end}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn ranking_warmup_sessions(
    app: &App,
    sessions: &[Session],
    start: Date,
) -> anyhow::Result<Vec<Session>> {
    let zone = get_timezone("America/New_York")?;
    let start_stamp: UnixNanos = start.at(9, 30, 0, 0).to_zoned(zone)?.timestamp().into();
    let needed = app
        .strategy
        .momentum
        .lookbacks
        .iter()
        .copied()
        .max()
        .unwrap_or(20)
        .max(app.strategy.momentum.relative_strength_lookback)
        .max(app.strategy.risk.correlation_lookback)
        .max(20)
        + 1;
    let expected = sessions
        .iter()
        .rev()
        .filter(|session| session.close < start_stamp)
        .take(needed)
        .copied()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        expected.len() == needed,
        "calendar cannot cover ranking warmup"
    );
    Ok(expected)
}

fn parse_cli_daily(
    output: &[u8],
    id: InstrumentId,
    missing: &[Session],
    price_precision: u8,
    observed: UnixNanos,
) -> anyhow::Result<Vec<Bar>> {
    let zone = get_timezone("America/New_York")?;
    let kind = format!("{id}-1-DAY-LAST-EXTERNAL").parse()?;
    let mut added = Vec::new();
    for row in serde_json::from_slice::<Vec<CliCandlestick>>(output)? {
        let timestamp = row.time.parse::<Timestamp>()?;
        let date = timestamp.to_zoned(zone.clone()).date();
        let Some(session) = missing
            .iter()
            .find(|session| session.open.to_datetime_utc().to_zoned(zone.clone()).date() == date)
        else {
            continue;
        };
        let open = row.open.parse::<rust_decimal::Decimal>()?;
        let close = row.close.parse::<rust_decimal::Decimal>()?;
        let high = row
            .high
            .parse::<rust_decimal::Decimal>()?
            .max(open)
            .max(close);
        let low = row
            .low
            .parse::<rust_decimal::Decimal>()?
            .min(open)
            .min(close);
        let volume = row.volume.parse::<u64>()?;
        added.push(Bar::new_checked(
            kind,
            Price::from_decimal_dp(open, price_precision)?,
            Price::from_decimal_dp(high, price_precision)?,
            Price::from_decimal_dp(low, price_precision)?,
            Price::from_decimal_dp(close, price_precision)?,
            Quantity::from(volume),
            session.close,
            observed,
        )?);
    }
    added.sort_by_key(|bar| bar.ts_event);
    Ok(added)
}

fn load_plan(root: &Path) -> anyhow::Result<HistoryPlan> {
    let plan: HistoryPlan = serde_json::from_slice(&fs::read(root.join("plan.json"))?)?;
    plan.strategy.validate()?;
    let required = plan
        .selections
        .iter()
        .flat_map(|selection| selection.candidates.iter().copied())
        .chain(plan.strategy.regime_benchmarks().iter().copied())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        plan.ranges.keys().copied().collect::<BTreeSet<_>>() == required,
        "minute requests must exactly cover selected candidates and benchmarks"
    );
    for selection in &plan.selections {
        selection.validate(&plan.strategy, selection.available_at)?;
    }
    Ok(plan)
}

fn read_daily(path: &Path, id: InstrumentId, sessions: &[Session]) -> anyhow::Result<Vec<Bar>> {
    let mut bars: Vec<Bar> = serde_json::from_slice(&fs::read(path)?)?;
    anyhow::ensure!(
        bars.iter()
            .all(|b| b.bar_type.instrument_id() == id
                && sessions.iter().any(|s| s.close == b.ts_event)),
        "daily cache identity/calendar mismatch"
    );
    // Preserve original receipt timestamps in the untouched source cache. Historical
    // publication at close is explicitly modeled only in the exploratory replay.
    for bar in &mut bars {
        bar.ts_init = bar.ts_event;
    }
    anyhow::ensure!(
        bars.windows(2).all(|w| w[0].ts_event < w[1].ts_event),
        "unordered daily cache"
    );
    Ok(bars)
}

/// Fills only daily bars absent from the existing ranking warmup, leaving its cache untouched.
pub(super) async fn fill_ranking(
    app_path: &Path,
    root: &Path,
    start: Date,
    end: Date,
) -> anyhow::Result<()> {
    let app: App = serde_json::from_slice(&fs::read(app_path)?)?;
    let cache = &app
        .scanner
        .as_ref()
        .context("daily cache required")?
        .cache_dir;
    fs::create_dir_all(root.join("ranking-gap"))?;
    let (context, _receiver) =
        QuoteContext::new(LongbridgeDataClientConfig::default().sdk_config().await?);
    let zone = get_timezone("America/New_York")?;
    let end_stamp = end.at(16, 0, 0, 0).to_zoned(zone.clone())?.timestamp();
    let mut sessions = calendar(&context, end_stamp).await?;
    sessions.extend(&app.strategy.sessions);
    sessions.sort_by_key(|s| s.open);
    sessions.dedup_by_key(|s| s.open);
    let expected = ranking_warmup_sessions(&app, &sessions, start)?;
    fs::write(root.join("sessions.json"), serde_json::to_vec(&sessions)?)?;
    let ids = app.strategy.instrument_ids();
    let mut downloads = stream::iter(ids.iter().enumerate()).map(|(index, id)| {
        let context = &context;
        let app = &app;
        let expected = &expected;
        let zone = zone.clone();
        async move {
        let path = root.join("ranking-gap").join(format!("{id}.json"));
        if path.exists() {
            return Ok::<_, anyhow::Error>(None);
        }
        let cached = cache.join(format!("{id}.json"));
        let bars: Vec<Bar> = if cached.exists() {
            serde_json::from_slice(&fs::read(cached)?)?
        } else {
            vec![]
        };
        let present = bars.iter().map(|b| b.ts_event).collect::<BTreeSet<_>>();
        let missing = expected
            .iter()
            .filter(|s| !present.contains(&s.close))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }
        disk_guard(root)?;
        let from = super::sdk_date(
            missing
                .last()
                .context("missing start")?
                .open
                .to_datetime_utc(),
        )?;
        let to = super::sdk_date(missing[0].open.to_datetime_utc())?;
        let rows = history_api_call_with_retry(|| {
            context.history_candlesticks_by_date(
                id.symbol.as_str(),
                Period::Day,
                AdjustType::NoAdjust,
                Some(from),
                Some(to),
                TradeSessions::Intraday,
            )
        })
        .await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                fs::write(
                    root.join("ranking-gap-error.json"),
                    serde_json::to_vec_pretty(
                        &serde_json::json!({"symbol": id, "error": error.to_string(), "request_index": index, "observed_at": Timestamp::now()}),
                    )?,
                )?;
                return Err(error.into());
            }
        };
        let observed: UnixNanos = Timestamp::now().into();
        let kind = format!("{id}-1-DAY-LAST-EXTERNAL").parse()?;
        let tick: Price = app.instrument_price_increments[&id.to_string()]
            .parse()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut added = Vec::new();
        for row in rows {
            let raw = nautilus_longbridge::common::parse::parse_bar_with_price_precision(
                kind,
                row,
                observed,
                tick.precision,
            )?;
            let date = raw.ts_event.to_datetime_utc().to_zoned(zone.clone()).date();
            if let Some(session) = missing
                .iter()
                .find(|s| s.open.to_datetime_utc().to_zoned(zone.clone()).date() == date)
            {
                added.push(Bar::new(
                    kind,
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
        added.sort_by_key(|b| b.ts_event);
        let temporary = path.with_extension("partial");
        fs::write(&temporary, serde_json::to_vec(&added)?)?;
        fs::rename(temporary, path)?;
        Ok::<_, anyhow::Error>(Some((id, missing.len(), added.len())))
        }
    }).buffer_unordered(4);
    let mut completed = 0;
    while let Some(result) = downloads.next().await {
        if let Some((id, requested, returned)) = result? {
            completed += 1;
            if completed % 50 == 0 {
                println!(
                    "ranking warmup: {completed} new requests; latest {id}: {requested} missing, {returned} returned"
                );
            }
        }
    }
    println!("Ranking gaps collected: {completed} requests; no minute data requested");
    Ok(())
}

/// Continues ranking-gap collection through the authenticated Longbridge CLI.
pub(super) async fn fill_ranking_cli(
    app_path: &Path,
    root: &Path,
    start: Date,
) -> anyhow::Result<()> {
    let app: App = serde_json::from_slice(&fs::read(app_path)?)?;
    let cache = &app
        .scanner
        .as_ref()
        .context("daily cache required")?
        .cache_dir;
    let sessions: Vec<Session> =
        serde_json::from_slice(&fs::read(root.join("sessions.json")).context(
            "sessions.json missing; run --history-fill-ranking once to fetch the calendar",
        )?)?;
    let expected = ranking_warmup_sessions(&app, &sessions, start)?;
    fs::create_dir_all(root.join("ranking-gap"))?;
    let mut jobs = Vec::new();
    for id in app.strategy.instrument_ids() {
        let path = root.join("ranking-gap").join(format!("{id}.json"));
        if path.exists() {
            continue;
        }
        let cached = cache.join(format!("{id}.json"));
        let bars: Vec<Bar> = if cached.exists() {
            serde_json::from_slice(&fs::read(cached)?)?
        } else {
            vec![]
        };
        let present = bars.iter().map(|bar| bar.ts_event).collect::<BTreeSet<_>>();
        let missing = expected
            .iter()
            .filter(|session| !present.contains(&session.close))
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            continue;
        }
        let tick: Price = app.instrument_price_increments[&id.to_string()]
            .parse()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        jobs.push((id, path, missing, tick.precision));
    }
    let total = jobs.len();
    let mut downloads = stream::iter(jobs.into_iter().enumerate())
        .map(|(index, (id, path, missing, precision))| {
            let root = root.to_path_buf();
            async move {
                disk_guard(&root)?;
                let zone = get_timezone("America/New_York")?;
                let first = missing
                    .last()
                    .context("missing CLI ranking start")?
                    .open
                    .to_datetime_utc()
                    .to_zoned(zone.clone())
                    .date();
                let last = missing[0].open.to_datetime_utc().to_zoned(zone).date();
                let symbol = id.symbol.to_string();
                let output =
                    tokio::task::spawn_blocking(move || cli_history(&symbol, "day", first, last))
                        .await??;
                let observed: UnixNanos = Timestamp::now().into();
                let bars = parse_cli_daily(&output, id, &missing, precision, observed)?;
                let temporary: PathBuf = path.with_extension("partial");
                fs::write(&temporary, serde_json::to_vec(&bars)?)?;
                fs::rename(temporary, path)?;
                Ok::<_, anyhow::Error>((index, id, missing.len(), bars.len()))
            }
        })
        .buffer_unordered(4);
    let mut completed = 0;
    while let Some(result) = downloads.next().await {
        let (index, id, requested, returned) = result?;
        completed += 1;
        if completed % 50 == 0 || completed == total {
            println!(
                "ranking-cli {}/{} {id}: {requested} missing, {returned} returned",
                index + 1,
                total
            );
        }
    }
    fs::write(
        root.join("ranking-gap-cli-audit.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "fetched_at": Timestamp::now(),
            "symbols_requested": total,
            "symbols_completed": completed,
            "orders_submitted": 0,
            "source": "longbridge kline history --period day"
        }))?,
    )?;
    Ok(())
}

/// Explicitly labels the current-master, daily-ranking approximation before planning minute requests.
pub(super) fn plan(app_path: &Path, root: &Path, start: Date, end: Date) -> anyhow::Result<()> {
    anyhow::ensure!(start <= end, "invalid dates");
    let app: App = serde_json::from_slice(&fs::read(app_path)?)?;
    let daily_cache_dir = app
        .scanner
        .as_ref()
        .context("daily cache configuration missing")?
        .cache_dir
        .canonicalize()?;
    fs::create_dir_all(root)?;
    let mut c = app.strategy;
    if root.join("sessions.json").exists() {
        c.sessions = serde_json::from_slice(&fs::read(root.join("sessions.json"))?)?;
    }
    let timezone = get_timezone("America/New_York")?;
    let dates = c
        .sessions
        .iter()
        .map(|s| s.open.to_datetime_utc().to_zoned(timezone.clone()).date())
        .collect::<Vec<_>>();
    let first = dates
        .iter()
        .position(|d| *d >= start)
        .context("missing start calendar")?;
    let last = dates
        .iter()
        .rposition(|d| *d <= end)
        .context("missing end calendar")?;
    anyhow::ensure!(
        first <= last && dates[0] <= start && dates[last] == end,
        "insufficient calendar warmup or end coverage"
    );
    c.trading_start = c.sessions[first].open;
    c.trading_end = c.sessions[last].close;
    c.longbridge = false;
    c.dry_run = false;
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    c.slc.require_intraday_trend = true;
    c.momentum.daily_only = true;
    let market = c.market.as_mut().context("full-market config required")?;
    market.max_candidates = (market.universe_size as f64 * market.top_fraction).ceil() as usize * 2;
    market.max_snapshot_age_minutes = 390;
    for meta in &mut c.universe {
        meta.effective_from = c.sessions[0].open;
        meta.known_at = c.sessions[0].open;
        meta.effective_until = c.sessions[last].close;
    }
    c.validate()?;
    let mut daily = BTreeMap::new();
    for id in c.instrument_ids() {
        let path = daily_cache_dir.join(format!("{id}.json"));
        let mut bars = if path.exists() {
            read_daily(&path, id, &c.sessions)?
        } else {
            vec![]
        };
        let gap = root.join("ranking-gap").join(format!("{id}.json"));
        if gap.exists() {
            bars.extend(read_daily(&gap, id, &c.sessions)?);
        }
        bars.sort_by_key(|b| b.ts_event);
        anyhow::ensure!(
            bars.windows(2).all(|w| w[0].ts_event < w[1].ts_event),
            "overlapping ranking cache for {id}"
        );
        if !bars.is_empty() {
            daily.insert(id, bars);
        }
    }
    let mut ranker = CrossSectionalMomentumRanker::default();
    let mut all = daily.values().flatten().copied().collect::<Vec<_>>();
    all.sort_by_key(|b| b.ts_event);
    let mut cursor = 0;
    let mut selections = Vec::new();
    let mut ranges: BTreeMap<InstrumentId, Session> = BTreeMap::new();
    for (index, date) in dates.iter().enumerate().take(last + 1).skip(first) {
        let session = c.sessions[index];
        while cursor < all.len() && all[cursor].ts_event < session.open {
            ranker.update_daily(all[cursor], session.open)?;
            cursor += 1;
        }
        let timestamp = UnixNanos::from(session.open.as_u64() + 1);
        let observations = c
            .universe
            .iter()
            .filter_map(|m| {
                let bar = daily
                    .get(&m.instrument_id)?
                    .iter()
                    .find(|b| b.ts_event == session.close)?;
                Some(MarketObservation {
                    symbol: m.instrument_id,
                    timestamp,
                    available_at: timestamp,
                    open: bar.open,
                    last: bar.open,
                    relative_volume: 1.0,
                })
            })
            .collect::<Vec<_>>();
        let selection = ranker.rank_market(timestamp, session.open, &c, &observations)?;
        for id in selection
            .candidates
            .iter()
            .copied()
            .chain(c.regime_benchmarks().iter().copied())
        {
            let range = ranges.entry(id).or_insert(Session {
                open: c.sessions[index.saturating_sub(12)].open,
                close: session.close,
            });
            range.close = session.close;
        }
        println!(
            "{} eligible={} candidates={} observed={}",
            date,
            selection.ranking.ranks.len(),
            selection.candidates.len(),
            selection.observed_size
        );
        selections.push(selection);
    }
    let output = HistoryPlan {
        provenance: "EXPLORATORY ONLY: source security-master universe, market cap and sectors held fixed across history; survivorship and classification bias. Daily ranking frozen at session open, using prior completed daily bars plus that day's opening price; no intraday ranking refresh. API retrieval timestamps retained in audit, historical bar availability modeled at close. Quotes, fills and short availability require explicit simulation assumptions. Not point-in-time alpha evidence.".to_string(),
        starting_equity: rust_decimal::Decimal::from(100_000),
        spread_bps: rust_decimal::Decimal::from(4),
        opening_spread_multiplier: rust_decimal::Decimal::from(2),
        quote_depth: nautilus_model::types::Quantity::from(100),
        slippage_probability: 1.0,
        daily_cache_dir,
        fetched_at: Timestamp::now().into(), strategy: c,
        price_increments: app.instrument_price_increments.into_iter().map(|(k,v)| Ok((k.parse()?, v.parse().map_err(|e| anyhow::anyhow!("{e}"))?))).collect::<anyhow::Result<_>>()?,
        selections, ranges,
    };
    fs::write(root.join("plan.json"), serde_json::to_vec(&output)?)?;
    println!("Minute histories required: {} symbols", output.ranges.len());
    Ok(())
}

pub(super) async fn minutes(root: &Path) -> anyhow::Result<()> {
    let plan = load_plan(root)?;
    fs::create_dir_all(root.join("minute"))?;
    let timezone = get_timezone("America/New_York")?;
    let (context, _receiver) =
        QuoteContext::new(LongbridgeDataClientConfig::default().sdk_config().await?);
    let total = plan.ranges.len();
    // Verify required indices first, then fill the earliest candidate sessions
    let mut ranges = plan.ranges.iter().collect::<Vec<_>>();
    ranges.sort_by_key(|(id, range)| {
        (
            !plan.strategy.regime_benchmarks().contains(id),
            range.open,
            **id,
        )
    });
    let mut downloads = stream::iter(ranges.into_iter().enumerate())
        .map(|(index, (id, range))| {
            let context = &context;
            let timezone = timezone.clone();
            async move {
                let dir = root.join("minute").join(id.to_string());
                fs::create_dir_all(&dir)?;
                let marker = dir.join("complete.json");
                if marker.exists()
                    && serde_json::from_slice::<Session>(&fs::read(&marker)?)? == *range
                {
                    return Ok::<(), anyhow::Error>(());
                }
                let mut before = range.close.to_datetime_utc().to_zoned(timezone.clone());
                let mut pages = 0;
                loop {
                    disk_guard(root)?;
                    let path = dir.join(format!("{}.csv", before.timestamp().as_second()));
                    let oldest = if path.exists() {
                        fs::read_to_string(&path)?
                            .lines()
                            .filter_map(|l| l.split(',').next()?.parse::<i64>().ok())
                            .min()
                    } else {
                        let end = time::Date::from_calendar_date(
                            i32::from(before.year()),
                            time::Month::try_from(before.month() as u8)?,
                            before.day() as u8,
                        )?
                        .with_time(time::Time::from_hms(
                            before.hour() as u8,
                            before.minute() as u8,
                            before.second() as u8,
                        )?);
                        let rows = history_api_call_with_retry(|| {
                            context.history_candlesticks_by_offset(
                                id.symbol.as_str(),
                                Period::OneMinute,
                                AdjustType::NoAdjust,
                                false,
                                Some(end),
                                1000,
                                TradeSessions::Intraday,
                            )
                        })
                        .await
                        .with_context(|| {
                            format!("minute history failed for {id} before {before}")
                        })?;
                        let oldest = rows.iter().map(|r| r.timestamp.unix_timestamp()).min();
                        write_rows(&path, &rows)?;
                        oldest
                    };
                    let Some(oldest) = oldest else {
                        break;
                    };
                    anyhow::ensure!(
                        oldest < before.timestamp().as_second(),
                        "history pagination failed to advance for {id}"
                    );
                    pages += 1;
                    if i128::from(oldest) * 1_000_000_000 <= i128::from(range.open.as_u64()) {
                        break;
                    }
                    before = Timestamp::from_second(oldest - 60)?.to_zoned(timezone.clone());
                }
                fs::write(marker, serde_json::to_vec(range)?)?;
                println!("minute {}/{} {id} pages={pages}", index + 1, total);
                Ok::<(), anyhow::Error>(())
            }
        })
        .buffer_unordered(4);
    while let Some(result) = downloads.next().await {
        result?;
    }
    fs::write(
        root.join("minute-audit.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"fetched_at": Timestamp::now(), "symbols": plan.ranges.len(), "orders_submitted": 0, "rate_limit": "shared 60/30.5s history and 10/1.1s quote", "minimum_free_disk_gib": 3}),
        )?,
    )?;
    Ok(())
}

/// Uses the authenticated Longbridge CLI to retry incomplete regular sessions.
///
/// Each request spans at most two trading sessions, keeping the result below the CLI's
/// 1,000-row response ceiling. Successful session markers make the operation resumable.
pub(super) fn minutes_cli(root: &Path) -> anyhow::Result<()> {
    let plan = load_plan(root)?;
    fs::create_dir_all(root.join("minute"))?;
    let timezone = get_timezone("America/New_York")?;
    let total = plan.ranges.len();
    let mut requests = 0;
    let mut ranges = plan.ranges.iter().collect::<Vec<_>>();
    ranges.sort_by_key(|(id, range)| {
        (
            !plan.strategy.regime_benchmarks().contains(id),
            range.open,
            **id,
        )
    });
    for (index, (id, range)) in ranges.into_iter().enumerate() {
        let directory = root.join("minute").join(id.to_string());
        fs::create_dir_all(&directory)?;
        let mut observed = existing_minute_seconds(&directory)?;
        let sessions = plan
            .strategy
            .sessions
            .iter()
            .copied()
            .filter(|session| range.open <= session.open && session.close <= range.close)
            .collect::<Vec<_>>();
        for chunk in sessions.chunks(2) {
            let needs_retry = chunk.iter().any(|session| {
                let expected = (session.close.as_u64() - session.open.as_u64()) / 60_000_000_000;
                let marker = directory.join(format!("cli-{}.json", session.close.as_u64()));
                !marker.exists() && session_minute_count(&observed, *session) < expected as usize
            });
            if !needs_retry {
                continue;
            }
            disk_guard(root)?;
            let start = chunk[0]
                .open
                .to_datetime_utc()
                .to_zoned(timezone.clone())
                .date();
            let end = chunk[chunk.len() - 1]
                .open
                .to_datetime_utc()
                .to_zoned(timezone.clone())
                .date();
            let output = cli_history(id.symbol.as_str(), "1m", start, end)?;
            let rows = parse_cli_minutes(&output, chunk)?;
            write_cli_rows(&directory.join(format!("cli-{start}-{end}.csv")), &rows)?;
            observed.extend(rows.iter().map(|row| row.timestamp));
            for session in chunk {
                fs::write(
                    directory.join(format!("cli-{}.json", session.close.as_u64())),
                    serde_json::to_vec(session)?,
                )?;
            }
            requests += 1;
        }
        fs::write(directory.join("complete.json"), serde_json::to_vec(range)?)?;
        println!(
            "minute-cli {}/{} {id} requests={requests}",
            index + 1,
            total
        );
    }
    fs::write(
        root.join("minute-cli-audit.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "fetched_at": Timestamp::now(),
            "symbols": plan.ranges.len(),
            "requests": requests,
            "orders_submitted": 0,
            "source": "longbridge kline history",
            "maximum_trading_sessions_per_request": 2,
            "minimum_free_disk_gib": 3
        }))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_minutes_and_rejects_out_of_session_rows() {
        let open: UnixNanos = "2026-06-01T13:30:00Z".parse::<Timestamp>().unwrap().into();
        let close: UnixNanos = "2026-06-01T20:00:00Z".parse::<Timestamp>().unwrap().into();
        let session = Session { open, close };
        let valid = br#"[{"close":"100.1","high":"100.2","low":"99.9","open":"100.0","time":"2026-06-01T13:30:00Z","volume":"12"}]"#;
        let rows = parse_cli_minutes(valid, &[session]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, 1_780_320_600);

        let invalid = br#"[{"close":"100.1","high":"100.2","low":"99.9","open":"100.0","time":"2026-06-01T20:00:00Z","volume":"12"}]"#;
        assert!(parse_cli_minutes(invalid, &[session]).is_err());
    }

    #[test]
    fn parses_cli_daily_at_the_session_close() {
        let open: UnixNanos = "2026-06-01T13:30:00Z".parse::<Timestamp>().unwrap().into();
        let close: UnixNanos = "2026-06-01T20:00:00Z".parse::<Timestamp>().unwrap().into();
        let valid = br#"[{"close":"101.0","high":"100.5","low":"99.0","open":"100.0","time":"2026-06-01T04:00:00Z","volume":"12"}]"#;
        let bars = parse_cli_daily(
            valid,
            "SPY.US.LONGBRIDGE".parse().unwrap(),
            &[Session { open, close }],
            2,
            close,
        )
        .unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].ts_event, close);
        assert_eq!(bars[0].high, Price::from("101.0"));
    }
}
