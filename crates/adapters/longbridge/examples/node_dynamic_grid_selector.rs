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

//! 只读网格候选研究入口：先保存可审计行情快照，再离线排名；不构建交易客户端。

mod intraday_calendar;

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use jiff::Timestamp;
use longbridge::quote::{
    AdjustType, Candlestick, Period, QuoteContext, SecurityBoard, TradeStatus,
};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_longbridge::{
    common::{
        parse::{instrument_id, parse_bar, parse_depth, period_from_bar_type, unix_nanos},
        rate_limit::quote_api_call_with_retry,
    },
    config::LongbridgeDataClientConfig,
};
use nautilus_model::{
    data::{Bar, BarType},
    enums::BarAggregation,
};
use nautilus_trading::examples::strategies::{
    IntradayMomentumSession,
    dynamic_grid::{
        files::GridInstrumentFile,
        selection::{
            GridSelectionConfig, GridSelectionInput, GridSelectionReport, select_grid_candidates,
        },
    },
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolConfig {
    sector: String,
    /// 缺省时继承模板；路径相对选股配置文件，而非当前工作目录。
    grid_config: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectorFile {
    grid_template: PathBuf,
    signal_history_bars: usize,
    selection: GridSelectionConfig,
    universe: BTreeMap<String, SymbolConfig>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    source: String,
    adjustment: String,
    /// 完成整个采集的观测时刻；每个 Bar/Quote 另有自己的时间戳。
    as_of_ns: u64,
    selection: GridSelectionConfig,
    inputs: Vec<GridSelectionInput>,
    collection_failures: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Report {
    source: String,
    adjustment: String,
    selection: GridSelectionConfig,
    collection_failures: BTreeMap<String, String>,
    result: GridSelectionReport,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    serde_json::from_reader(File::open(path).with_context(|| format!("open {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn write_new(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    // create_new 同时阻止覆盖配置、旧研究结果和交易检查点。
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create new output {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn now() -> UnixNanos {
    Timestamp::now().into()
}

fn market_date(timestamp: UnixNanos) -> anyhow::Result<jiff::civil::Date> {
    Ok(Timestamp::from_nanosecond(i128::from(timestamp.as_u64()))?
        .to_zoned(get_timezone("America/New_York")?)
        .date())
}

/// SDK Bar 标注起始时间，研究输入必须标注完成时间。日线保守滞后一交易日。
fn completed_bars(
    candles: Vec<Candlestick>,
    kind: BarType,
    sessions: &[IntradayMomentumSession],
    requested_at: UnixNanos,
) -> anyhow::Result<Vec<Bar>> {
    let today = market_date(requested_at)?;
    let interval_ns = match kind.spec().aggregation {
        BarAggregation::Day => None,
        BarAggregation::Minute => Some(kind.spec().step.get() as u64 * 60_000_000_000),
        BarAggregation::Hour => Some(kind.spec().step.get() as u64 * 3_600_000_000_000),
        _ => anyhow::bail!("Only regular-session daily/minute/hour bars supported"),
    };
    let mut bars = Vec::with_capacity(candles.len());
    for candle in candles {
        anyhow::ensure!(
            candle.trade_session == longbridge::quote::TradeSession::Intraday,
            "Non-regular-session candle in Intraday response"
        );
        let start = unix_nanos(candle.timestamp)?;
        let date = market_date(start)?;
        let session = sessions
            .iter()
            .find(|session| {
                market_date(session.open).is_ok_and(|session_date| session_date == date)
            })
            .context("Candle date missing from broker trading calendar")?;
        let end = if let Some(interval) = interval_ns {
            let end = start.checked_add(interval).context("Bar time overflow")?;
            anyhow::ensure!(
                start >= session.open && start < session.close,
                "Non-regular-session candle in Intraday response"
            );
            // 半日市或非整除周期的末根 Bar 在真实收市时完成。
            end.min(session.close)
        } else {
            if date >= today {
                continue;
            }
            session.close
        };
        if end > requested_at {
            continue;
        }
        let raw = parse_bar(kind, candle, requested_at)?;
        bars.push(Bar::new(
            kind,
            raw.open,
            raw.high,
            raw.low,
            raw.close,
            raw.volume,
            end,
            requested_at,
        ));
    }
    // 不自行去重或重排：异常供应商顺序交给核心校验并记录失败。
    Ok(bars)
}

async fn collect_symbol(
    context: &QuoteContext,
    symbol: &str,
    config: &SymbolConfig,
    template: GridInstrumentFile,
    sessions: &[IntradayMomentumSession],
    settings: &SelectorFile,
) -> anyhow::Result<GridSelectionInput> {
    let info = quote_api_call_with_retry(|| context.static_info([symbol])).await?;
    let info = info
        .iter()
        .find(|row| row.symbol == symbol)
        .context("Missing security metadata")?;
    anyhow::ensure!(
        info.board == SecurityBoard::USMain && info.currency == "USD",
        "Only USD US main-board stocks supported"
    );
    anyhow::ensure!(
        template.lot_size.as_decimal() == Decimal::from(info.lot_size),
        "Configured lot size differs from broker metadata"
    );
    let id = instrument_id(symbol);
    let signal_type: BarType =
        format!("{id}-{}-EXTERNAL", template.strategy.bar_type.spec()).parse()?;
    let daily_type: BarType = format!("{id}-1-DAY-LAST-EXTERNAL").parse()?;
    let requested_at = now();
    let daily = quote_api_call_with_retry(|| {
        context.candlesticks(
            symbol,
            Period::Day,
            settings.selection.lookback_sessions + 2,
            AdjustType::NoAdjust,
            longbridge::quote::TradeSessions::Intraday,
        )
    })
    .await?;
    let daily_bars = completed_bars(daily, daily_type, sessions, requested_at)?;
    let period = period_from_bar_type(signal_type)?;
    let requested_at = now();
    let signal = quote_api_call_with_retry(|| {
        context.candlesticks(
            symbol,
            period,
            settings.signal_history_bars,
            AdjustType::NoAdjust,
            longbridge::quote::TradeSessions::Intraday,
        )
    })
    .await?;
    let signal_bars = completed_bars(signal, signal_type, sessions, requested_at)?;
    let quotes = quote_api_call_with_retry(|| context.quote([symbol])).await?;
    let quote = quotes
        .iter()
        .find(|row| row.symbol == symbol)
        .context("Missing security quote")?;
    anyhow::ensure!(
        quote.trade_status == TradeStatus::Normal,
        "Non-normal trading status"
    );
    let depth = quote_api_call_with_retry(|| context.depth(symbol)).await?;
    // 深度拉取没有交易所时间戳：借最新成交时间做保守陈旧检查，绝不伪称是真实盘口时间。
    let (_, quote) = parse_depth(
        symbol,
        &depth.bids,
        &depth.asks,
        unix_nanos(quote.timestamp)?,
        now(),
    )?;
    Ok(GridSelectionInput {
        instrument_id: id,
        sector: config.sector.clone(),
        grid: template.strategy.grid,
        price_increment: template.price_increment,
        lot_size: template.lot_size,
        daily_bars,
        signal_bars,
        quote,
    })
}

async fn collect(path: &Path) -> anyhow::Result<Snapshot> {
    let settings: SelectorFile = read_json(path)?;
    settings.selection.validate()?;
    anyhow::ensure!(
        (100..=1000).contains(&settings.signal_history_bars),
        "Signal history must be 100–1000 bars"
    );
    anyhow::ensure!(
        !settings.universe.is_empty() && settings.universe.len() <= 100,
        "Explicit universe must contain 1–100 symbols"
    );
    let directory = path.parent().context("Missing config directory")?;
    let mut templates = BTreeMap::new();
    for (symbol, config) in &settings.universe {
        anyhow::ensure!(
            symbol.ends_with(".US") && !config.sector.trim().is_empty(),
            "US symbol and explicit sector required: {symbol}"
        );
        let source = directory.join(
            config
                .grid_config
                .as_ref()
                .unwrap_or(&settings.grid_template),
        );
        let template: GridInstrumentFile = read_json(&source)?;
        template.strategy.grid.validate()?;
        anyhow::ensure!(
            !template.strategy.confirmed_custom_bars,
            "Selector requires ordinary completed external bars"
        );
        period_from_bar_type(template.strategy.bar_type)?;
        templates.insert(symbol.clone(), template);
    }
    let sdk_config = LongbridgeDataClientConfig::default().sdk_config().await?;
    let (context, _receiver) = QuoteContext::new(sdk_config);
    let sessions = intraday_calendar::calendar(
        &context,
        Timestamp::now(),
        (settings.selection.lookback_sessions * 3 + 14) as i64,
    )
    .await?;
    let mut inputs = Vec::new();
    let mut collection_failures = BTreeMap::new();
    // ponytail: 小型显式股票池顺序采集；扩大到全市场时再做配额感知批处理。
    for (symbol, template) in templates {
        match collect_symbol(
            &context,
            &symbol,
            &settings.universe[&symbol],
            template,
            &sessions,
            &settings,
        )
        .await
        {
            Ok(input) => {
                println!(
                    "COLLECTED {symbol} daily={} signal={}",
                    input.daily_bars.len(),
                    input.signal_bars.len()
                );
                inputs.push(input);
            }
            Err(error) => {
                eprintln!("COLLECTION_FAILED {symbol}: {error:#}");
                collection_failures.insert(symbol, format!("{error:#}"));
            }
        }
    }
    Ok(Snapshot {
        schema_version: 1,
        source: "Longbridge regular-session candles; depth age proxied by last-trade timestamp"
            .into(),
        adjustment: "NoAdjust".into(),
        as_of_ns: now().as_u64(),
        selection: settings.selection,
        inputs,
        collection_failures,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] {
        println!(
            "Read-only grid candidate research (no orders):\n  --collect CONFIG.json NEW_SNAPSHOT.json\n  --rank SNAPSHOT.json NEW_REPORT.json"
        );
        return Ok(());
    }
    anyhow::ensure!(
        args.len() == 3,
        "Expected mode, input, new output; use --help"
    );
    let input = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    anyhow::ensure!(!output.exists(), "Output already exists; choose a new path");
    match args[0].as_str() {
        "--collect" => write_new(output, &collect(input).await?),
        "--rank" => {
            let snapshot: Snapshot = read_json(input)?;
            anyhow::ensure!(
                snapshot.schema_version == 1 && snapshot.adjustment == "NoAdjust",
                "Unsupported snapshot schema or adjustment"
            );
            let result =
                select_grid_candidates(&snapshot.selection, &snapshot.inputs, snapshot.as_of_ns)?;
            for row in &result.ranked {
                println!(
                    "{} selected={} score={:?} reasons={:?}",
                    row.instrument_id, row.selected, row.score, row.reasons
                );
            }
            write_new(
                output,
                &Report {
                    source: snapshot.source,
                    adjustment: snapshot.adjustment,
                    selection: snapshot.selection,
                    collection_failures: snapshot.collection_failures,
                    result,
                },
            )
        }
        _ => anyhow::bail!("Unknown mode; use --help"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_bar_is_not_available_at_its_open() {
        let sessions = vec![IntradayMomentumSession {
            open: "2026-09-22T13:30:00Z".parse::<Timestamp>().unwrap().into(),
            close: "2026-09-22T20:00:00Z".parse::<Timestamp>().unwrap().into(),
            dividend: Decimal::ZERO,
        }];
        let candle: Candlestick = serde_json::from_value(serde_json::json!({
            "close":"100", "open":"100", "low":"99", "high":"101", "volume":100,
            "turnover":"10000", "timestamp":"2026-09-22T13:30:00Z",
            "trade_session":"Intraday", "open_updated":true
        }))
        .unwrap();
        let kind: BarType = "AAPL.US.LONGBRIDGE-1-MINUTE-LAST-EXTERNAL".parse().unwrap();
        let start = sessions[0].open;
        assert!(
            completed_bars(vec![candle], kind, &sessions, start)
                .unwrap()
                .is_empty()
        );
        let end = start + 60_000_000_000;
        let result = completed_bars(vec![candle], kind, &sessions, end).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ts_event, end);
    }

    #[test]
    fn daily_bar_is_lagged_and_half_day_close_is_preserved() {
        let open: UnixNanos = "2026-11-27T14:30:00Z".parse::<Timestamp>().unwrap().into();
        let close: UnixNanos = "2026-11-27T18:00:00Z".parse::<Timestamp>().unwrap().into();
        let sessions = vec![IntradayMomentumSession {
            open,
            close,
            dividend: Decimal::ZERO,
        }];
        let candle: Candlestick = serde_json::from_value(serde_json::json!({
            "close":"100", "open":"100", "low":"99", "high":"101", "volume":100,
            "turnover":"10000", "timestamp":"2026-11-27T05:00:00Z",
            "trade_session":"Intraday", "open_updated":true
        }))
        .unwrap();
        let kind: BarType = "AAPL.US.LONGBRIDGE-1-DAY-LAST-EXTERNAL".parse().unwrap();
        assert!(
            completed_bars(vec![candle], kind, &sessions, close)
                .unwrap()
                .is_empty()
        );
        let next_day: UnixNanos = "2026-11-28T14:00:00Z".parse::<Timestamp>().unwrap().into();
        let result = completed_bars(vec![candle], kind, &sessions, next_day).unwrap();
        assert_eq!(result[0].ts_event, close);
    }
}
