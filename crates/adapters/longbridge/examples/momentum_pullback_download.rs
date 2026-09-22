// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Downloads split-adjusted Longbridge Daily bars into the local momentum-pullback catalog.
//!
//! Longbridge limits a history response to 1,000 bars and the endpoint to 60 requests per 30
//! seconds. This importer uses three-year date chunks (well below 1,000 US trading days) and a
//! small pause between requests. Re-running skips a symbol when the requested catalog interval is
//! already present.

use std::{collections::BTreeSet, env, fs, path::Path, str::FromStr, time::Duration};

use anyhow::Context;
use jiff::Timestamp;
use longbridge::quote::{AdjustType, Period, QuoteContext, TradeSessions};
use nautilus_core::UnixNanos;
use nautilus_longbridge::{
    LongbridgeDataClientConfig,
    common::{
        parse::{instrument_id, parse_bar_with_price_precision, parse_instrument},
        rate_limit::quote_api_call_with_retry,
    },
};
use nautilus_model::{
    data::{Bar, BarType},
    identifiers::InstrumentId,
    types::Price,
};
use nautilus_persistence::backend::catalog::ParquetDataCatalog;
use serde::Deserialize;
use time::{Date, Month};

const DEFAULT_CONFIG_PATH: &str = "crates/backtest/examples/momentum_pullback.toml";
const PRICE_INCREMENT: &str = "0.01";
const PRICE_PRECISION: u8 = 2;
const CHUNK_DAYS: i64 = 1_095;
const REQUEST_PAUSE_MS: u64 = 550;
const MAX_RESPONSE_BARS: usize = 1_000;

#[derive(Debug, Deserialize)]
struct DownloadConfig {
    catalog_path: String,
    start: String,
    end: String,
    strategy: DownloadStrategyConfig,
}

#[derive(Debug, Deserialize)]
struct DownloadStrategyConfig {
    universe: Vec<InstrumentId>,
    market_regime_instrument_id: InstrumentId,
    secondary_market_instrument_id: InstrumentId,
    relative_strength_instrument_id: InstrumentId,
}

impl DownloadConfig {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading download config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid download config {}", path.display()))?;
        anyhow::ensure!(
            !config.catalog_path.trim().is_empty(),
            "catalog_path is required"
        );
        anyhow::ensure!(
            Timestamp::from_str(&config.start)? < Timestamp::from_str(&config.end)?,
            "start must be before end",
        );
        Ok(config)
    }

    fn instrument_ids(&self) -> anyhow::Result<Vec<InstrumentId>> {
        let ids: BTreeSet<_> = self
            .strategy
            .universe
            .iter()
            .copied()
            .chain([
                self.strategy.market_regime_instrument_id,
                self.strategy.secondary_market_instrument_id,
                self.strategy.relative_strength_instrument_id,
            ])
            .collect();
        anyhow::ensure!(!ids.is_empty(), "strategy universe must not be empty");
        for id in &ids {
            anyhow::ensure!(
                id.venue.as_str() == "LONGBRIDGE" && id.symbol.as_str().ends_with(".US"),
                "momentum-pullback downloader only supports US Longbridge instruments: {id}",
            );
        }
        Ok(ids.into_iter().collect())
    }
}

fn parse_date(value: &str) -> anyhow::Result<Date> {
    let date = value
        .get(..10)
        .context("timestamp must start with YYYY-MM-DD")?;
    let mut parts = date.split('-');
    let year = parts.next().context("missing year")?.parse()?;
    let month = parts.next().context("missing month")?.parse::<u8>()?;
    let day = parts.next().context("missing day")?.parse()?;
    anyhow::ensure!(parts.next().is_none(), "invalid date {date:?}");
    Ok(Date::from_calendar_date(
        year,
        Month::try_from(month)?,
        day,
    )?)
}

fn date_chunks(start: Date, end: Date) -> anyhow::Result<Vec<(Date, Date)>> {
    anyhow::ensure!(start <= end, "download start date must not exceed end date");
    let mut chunks = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let chunk_end = cursor
            .checked_add(time::Duration::days(CHUNK_DAYS - 1))
            .unwrap_or(end)
            .min(end);
        chunks.push((cursor, chunk_end));
        if chunk_end == end {
            break;
        }
        cursor = chunk_end
            .next_day()
            .context("historical date range exceeded supported calendar")?;
    }
    Ok(chunks)
}

fn requested_interval(config: &DownloadConfig) -> anyhow::Result<(UnixNanos, UnixNanos)> {
    Ok((
        Timestamp::from_str(&config.start)?.into(),
        Timestamp::from_str(&config.end)?.into(),
    ))
}

fn interval_is_cached(
    catalog: &ParquetDataCatalog,
    bar_type: BarType,
    requested: (UnixNanos, UnixNanos),
) -> anyhow::Result<bool> {
    Ok(catalog
        .get_intervals("bars", Some(&bar_type.to_string()))?
        .contains(&(requested.0.as_u64(), requested.1.as_u64())))
}

async fn download_bars(
    context: &QuoteContext,
    instrument_id: InstrumentId,
    chunks: &[(Date, Date)],
    requested: (UnixNanos, UnixNanos),
) -> anyhow::Result<Vec<Bar>> {
    let symbol = instrument_id.symbol.as_str();
    let bar_type = BarType::from(format!("{instrument_id}-1-DAY-LAST-EXTERNAL").as_str());
    let mut bars = Vec::new();
    for (start, end) in chunks {
        let candlesticks = quote_api_call_with_retry(|| {
            context.history_candlesticks_by_date(
                symbol,
                Period::Day,
                AdjustType::ForwardAdjust,
                Some(*start),
                Some(*end),
                TradeSessions::Intraday,
            )
        })
        .await
        .with_context(|| format!("failed downloading {symbol} Daily bars from {start} to {end}"))?;
        anyhow::ensure!(
            candlesticks.len() < MAX_RESPONSE_BARS,
            "Longbridge returned {MAX_RESPONSE_BARS} bars for {symbol} from {start} to {end}; shorten the chunk to avoid silent truncation",
        );
        bars.extend(
            candlesticks
                .into_iter()
                .map(|candle| {
                    parse_bar_with_price_precision(bar_type, candle, requested.0, PRICE_PRECISION)
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
        tokio::time::sleep(Duration::from_millis(REQUEST_PAUSE_MS)).await;
    }
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    bars.retain(|bar| bar.ts_event >= requested.0 && bar.ts_event <= requested.1);
    anyhow::ensure!(
        !bars.is_empty(),
        "Longbridge returned no Daily bars for {symbol}"
    );
    Ok(bars)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
    let config = DownloadConfig::load(Path::new(&config_path))?;
    let instrument_ids = config.instrument_ids()?;
    let requested = requested_interval(&config)?;
    let chunks = date_chunks(parse_date(&config.start)?, parse_date(&config.end)?)?;
    let price_increment = Price::from_str(PRICE_INCREMENT).map_err(anyhow::Error::msg)?;
    let data_config = LongbridgeDataClientConfig {
        instrument_price_increments: instrument_ids
            .iter()
            .map(|id| (id.to_string(), PRICE_INCREMENT.to_string()))
            .collect(),
        ..Default::default()
    };
    let sdk_config = data_config.sdk_config().await?;
    let (context, _receiver) = QuoteContext::new(sdk_config);
    let symbols = instrument_ids
        .iter()
        .map(|id| id.symbol.as_str().to_string())
        .collect::<Vec<_>>();
    let static_info = quote_api_call_with_retry(|| context.static_info(symbols.clone()))
        .await
        .context("failed downloading Longbridge instrument metadata")?;
    anyhow::ensure!(
        static_info.len() == instrument_ids.len(),
        "Longbridge returned metadata for {} of {} instruments",
        static_info.len(),
        instrument_ids.len(),
    );

    let instruments = static_info
        .iter()
        .map(|info| parse_instrument(info, price_increment, requested.0))
        .collect::<anyhow::Result<Vec<_>>>()?;
    fs::create_dir_all(&config.catalog_path).with_context(|| {
        format!(
            "failed creating local catalog directory {}",
            config.catalog_path
        )
    })?;
    let catalog = ParquetDataCatalog::from_uri(&config.catalog_path, None, None, None, None)?;
    catalog.write_instruments(instruments)?;

    println!(
        "Longbridge Daily download: symbols={}, chunks_per_symbol={}, catalog={}",
        instrument_ids.len(),
        chunks.len(),
        config.catalog_path,
    );
    for (index, id) in instrument_ids.into_iter().enumerate() {
        let bar_type = BarType::from(format!("{id}-1-DAY-LAST-EXTERNAL").as_str());
        if interval_is_cached(&catalog, bar_type, requested)? {
            println!("[{}/{}] {id}: cached", index + 1, symbols.len());
            continue;
        }
        let bars = download_bars(&context, id, &chunks, requested).await?;
        let first = bars
            .first()
            .map(|bar| bar.ts_event)
            .context("missing first bar")?;
        let last = bars
            .last()
            .map(|bar| bar.ts_event)
            .context("missing last bar")?;
        catalog.write_to_parquet(&bars, Some(requested.0), Some(requested.1), None)?;
        println!(
            "[{}/{}] {}: {} bars, {} through {}",
            index + 1,
            symbols.len(),
            instrument_id(id.symbol.as_str()),
            bars.len(),
            first,
            last,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_six_years_below_api_limit() {
        let chunks = date_chunks(
            Date::from_calendar_date(2021, Month::January, 1).unwrap(),
            Date::from_calendar_date(2026, Month::September, 12).unwrap(),
        )
        .unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|(start, end)| (*end - *start).whole_days() < CHUNK_DAYS)
        );
    }

    #[test]
    fn parses_config_timestamp_date_without_timezone_shift() {
        assert_eq!(
            parse_date("2021-01-01T00:00:00Z").unwrap(),
            Date::from_calendar_date(2021, Month::January, 1).unwrap(),
        );
    }
}
