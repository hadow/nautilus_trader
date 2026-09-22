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

//! Full-universe daily selection and bounded intraday collection on the adapter's quote connection.
//!
//! 中文说明：负责准备股票主数据、完整市场观察和实时候选发布；覆盖不足、数据过期或成员
//! 不一致时不授权新候选，避免把当前订阅集合误当作全市场。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use jiff::Timestamp;
use longbridge::{
    quote::{AdjustType, CalcIndex, Period, QuoteContext, SecurityBoard, TradeSessions},
    screener::{ScreenerCondition, ScreenerContext},
};
use nautilus_common::messages::DataEvent;
use nautilus_core::UnixNanos;
use nautilus_longbridge::{
    LongbridgeDataClientConfig,
    common::{parse::parse_bar_with_price_precision, rate_limit::quote_api_call_with_retry},
};
use nautilus_model::{
    data::{Bar, BarType},
    identifiers::InstrumentId,
    types::Price,
};
use nautilus_trading::examples::strategies::slc_momentum::{
    CrossSectionalMomentumRanker, MarketObservation, MarketUpdate, SlcMomentumReport,
    SymbolMetadata,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use super::{App, MINUTE, calendar, warmup};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ScannerConfig {
    pub cache_dir: PathBuf,
    pub industry_etfs: BTreeMap<String, InstrumentId>,
    pub default_price_increment: Price,
    pub max_cache_bytes: u64,
    pub poll_seconds: u64,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            cache_dir: "test_data/local/slc_momentum/market-cache".into(),
            industry_etfs: BTreeMap::new(),
            default_price_increment: Price::from("0.01"),
            max_cache_bytes: 512 * 1024 * 1024,
            poll_seconds: 90,
        }
    }
}

impl ScannerConfig {
    fn next_refresh(
        &self,
        published: UnixNanos,
        close: UnixNanos,
        momentum: &nautilus_trading::examples::strategies::slc_momentum::MomentumConfig,
        has_ranks: bool,
    ) -> UnixNanos {
        if has_ranks && momentum.daily_only {
            return close;
        }
        let delay = if has_ranks {
            momentum.refresh_minutes * MINUTE
        } else {
            self.poll_seconds * 1_000_000_000
        };
        UnixNanos::from(published.as_u64().saturating_add(delay).min(close.as_u64()))
    }
}

fn now() -> UnixNanos {
    Timestamp::now().into()
}

#[derive(Debug, Deserialize)]
struct ScreenerPage {
    total: usize,
    items: Vec<serde_json::Value>,
}

fn append_page(
    rows: &mut BTreeMap<String, serde_json::Value>,
    expected: &mut Option<usize>,
    page: ScreenerPage,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        page.total > 0 && page.total <= 30_000,
        "invalid US security count"
    );
    if let Some(total) = expected {
        anyhow::ensure!(
            *total == page.total,
            "screener universe changed during pagination; retry preparation"
        );
    }
    *expected = Some(page.total);
    anyhow::ensure!(!page.items.is_empty(), "incomplete screener pagination");
    for item in page.items {
        let counter = item["counter_id"]
            .as_str()
            .context("screener counter_id missing")?;
        let ticker = counter.strip_prefix("ST/US/").filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        });
        let symbol = ticker.map(|ticker| format!("{ticker}.US"));
        let key = symbol.clone().unwrap_or_else(|| counter.to_owned());
        let indicators = item["indicators"]
            .as_array()
            .context("screener indicators missing")?;
        let industry = indicators
            .iter()
            .find(|i| i["key"] == "industry")
            .and_then(|i| i["value"].as_str())
            .unwrap_or("");
        anyhow::ensure!(
            rows.insert(
                key,
                serde_json::json!({"symbol": symbol, "industry": industry, "counter_id": counter})
            )
            .is_none(),
            "duplicate screener symbol; retry a stable scan"
        );
    }
    anyhow::ensure!(rows.len() <= page.total, "screener count mismatch");
    Ok(rows.len() == page.total)
}

pub(super) async fn prepare(template: &Path, output: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        !output.exists(),
        "output already exists; choose a new dated preparation path"
    );
    let mut app: App = serde_json::from_slice(&fs::read(template)?)?;
    let scanner = app.scanner.clone().context("scanner config required")?;
    let size = app
        .strategy
        .market
        .as_ref()
        .context("market config required")?
        .universe_size;
    anyhow::ensure!((2..=20_000).contains(&size), "invalid universe size");
    let sdk = LongbridgeDataClientConfig::default().sdk_config().await?;
    let screener = ScreenerContext::new(sdk.clone());
    let mut rows = BTreeMap::new();
    let mut total = None;
    for page in 0..300 {
        let response = quote_api_call_with_retry(|| {
            screener.screener_search(
                "US",
                None,
                vec![ScreenerCondition {
                    key: "marketcap".into(),
                    min: "0".into(),
                    max: String::new(),
                    tech_values: serde_json::Value::Null,
                }],
                vec!["industry".into(), "marketcap".into()],
                page,
                100,
            )
        })
        .await?;
        if append_page(
            &mut rows,
            &mut total,
            serde_json::from_value(response.data)?,
        )? {
            break;
        }
    }
    anyhow::ensure!(Some(rows.len()) == total, "US screener did not complete");
    let (context, _receiver) = QuoteContext::new(sdk);
    app.strategy.sessions = calendar(&context, Timestamp::now()).await?;
    let mut securities = Vec::new();
    let symbols = rows
        .values()
        .filter_map(|row| row["symbol"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let unsupported_counters = rows
        .values()
        .filter(|row| row["symbol"].is_null())
        .map(|row| row["counter_id"].clone())
        .collect::<Vec<_>>();
    println!(
        "Screener complete: {} records, {} supported US stock codes, {} excluded counters",
        rows.len(),
        symbols.len(),
        unsupported_counters.len()
    );
    for chunk in symbols.chunks(500) {
        let info = quote_api_call_with_retry(|| context.static_info(chunk.to_vec())).await?;
        anyhow::ensure!(
            info.len() == chunk.len(),
            "incomplete security master response"
        );
        let caps = quote_api_call_with_retry(|| {
            context.calc_indexes(chunk.to_vec(), [CalcIndex::TotalMarketValue])
        })
        .await?
        .into_iter()
        .map(|c| (c.symbol, c.total_market_value))
        .collect::<BTreeMap<_, _>>();
        for security in info {
            let row = rows
                .get(&security.symbol)
                .context("unexpected security master symbol")?;
            let cap = caps
                .get(&security.symbol)
                .copied()
                .flatten()
                .unwrap_or(Decimal::ZERO);
            let industry = row["industry"].as_str().unwrap_or("");
            // Screener capitalization + industry and main-board USD listing exclude OTC and index products.
            // Explicit product names cover funds/warrants which some vendors classify as USMain.
            let name = security.name_en.to_uppercase();
            if security.board == SecurityBoard::USMain
                && security.currency == "USD"
                && cap > Decimal::ZERO
                && !industry.is_empty()
                && !industry.to_uppercase().contains("ETF")
                && ![
                    " ETF",
                    " ETN",
                    " WARRANT",
                    " PREFERRED",
                    " DEPOSITARY SHARES",
                    " UNIT",
                ]
                .iter()
                .any(|word| name.contains(word))
            {
                securities.push((security.symbol, cap, industry.to_owned()));
            }
        }
    }
    securities.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    anyhow::ensure!(
        securities.len() >= size,
        "only {} eligible US listings; need {size}",
        securities.len()
    );
    securities.truncate(size);
    let published = now();
    let session = app
        .strategy
        .sessions
        .iter()
        .find(|s| published < s.close)
        .copied()
        .context("next US session missing")?;
    app.strategy.trading_start = session.open.max(published);
    app.strategy.trading_end = session.close;
    app.strategy.universe.clear();
    let mut unmapped = BTreeSet::new();
    for (symbol, cap, industry) in securities {
        let etf = scanner
            .industry_etfs
            .get(&industry)
            .copied()
            .unwrap_or_else(|| {
                unmapped.insert(industry.clone());
                app.strategy.benchmarks[0]
            });
        // Unmapped industries share one conservative risk group; SPY is explicitly only a proxy.
        let sector = if scanner.industry_etfs.contains_key(&industry) {
            etf.symbol.to_string()
        } else {
            "UNMAPPED".into()
        };
        app.strategy.universe.push(SymbolMetadata {
            instrument_id: format!("{symbol}.LONGBRIDGE").parse()?,
            sector,
            sector_etf: etf,
            market_cap: cap,
            known_at: published,
            effective_from: published,
            effective_until: session.close,
        });
    }
    anyhow::ensure!(
        unmapped.is_empty() || app.strategy.momentum.sector_weight == 0.0,
        "unmapped industries require explicit sector ETFs or sector_weight=0; missing: {unmapped:?}"
    );
    for id in app.strategy.instrument_ids() {
        app.instrument_price_increments
            .entry(id.to_string())
            .or_insert_with(|| scanner.default_price_increment.to_string());
    }
    app.strategy.longbridge = true;
    app.strategy.dry_run = true;
    app.strategy.validate()?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, serde_json::to_vec_pretty(&app)?)?;
    fs::write(
        output.with_extension("universe-audit.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "known_at": published, "screener_listings": total, "selected": size, "selection": "USMain USD equities, descending observed market capitalization",
            "unmapped_industries": unmapped, "excluded_counter_ids": unsupported_counters, "sector_proxy": "SPY only for UNMAPPED; sector ranking weight must be zero", "historical_membership": false,
            "source": "Longbridge screener + static_info + exact calc_indexes TotalMarketValue", "orders_submitted": 0
        }))?,
    )?;
    println!("Prepared {size} current US equities: {}", output.display());
    Ok(())
}

fn cache_size(directory: &Path) -> anyhow::Result<u64> {
    let mut size = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            size += entry.metadata()?.len();
        }
    }
    Ok(size)
}

async fn history_parallel(
    context: &QuoteContext,
    app: &App,
    ids: &[InstrumentId],
    daily: bool,
    batches: usize,
) -> anyhow::Result<Vec<Bar>> {
    let chunks = ids.chunks(ids.len().div_ceil(4).max(1)).collect::<Vec<_>>();
    let empty = &[][..];
    let (a, b, c, d) = tokio::join!(
        warmup(
            context,
            app,
            chunks.first().copied().unwrap_or(empty),
            daily,
            batches
        ),
        warmup(
            context,
            app,
            chunks.get(1).copied().unwrap_or(empty),
            daily,
            batches
        ),
        warmup(
            context,
            app,
            chunks.get(2).copied().unwrap_or(empty),
            daily,
            batches
        ),
        warmup(
            context,
            app,
            chunks.get(3).copied().unwrap_or(empty),
            daily,
            batches
        )
    );
    Ok([a?, b?, c?, d?].into_iter().flatten().collect())
}

pub(super) async fn daily_history(context: &QuoteContext, app: &App) -> anyhow::Result<Vec<Bar>> {
    let scanner = app.scanner.as_ref().context("scanner config required")?;
    anyhow::ensure!(
        (30..=120).contains(&scanner.poll_seconds) && scanner.max_cache_bytes <= 1024 * 1024 * 1024,
        "invalid polling interval/cache budget"
    );
    fs::create_dir_all(&scanner.cache_dir)?;
    let cutoff = app
        .strategy
        .sessions
        .iter()
        .rfind(|s| s.close < app.strategy.trading_start)
        .context("prior session missing")?
        .close;
    let ids = app.strategy.instrument_ids();
    let mut result = Vec::new();
    let mut missing = Vec::new();
    let mut used = cache_size(&scanner.cache_dir)?;
    for id in &ids {
        let path = scanner.cache_dir.join(format!("{id}.json"));
        let cached = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Bar>>(&b).ok());
        if let Some(bars) = cached.filter(|bars| {
            bars.len() >= 21
                && bars.last().is_some_and(|b| b.ts_event == cutoff)
                && bars.iter().all(|b| {
                    b.bar_type.instrument_id() == *id && b.ts_init <= now() && b.ts_event <= cutoff
                })
        }) {
            result.extend(bars);
        } else {
            missing.push(*id);
        }
    }
    println!(
        "Daily cache: {} symbols ready, {} to fetch; {} MiB budget",
        ids.len() - missing.len(),
        missing.len(),
        scanner.max_cache_bytes / 1024 / 1024
    );
    for chunk in missing.chunks(100) {
        let disk = std::process::Command::new("df")
            .arg("-Pk")
            .arg(&scanner.cache_dir)
            .output()?;
        anyhow::ensure!(
            disk.status.success(),
            "cannot verify available cache disk space"
        );
        let text = String::from_utf8(disk.stdout)?;
        let available = text
            .lines()
            .nth(1)
            .and_then(|line| line.split_whitespace().nth(3))
            .context("invalid df output")?
            .parse::<u64>()?;
        anyhow::ensure!(
            available >= 2 * 1024 * 1024,
            "less than 2 GiB disk space available; daily download stopped"
        );
        let bars = history_parallel(context, app, chunk, true, 0).await?;
        let mut grouped = BTreeMap::<InstrumentId, Vec<Bar>>::new();
        for bar in bars {
            grouped
                .entry(bar.bar_type.instrument_id())
                .or_default()
                .push(bar);
        }
        for (id, mut bars) in grouped {
            bars.sort_by_key(|b| b.ts_event);
            let path = scanner.cache_dir.join(format!("{id}.json"));
            let bytes = serde_json::to_vec(&bars)?;
            used = used.saturating_sub(fs::metadata(&path).map_or(0, |m| m.len()))
                + bytes.len() as u64;
            anyhow::ensure!(
                used <= scanner.max_cache_bytes,
                "daily cache budget exceeded; free space or use another cache directory"
            );
            let temp = path.with_extension("json.tmp");
            fs::write(&temp, bytes)?;
            fs::rename(temp, path)?;
            result.extend(bars);
        }
        println!(
            "Daily history: {} bars; cache {} MiB",
            result.len(),
            used / 1024 / 1024
        );
    }
    Ok(result)
}

async fn observations(context: &QuoteContext, app: &App) -> anyhow::Result<Vec<MarketObservation>> {
    let mut result = Vec::new();
    let ids = app
        .strategy
        .universe
        .iter()
        .map(|m| m.instrument_id)
        .collect::<Vec<_>>();
    for chunk in ids.chunks(500) {
        let symbols = chunk
            .iter()
            .map(|id| id.symbol.to_string())
            .collect::<Vec<_>>();
        let volume = quote_api_call_with_retry(|| {
            context.calc_indexes(symbols.clone(), [CalcIndex::VolumeRatio])
        })
        .await?;
        let volumes = volume
            .into_iter()
            .map(|v| (v.symbol, v.volume_ratio))
            .collect::<BTreeMap<_, _>>();
        let quotes = quote_api_call_with_retry(|| context.quote(symbols.clone())).await?;
        let available = now();
        for q in quotes {
            if q.trade_status != longbridge::quote::TradeStatus::Normal
                || q.open <= Decimal::ZERO
                || q.last_done <= Decimal::ZERO
            {
                continue;
            }
            result.push(MarketObservation {
                symbol: format!("{}.LONGBRIDGE", q.symbol).parse()?,
                timestamp: UnixNanos::from(u64::try_from(q.timestamp.unix_timestamp_nanos())?),
                available_at: available,
                open: q
                    .open
                    .to_string()
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid open: {e}"))?,
                last: q
                    .last_done
                    .to_string()
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid price: {e}"))?,
                relative_volume: volumes
                    .get(&q.symbol)
                    .copied()
                    .flatten()
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0),
            });
        }
    }
    Ok(result)
}

async fn minute_batch(
    context: &QuoteContext,
    app: &App,
    ids: &[InstrumentId],
    cutoff: UnixNanos,
) -> anyhow::Result<Vec<Bar>> {
    let mut bars = Vec::new();
    for id in ids {
        let kind: BarType = format!("{id}-1-MINUTE-LAST-EXTERNAL").parse()?;
        let tick: Price = app.instrument_price_increments[&id.to_string()]
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid tick: {e}"))?;
        let rows = quote_api_call_with_retry(|| {
            context.candlesticks(
                id.symbol.as_str(),
                Period::OneMinute,
                20,
                AdjustType::NoAdjust,
                TradeSessions::Intraday,
            )
        })
        .await?;
        let available = now();
        let successor = rows
            .iter()
            .map(|r| r.timestamp.unix_timestamp_nanos())
            .max()
            .unwrap_or(0);
        for row in rows {
            let raw = parse_bar_with_price_precision(kind, row, available, tick.precision)?;
            let end = UnixNanos::from(raw.ts_event.as_u64() + MINUTE);
            if end <= cutoff
                && i128::from(end.as_u64()) <= successor
                && app
                    .strategy
                    .sessions
                    .iter()
                    .any(|s| s.open < end && end <= s.close)
            {
                bars.push(Bar::new(
                    kind, raw.open, raw.high, raw.low, raw.close, raw.volume, end, available,
                ));
            }
        }
    }
    Ok(bars)
}

pub(super) async fn collect(
    app: App,
    mut receiver: tokio::sync::watch::Receiver<Option<QuoteContext>>,
    history: Vec<Bar>,
    report: Arc<Mutex<SlcMomentumReport>>,
    sender: tokio::sync::mpsc::UnboundedSender<DataEvent>,
) -> anyhow::Result<()> {
    let context = loop {
        if let Some(context) = receiver.borrow().clone() {
            break context;
        }
        receiver
            .changed()
            .await
            .context("data client closed before collector startup")?;
    };
    let work = async {
        while !report
            .lock()
            .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
            .started
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut ranker = CrossSectionalMomentumRanker::default();
        for bar in history {
            ranker.update_daily(bar, now())?;
        }
        let scanner = app.scanner.as_ref().context("scanner config required")?;
        let mut next_rank = UnixNanos::default();
        let mut selection = None;
        let mut previous = BTreeSet::new();
        let mut seen = BTreeMap::<InstrumentId, UnixNanos>::new();
        while now() < app.strategy.trading_end {
            let cycle = tokio::time::Instant::now();
            let timestamp = now();
            let session = app
                .strategy
                .sessions
                .iter()
                .find(|s| s.open <= timestamp && timestamp < s.close)
                .context("collector requires regular session")?;
            let mut changed = None;
            if timestamp >= next_rank {
                let quotes = observations(&context, &app).await?;
                let published = now();
                let global = ranker.rank_market(published, session.open, &app.strategy, &quotes)?;
                println!(
                    "MARKET universe={} observed={} eligible={} candidates={} timestamp={}",
                    global.universe_size,
                    global.observed_size,
                    global.ranking.ranks.len(),
                    global.candidates.len(),
                    published
                );
                next_rank = scanner.next_refresh(
                    published,
                    session.close,
                    &app.strategy.momentum,
                    !global.ranking.ranks.is_empty(),
                );
                selection = Some(global.clone());
                changed = Some(global);
            }
            let mut active = app
                .strategy
                .regime_benchmarks()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            if let Some(global) = &selection {
                active.extend(&global.candidates);
            }
            active.extend(
                report
                    .lock()
                    .map_err(|_| anyhow::anyhow!("report lock poisoned"))?
                    .retained_symbols
                    .iter(),
            );
            let added = active
                .difference(&previous)
                .copied()
                .collect::<BTreeSet<_>>();
            let mut bars = history_parallel(
                &context,
                &app,
                &added.iter().copied().collect::<Vec<_>>(),
                false,
                app.warmup_minute_batches,
            )
            .await?;
            let cutoff = UnixNanos::from(now().as_u64() / MINUTE * MINUTE);
            let continuing = active.intersection(&previous).copied().collect::<Vec<_>>();
            // Four requests in flight at most; the adapter's process-wide limiter also covers execution subscriptions.
            let chunks = continuing
                .chunks(continuing.len().div_ceil(4).max(1))
                .collect::<Vec<_>>();
            let empty = &[][..];
            let (a, b, c, d) = tokio::join!(
                minute_batch(
                    &context,
                    &app,
                    chunks.first().copied().unwrap_or(empty),
                    cutoff
                ),
                minute_batch(
                    &context,
                    &app,
                    chunks.get(1).copied().unwrap_or(empty),
                    cutoff
                ),
                minute_batch(
                    &context,
                    &app,
                    chunks.get(2).copied().unwrap_or(empty),
                    cutoff
                ),
                minute_batch(
                    &context,
                    &app,
                    chunks.get(3).copied().unwrap_or(empty),
                    cutoff
                )
            );
            bars.extend([a?, b?, c?, d?].into_iter().flatten().filter(|b| {
                seen.get(&b.bar_type.instrument_id())
                    .is_none_or(|last| b.ts_event > *last)
            }));
            bars.sort_by_key(|b| (b.ts_event, b.bar_type));
            for bar in &bars {
                seen.insert(bar.bar_type.instrument_id(), bar.ts_event);
            }
            sender.send(DataEvent::Data(
                MarketUpdate {
                    timestamp: now(),
                    selection: changed,
                    warmup_symbols: added,
                    bars,
                }
                .into_data(),
            ))?;
            previous = active;
            seen.retain(|id, _| previous.contains(id));
            tokio::time::sleep_until(cycle + Duration::from_secs(scanner.poll_seconds)).await;
        }
        Ok(())
    };
    tokio::select! {
        result = work => result,
        () = async {
            while receiver.changed().await.is_ok() {
                if receiver.borrow().is_none() { break; }
            }
        } => anyhow::bail!("data client disconnected; collector released its quote context"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_opening_snapshot_retries_without_freezing_daily_selection() {
        let scanner = ScannerConfig::default();
        let mut momentum = nautilus_trading::examples::strategies::slc_momentum::MomentumConfig {
            daily_only: true,
            refresh_minutes: 15,
            ..Default::default()
        };
        let at = UnixNanos::from(MINUTE);
        let close = UnixNanos::from(390 * MINUTE);
        assert_eq!(
            scanner.next_refresh(at, close, &momentum, false),
            UnixNanos::from(at.as_u64() + scanner.poll_seconds * 1_000_000_000)
        );
        assert_eq!(scanner.next_refresh(at, close, &momentum, true), close);
        momentum.daily_only = false;
        assert_eq!(
            scanner.next_refresh(at, close, &momentum, true),
            UnixNanos::from(16 * MINUTE)
        );
    }

    #[test]
    fn incomplete_duplicate_or_changing_universe_fails() {
        let mut rows = BTreeMap::new();
        let mut total = None;
        let page = |total, symbols: &[&str]| {
            ScreenerPage {
            total,
            items: symbols
                .iter()
                .map(|s| serde_json::json!({"counter_id": format!("ST/US/{}", s.strip_suffix(".US").unwrap()), "indicators": [{"key":"industry", "value":"Software"}]}))
                .collect(),
        }
        };
        assert!(!append_page(&mut rows, &mut total, page(3, &["A.US", "B.US"])).unwrap());
        assert!(append_page(&mut rows.clone(), &mut total, page(3, &["A.US"])).is_err());
        assert!(append_page(&mut rows.clone(), &mut total, page(4, &["C.US"])).is_err());
        assert!(append_page(&mut rows, &mut total, page(3, &["C.US"])).unwrap());
    }
}

/// Read-only pre-market download; reuses bounded per-symbol cache.
pub(super) async fn prefetch(path: &Path) -> anyhow::Result<()> {
    let app: App = serde_json::from_slice(&fs::read(path)?)?;
    app.strategy.validate()?;
    let sdk = LongbridgeDataClientConfig::default().sdk_config().await?;
    let (context, _receiver) = QuoteContext::new(sdk);
    let history = daily_history(&context, &app).await?;
    let observed = now();
    let mut validator = CrossSectionalMomentumRanker::default();
    let mut dates = BTreeMap::<InstrumentId, Vec<UnixNanos>>::new();
    for &bar in &history {
        validator.update_daily(bar, observed)?;
        dates
            .entry(bar.bar_type.instrument_id())
            .or_default()
            .push(bar.ts_event);
    }
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
    let expected = app
        .strategy
        .sessions
        .iter()
        .rev()
        .filter(|s| s.close < app.strategy.trading_start)
        .take(needed)
        .map(|s| s.close)
        .collect::<Vec<_>>();
    let missing = app
        .strategy
        .instrument_ids()
        .into_iter()
        .filter(|id| {
            dates.get(id).is_none_or(|dates| {
                dates.len() < needed
                    || dates
                        .iter()
                        .rev()
                        .take(needed)
                        .copied()
                        .ne(expected.iter().copied())
            })
        })
        .collect::<Vec<_>>();
    let quotes = observations(&context, &app).await?;
    let quote_symbols = quotes.iter().map(|q| q.symbol).collect::<BTreeSet<_>>();
    anyhow::ensure!(
        quote_symbols.len() == quotes.len(),
        "global quote response contains duplicate symbols"
    );
    anyhow::ensure!(
        quote_symbols
            .iter()
            .all(|id| app.strategy.universe.iter().any(|m| m.instrument_id == *id)),
        "unexpected global quote symbol"
    );
    fs::write(
        path.with_extension("history-audit.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "observed_at": now(), "normal_quote_symbols": quote_symbols.len(), "latest_quote_event": quotes.iter().map(|q| q.timestamp).max(), "daily_bars": history.len(), "cached_symbols": dates.len(), "required_sessions": needed,
            "missing_or_unaligned_history": missing, "daily_cutoff": expected.first(), "orders_submitted": 0,
            "note": "History readiness only; liquidity, live quote coverage, regime and SLC gates still apply."
        }))?,
    )?;
    println!(
        "Cached {} prior daily bars; no execution client opened",
        history.len()
    );
    Ok(())
}
