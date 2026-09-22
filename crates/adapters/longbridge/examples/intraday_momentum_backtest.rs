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

//! Downloads Longbridge minute bars to a local catalog and replays them offline.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::Path,
    str::FromStr,
    time::Duration,
};

use anyhow::Context;
use jiff::{civil::Time as CivilTime, tz::TimeZone};
use longbridge::quote::{AdjustType, Period, QuoteContext, TradeSessions};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_execution::models::{
    fee::{FeeModelAny, MakerTakerFeeModel},
    fill::{FillModelAny, OneTickSlippageFillModel},
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig,
    common::{
        parse::{instrument_id, parse_bar_with_price_precision, parse_instrument},
        rate_limit::quote_api_call_with_retry,
    },
};
use nautilus_model::{
    data::{Bar, BarType, Data, QuoteTick, bar::BAR_SPEC_1_MINUTE_LAST},
    enums::{AccountType, AggregationSource, BookType, OmsType},
    identifiers::{InstrumentId, StrategyId},
    instruments::{Instrument, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_persistence::backend::catalog::ParquetDataCatalog;
use nautilus_trading::{
    examples::strategies::{
        IntradayMomentumConfig, IntradayMomentumReport, IntradayMomentumSession,
        IntradayMomentumStrategy,
    },
    strategy::StrategyConfig,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::{Date, Month};

const DEFAULT_CONFIG_PATH: &str =
    "crates/adapters/longbridge/examples/intraday_momentum_backtest.toml";
const MINUTE: u64 = 60_000_000_000;
const DOWNLOAD_CHUNK_DAYS: i64 = 2;
const REQUEST_PAUSE_MS: u64 = 550;
const US_TIMEZONE: &str = "America/New_York";
const USAGE: &str = "usage: longbridge-intraday-momentum-backtest <download|run> [CONFIG.toml]";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct AppConfig {
    catalog_path: String,
    output_path: String,
    download_start: String,
    backtest_start: String,
    backtest_end: String,
    starting_balance: String,
    spread_bps: Decimal,
    commission_rate: Decimal,
    slippage_probability: f64,
    random_seed: u64,
    calendar_instrument: InstrumentId,
    instruments: Vec<InstrumentId>,
    dividends: HashMap<String, HashMap<String, Decimal>>,
    instrument_price_increments: HashMap<String, String>,
    strategy: IntradayMomentumConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            catalog_path: "target/intraday_momentum_catalog".to_string(),
            output_path: "target/intraday_momentum_report.json".to_string(),
            download_start: "2025-01-01".to_string(),
            backtest_start: "2025-04-01".to_string(),
            backtest_end: "2026-09-17".to_string(),
            starting_balance: "100000 USD".to_string(),
            spread_bps: Decimal::ONE,
            commission_rate: Decimal::new(1, 4),
            slippage_probability: 1.0,
            random_seed: 42,
            calendar_instrument: InstrumentId::from("SPY.US.LONGBRIDGE"),
            instruments: vec![InstrumentId::from("SPY.US.LONGBRIDGE")],
            dividends: HashMap::new(),
            instrument_price_increments: HashMap::new(),
            strategy: IntradayMomentumConfig::default(),
        }
    }
}

impl AppConfig {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading backtest config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid backtest config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.catalog_path.trim().is_empty(),
            "catalog_path is required"
        );
        anyhow::ensure!(
            !self.output_path.trim().is_empty(),
            "output_path is required"
        );
        let download_start = self.download_start()?;
        let backtest_start = self.backtest_start()?;
        let backtest_end = self.backtest_end()?;
        anyhow::ensure!(
            download_start < backtest_start && backtest_start <= backtest_end,
            "dates must satisfy download_start < backtest_start <= backtest_end",
        );
        anyhow::ensure!(
            !self.instruments.is_empty(),
            "at least one instrument is required"
        );
        let unique = self.instruments.iter().copied().collect::<BTreeSet<_>>();
        anyhow::ensure!(
            unique.len() == self.instruments.len(),
            "instruments must be unique",
        );
        anyhow::ensure!(
            unique.contains(&self.calendar_instrument),
            "calendar_instrument must be included in instruments",
        );
        for instrument_id in &self.instruments {
            anyhow::ensure!(
                instrument_id.venue.as_str() == "LONGBRIDGE"
                    && instrument_id.symbol.as_str().ends_with(".US"),
                "backtest only supports US Longbridge instruments: {instrument_id}",
            );
            self.price_increment(*instrument_id)?;
        }
        let allocation =
            self.strategy.capital_fraction * Decimal::from(u64::try_from(self.instruments.len())?);
        anyhow::ensure!(
            allocation <= Decimal::ONE,
            "combined capital_fraction is {allocation}; it must not exceed 1",
        );
        anyhow::ensure!(
            self.spread_bps >= Decimal::ZERO,
            "spread_bps must be non-negative",
        );
        anyhow::ensure!(
            self.commission_rate >= Decimal::ZERO,
            "commission_rate must be non-negative",
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.slippage_probability),
            "slippage_probability must be in [0, 1]",
        );
        self.starting_balance_money()?;
        for (instrument_id, values) in &self.dividends {
            anyhow::ensure!(
                self.instruments
                    .iter()
                    .any(|configured| configured.to_string() == *instrument_id),
                "dividends configured for unknown instrument {instrument_id}",
            );
            for (date, amount) in values {
                parse_date(date)?;
                anyhow::ensure!(
                    *amount >= Decimal::ZERO,
                    "negative dividend for {instrument_id}"
                );
            }
        }

        let mut strategy = self.strategy.clone();
        strategy.instrument_id = self.instruments[0];
        strategy.sessions = vec![IntradayMomentumSession {
            open: UnixNanos::from(MINUTE),
            close: UnixNanos::from(391 * MINUTE),
            dividend: Decimal::ZERO,
        }];
        strategy.bars_are_final = true;
        strategy.validate()
    }

    fn download_start(&self) -> anyhow::Result<Date> {
        parse_date(&self.download_start)
    }

    fn backtest_start(&self) -> anyhow::Result<Date> {
        parse_date(&self.backtest_start)
    }

    fn backtest_end(&self) -> anyhow::Result<Date> {
        parse_date(&self.backtest_end)
    }

    fn price_increment(&self, instrument_id: InstrumentId) -> anyhow::Result<Price> {
        self.instrument_price_increments
            .get(&instrument_id.to_string())
            .with_context(|| format!("missing price increment for {instrument_id}"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid price increment for {instrument_id}: {e}"))
    }

    fn starting_balance_money(&self) -> anyhow::Result<Money> {
        let balance = Money::from_str(&self.starting_balance).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            balance.is_positive() && balance.currency == Currency::USD(),
            "starting_balance must be positive USD",
        );
        Ok(balance)
    }

    fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            instrument_price_increments: self.instrument_price_increments.clone(),
            ..Default::default()
        }
    }
}

fn parse_date(value: &str) -> anyhow::Result<Date> {
    let mut parts = value.split('-');
    let year = parts.next().context("missing year")?.parse()?;
    let month = parts.next().context("missing month")?.parse::<u8>()?;
    let day = parts.next().context("missing day")?.parse()?;
    anyhow::ensure!(parts.next().is_none(), "date must use YYYY-MM-DD: {value}");
    let date = Date::from_calendar_date(year, Month::try_from(month)?, day)?;
    anyhow::ensure!(
        date.to_string() == value,
        "date must use YYYY-MM-DD: {value}"
    );
    Ok(date)
}

fn date_chunks(start: Date, end: Date) -> anyhow::Result<Vec<(Date, Date)>> {
    anyhow::ensure!(start <= end, "download start date must not exceed end date");
    let mut chunks = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let chunk_end = cursor
            .checked_add(time::Duration::days(DOWNLOAD_CHUNK_DAYS - 1))
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

fn to_civil_date(date: Date) -> anyhow::Result<jiff::civil::Date> {
    Ok(jiff::civil::Date::new(
        i16::try_from(date.year())?,
        i8::try_from(u8::from(date.month()))?,
        i8::try_from(date.day())?,
    )?)
}

fn market_midnight(date: Date, timezone: &TimeZone) -> anyhow::Result<UnixNanos> {
    Ok(timezone
        .to_ambiguous_timestamp(to_civil_date(date)?.to_datetime(CivilTime::midnight()))
        .unambiguous()
        .context("New York midnight is ambiguous")?
        .into())
}

fn requested_interval(config: &AppConfig) -> anyhow::Result<(UnixNanos, UnixNanos)> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let start = market_midnight(config.download_start()?, &timezone)?;
    let next_day = config
        .backtest_end()?
        .next_day()
        .context("backtest end exceeds supported calendar")?;
    let end = market_midnight(next_day, &timezone)?
        .checked_sub(1_u64)
        .context("backtest interval underflow")?;
    Ok((start, end))
}

fn bar_type(instrument_id: InstrumentId) -> BarType {
    BarType::new(
        instrument_id,
        BAR_SPEC_1_MINUTE_LAST,
        AggregationSource::External,
    )
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
    precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let mut bars = Vec::new();
    for (start, end) in chunks {
        let rows = quote_api_call_with_retry(|| {
            context.history_candlesticks_by_date(
                instrument_id.symbol.as_str(),
                Period::OneMinute,
                AdjustType::NoAdjust,
                Some(*start),
                Some(*end),
                TradeSessions::Intraday,
            )
        })
        .await
        .map_err(|error| {
            let quota_exhausted = error.openapi_error_code() == Some(301607);
            let error = anyhow::Error::new(error);
            if quota_exhausted {
                error.context(
                    "Longbridge historical candlestick monthly unique-symbol quota is exhausted; wait for the next calendar-month reset or increase the account quota",
                )
            } else {
                error
            }
        })
        .with_context(|| {
            format!(
                "failed downloading {} minute bars from {start} through {end}",
                instrument_id.symbol,
            )
        })?;
        let chunk = rows
            .into_iter()
            .map(|row| {
                let raw = parse_bar_with_price_precision(
                    bar_type(instrument_id),
                    row,
                    requested.0,
                    precision,
                )?;
                let completed = raw
                    .ts_event
                    .checked_add(MINUTE)
                    .context("minute completion timestamp overflowed")?;
                Ok(Bar::new(
                    raw.bar_type,
                    raw.open,
                    raw.high,
                    raw.low,
                    raw.close,
                    raw.volume,
                    completed,
                    completed,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        bars.extend(chunk);
        tokio::time::sleep(Duration::from_millis(REQUEST_PAUSE_MS)).await;
    }
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    bars.retain(|bar| requested.0 <= bar.ts_event && bar.ts_event <= requested.1);
    anyhow::ensure!(
        !bars.is_empty(),
        "Longbridge returned no minute bars for {instrument_id}",
    );
    Ok(bars)
}

async fn download(config: &AppConfig) -> anyhow::Result<()> {
    fs::create_dir_all(&config.catalog_path).with_context(|| {
        format!(
            "failed creating local catalog directory {}",
            config.catalog_path
        )
    })?;
    let requested = requested_interval(config)?;
    let chunks = date_chunks(config.download_start()?, config.backtest_end()?)?;
    let data_config = config.data_config();
    data_config.validate()?;
    let (context, _receiver) = QuoteContext::new(data_config.sdk_config().await?);
    let symbols = config
        .instruments
        .iter()
        .map(|id| id.symbol.as_str().to_string())
        .collect::<Vec<_>>();
    let static_info = quote_api_call_with_retry(|| context.static_info(symbols.clone()))
        .await
        .context("failed downloading Longbridge instrument metadata")?;
    anyhow::ensure!(
        static_info.len() == config.instruments.len(),
        "Longbridge returned metadata for {} of {} instruments",
        static_info.len(),
        config.instruments.len(),
    );
    let instruments = static_info
        .iter()
        .map(|info| {
            let id = instrument_id(info.symbol.as_str());
            anyhow::ensure!(
                config.instruments.contains(&id),
                "Longbridge returned unexpected instrument {id}",
            );
            parse_instrument(info, config.price_increment(id)?, requested.0)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let catalog = ParquetDataCatalog::from_uri(&config.catalog_path, None, None, None, None)?;
    catalog.write_instruments(instruments)?;

    println!(
        "Intraday momentum download: symbols={}, chunks_per_symbol={}, catalog={}",
        config.instruments.len(),
        chunks.len(),
        config.catalog_path,
    );
    for (index, instrument_id) in config.instruments.iter().copied().enumerate() {
        let kind = bar_type(instrument_id);
        if interval_is_cached(&catalog, kind, requested)? {
            println!(
                "[{}/{}] {instrument_id}: cached",
                index + 1,
                config.instruments.len()
            );
            continue;
        }
        let increment = config.price_increment(instrument_id)?;
        let bars = download_bars(
            &context,
            instrument_id,
            &chunks,
            requested,
            increment.precision,
        )
        .await?;
        catalog.write_to_parquet(&bars, Some(requested.0), Some(requested.1), None)?;
        println!(
            "[{}/{}] {instrument_id}: {} bars, {} through {}",
            index + 1,
            config.instruments.len(),
            bars.len(),
            bars.first().expect("bars are non-empty").ts_event,
            bars.last().expect("bars are non-empty").ts_event,
        );
    }
    Ok(())
}

fn local_date(timestamp: UnixNanos, timezone: &TimeZone) -> anyhow::Result<Date> {
    let date = timestamp
        .to_datetime_utc()
        .to_zoned(timezone.clone())
        .date();
    Ok(Date::from_calendar_date(
        i32::from(date.year()),
        Month::try_from(u8::try_from(date.month())?)?,
        u8::try_from(date.day())?,
    )?)
}

fn infer_sessions(
    bars: &[Bar],
    timezone: &TimeZone,
) -> anyhow::Result<Vec<IntradayMomentumSession>> {
    let mut groups = BTreeMap::<Date, Vec<Bar>>::new();
    for bar in bars {
        groups
            .entry(local_date(bar.ts_event, timezone)?)
            .or_default()
            .push(*bar);
    }
    groups
        .into_iter()
        .map(|(date, mut bars)| {
            bars.sort_unstable_by_key(|bar| bar.ts_event);
            bars.dedup_by_key(|bar| bar.ts_event);
            let first = bars.first().context("session contains no bars")?.ts_event;
            let close = bars.last().context("session contains no bars")?.ts_event;
            let open = first
                .checked_sub(MINUTE)
                .context("session open underflowed")?;
            let duration = close.as_u64() - open.as_u64();
            anyhow::ensure!(
                duration == 390 * MINUTE || duration == 210 * MINUTE,
                "{date} has an invalid US regular-session duration",
            );
            anyhow::ensure!(
                bars.len() == usize::try_from(duration / MINUTE)?,
                "{date} has {} minute bars; expected {}",
                bars.len(),
                duration / MINUTE,
            );
            anyhow::ensure!(
                bars.windows(2)
                    .all(|pair| pair[1].ts_event.as_u64() == pair[0].ts_event.as_u64() + MINUTE),
                "{date} contains a minute-bar gap",
            );
            let local_open = open.to_datetime_utc().to_zoned(timezone.clone());
            let local_close = close.to_datetime_utc().to_zoned(timezone.clone());
            anyhow::ensure!(
                local_open.hour() == 9
                    && local_open.minute() == 30
                    && ((local_close.hour() == 16 && local_close.minute() == 0)
                        || (local_close.hour() == 13 && local_close.minute() == 0)),
                "{date} does not match a US regular session",
            );
            Ok(IntradayMomentumSession {
                open,
                close,
                dividend: Decimal::ZERO,
            })
        })
        .collect()
}

fn selected_sessions(
    sessions: &[IntradayMomentumSession],
    config: &AppConfig,
    timezone: &TimeZone,
) -> anyhow::Result<(Vec<IntradayMomentumSession>, UnixNanos, UnixNanos)> {
    let start = config.backtest_start()?;
    let end = config.backtest_end()?;
    let first = sessions
        .iter()
        .position(|session| local_date(session.open, timezone).is_ok_and(|date| date >= start))
        .context("catalog contains no session on or after backtest_start")?;
    let last = sessions
        .iter()
        .rposition(|session| local_date(session.open, timezone).is_ok_and(|date| date <= end))
        .context("catalog contains no session on or before backtest_end")?;
    anyhow::ensure!(first <= last, "backtest date range contains no sessions");
    let warmup_sessions = config.strategy.lookback_days + 1;
    anyhow::ensure!(
        first >= warmup_sessions,
        "catalog has {first} warmup sessions; need {warmup_sessions}",
    );
    Ok((
        sessions[first - warmup_sessions..=last].to_vec(),
        sessions[first].open,
        sessions[last].close,
    ))
}

fn validate_coverage(bars: &[Bar], sessions: &[IntradayMomentumSession]) -> anyhow::Result<()> {
    for session in sessions {
        let selected = bars
            .iter()
            .filter(|bar| session.open < bar.ts_event && bar.ts_event <= session.close)
            .collect::<Vec<_>>();
        let expected = usize::try_from((session.close.as_u64() - session.open.as_u64()) / MINUTE)?;
        anyhow::ensure!(
            selected.len() == expected
                && selected
                    .windows(2)
                    .all(|pair| pair[1].ts_event.as_u64() == pair[0].ts_event.as_u64() + MINUTE),
            "{} has incomplete minute coverage for session {}",
            bars.first().map_or_else(
                || "unknown instrument".to_string(),
                |bar| bar.instrument_id().to_string(),
            ),
            session.open,
        );
    }
    Ok(())
}

fn strategy_config(
    app: &AppConfig,
    index: usize,
    instrument_id: InstrumentId,
    sessions: &[IntradayMomentumSession],
    timezone: &TimeZone,
) -> anyhow::Result<IntradayMomentumConfig> {
    let dividends = app.dividends.get(&instrument_id.to_string());
    let sessions = sessions
        .iter()
        .map(|session| {
            let date = local_date(session.open, timezone)?.to_string();
            Ok(IntradayMomentumSession {
                dividend: dividends
                    .and_then(|values| values.get(&date))
                    .copied()
                    .unwrap_or_default(),
                ..*session
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut config = app.strategy.clone();
    config.instrument_id = instrument_id;
    config.sessions = sessions;
    config.bars_are_final = true;
    config.base = StrategyConfig {
        strategy_id: Some(StrategyId::from(format!(
            "INTRADAY-MOMENTUM-{:03}",
            index + 1,
        ))),
        order_id_tag: Some(format!("{:03}", 805 + index)),
        oms_type: Some(OmsType::Netting),
        external_order_claims: Some(vec![instrument_id]),
        market_exit_reduce_only: false,
        ..config.base
    };
    config.validate()?;
    Ok(config)
}

fn quote_after_bar(
    bar: Bar,
    instrument: &InstrumentAny,
    spread_bps: Decimal,
) -> anyhow::Result<QuoteTick> {
    let half_spread = spread_bps / Decimal::from(20_000);
    let bid = instrument
        .try_make_price_from_decimal(bar.close.as_decimal() * (Decimal::ONE - half_spread))?;
    let ask = instrument
        .try_make_price_from_decimal(bar.close.as_decimal() * (Decimal::ONE + half_spread))?
        .max(bid);
    let size = if bar.volume > Quantity::from(0) {
        bar.volume
    } else {
        Quantity::from(1)
    };
    let timestamp = bar
        .ts_event
        .checked_add(1_u64)
        .context("post-bar quote timestamp overflowed")?;
    Ok(QuoteTick::new(
        bar.instrument_id(),
        bid,
        ask,
        size,
        size,
        timestamp,
        timestamp,
    ))
}

fn run(config: &AppConfig) -> anyhow::Result<()> {
    let requested = requested_interval(config)?;
    let timezone = get_timezone(US_TIMEZONE)?;
    let mut catalog = ParquetDataCatalog::from_uri(&config.catalog_path, None, None, None, None)?;
    let identifiers = config
        .instruments
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut instruments = catalog.instruments(Some(&identifiers), None, Some(requested.1))?;
    instruments.sort_unstable_by_key(Instrument::id);
    instruments.dedup_by_key(|instrument| instrument.id());
    for instrument_id in &config.instruments {
        anyhow::ensure!(
            instruments
                .iter()
                .any(|instrument| instrument.id() == *instrument_id),
            "catalog is missing instrument metadata for {instrument_id}",
        );
    }
    for instrument in &mut instruments {
        let InstrumentAny::Equity(equity) = instrument else {
            anyhow::bail!(
                "intraday momentum only supports equities: {}",
                instrument.id()
            );
        };
        equity.maker_fee = config.commission_rate;
        equity.taker_fee = config.commission_rate;
    }
    let kinds = config
        .instruments
        .iter()
        .map(|id| bar_type(*id).to_string())
        .collect();
    let mut bars = catalog.query_typed_data::<Bar>(
        Some(kinds),
        Some(requested.0),
        Some(requested.1),
        None,
        None,
        true,
    )?;
    anyhow::ensure!(!bars.is_empty(), "catalog query returned no minute bars");
    bars.sort_unstable_by_key(|bar| (bar.instrument_id(), bar.ts_event));
    bars.dedup_by_key(|bar| (bar.instrument_id(), bar.ts_event));

    let calendar_bars = bars
        .iter()
        .copied()
        .filter(|bar| bar.instrument_id() == config.calendar_instrument)
        .collect::<Vec<_>>();
    let calendar = infer_sessions(&calendar_bars, &timezone)?;
    let (sessions, trading_start, trading_end) = selected_sessions(&calendar, config, &timezone)?;

    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    let fill_model = FillModelAny::OneTickSlippage(OneTickSlippageFillModel::new(
        1.0,
        config.slippage_probability,
        Some(config.random_seed),
    )?);
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(config.instruments[0].venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![config.starting_balance_money()?])
            .base_currency(Currency::USD())
            .default_leverage(config.strategy.max_leverage)
            .bar_execution(false)
            .trade_execution(false)
            .fill_model(fill_model.into())
            .fee_model(FeeModelAny::MakerTaker(MakerTakerFeeModel).into())
            .build()?,
    )?;
    for instrument in &instruments {
        engine.add_instrument(instrument)?;
    }

    let instruments_by_id = instruments
        .iter()
        .map(|instrument| (instrument.id(), instrument))
        .collect::<HashMap<_, _>>();
    let mut events = Vec::new();
    let mut reports = Vec::new();
    let mut warmup_bars = 0_usize;
    for (index, instrument_id) in config.instruments.iter().copied().enumerate() {
        let instrument_bars = bars
            .iter()
            .copied()
            .filter(|bar| bar.instrument_id() == instrument_id)
            .collect::<Vec<_>>();
        validate_coverage(&instrument_bars, &sessions)?;
        let history = instrument_bars
            .iter()
            .copied()
            .filter(|bar| bar.ts_event < trading_start)
            .collect::<Vec<_>>();
        let replay = instrument_bars
            .iter()
            .copied()
            .filter(|bar| trading_start < bar.ts_event && bar.ts_event <= trading_end)
            .collect::<Vec<_>>();
        warmup_bars += history.len();
        let strategy_config = strategy_config(config, index, instrument_id, &sessions, &timezone)?;
        let mut strategy = IntradayMomentumStrategy::new(strategy_config)?;
        reports.push((instrument_id, strategy.report_handle()));
        strategy.warmup(history)?;
        engine.add_strategy(strategy)?;
        let instrument = instruments_by_id[&instrument_id];
        for bar in replay {
            events.push(Data::Bar(bar));
            events.push(Data::Quote(quote_after_bar(
                bar,
                instrument,
                config.spread_bps,
            )?));
        }
    }
    let replay_events = events.len();
    engine.add_data(events, None, true, true)?;
    engine.run(None, None, None, false)?;

    let remaining_positions = engine
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
    let reports = reports
        .into_iter()
        .map(|(instrument_id, report)| {
            let report = report
                .lock()
                .map_err(|_| anyhow::anyhow!("intraday momentum report lock poisoned"))?
                .clone();
            Ok((instrument_id.to_string(), report))
        })
        .collect::<anyhow::Result<BTreeMap<String, IntradayMomentumReport>>>()?;
    let result = engine.get_result();
    let output = serde_json::json!({
        "configuration": config,
        "warmup_bars": warmup_bars,
        "replay_events": replay_events,
        "remaining_positions": remaining_positions,
        "remaining_orders": remaining_orders,
        "result": result,
        "strategy_reports": reports,
    });
    if let Some(parent) = Path::new(&config.output_path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config.output_path, serde_json::to_vec_pretty(&output)?)?;
    anyhow::ensure!(
        remaining_positions == 0 && remaining_orders == 0,
        "backtest ended with open positions or orders",
    );
    anyhow::ensure!(
        reports.values().all(|report| report.errors.is_empty()),
        "strategy halted; inspect strategy_reports in {}",
        config.output_path,
    );
    println!(
        "Intraday momentum backtest complete: warmup_bars={warmup_bars}, replay_events={replay_events}, orders={}, positions={}, report={}",
        result.total_orders, result.total_positions, config.output_path,
    );
    println!("PnL statistics: {:?}", result.stats_pnls);
    println!("Return statistics: {:?}", result.stats_returns);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!((1..=2).contains(&args.len()), "{USAGE}");
    let path = args
        .get(1)
        .map_or_else(|| Path::new(DEFAULT_CONFIG_PATH), Path::new);
    let config = AppConfig::load(path)?;
    match args[0].as_str() {
        "download" => download(&config).await,
        "run" => run(&config),
        _ => anyhow::bail!(USAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_download_chunks_never_exceed_two_calendar_days() {
        let chunks = date_chunks(
            Date::from_calendar_date(2026, Month::September, 1).unwrap(),
            Date::from_calendar_date(2026, Month::September, 6).unwrap(),
        )
        .unwrap();

        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|(start, end)| (*end - *start).whole_days() <= 1)
        );
    }

    #[test]
    fn example_configuration_reserves_warmup_before_backtest() {
        let config: AppConfig =
            toml::from_str(include_str!("intraday_momentum_backtest.toml")).unwrap();

        config.validate().unwrap();
        assert!(config.download_start().unwrap() < config.backtest_start().unwrap());
        assert_eq!(config.instruments.len(), 2);
        assert_eq!(
            config.strategy.relative_volume_threshold,
            Some(Decimal::ONE)
        );
        assert_eq!(config.strategy.target_daily_volatility, 0.03);
        assert_eq!(config.strategy.capital_fraction, Decimal::new(15, 2));
    }

    #[test]
    fn coverage_rejects_a_missing_minute() {
        let instrument_id = InstrumentId::from("SPY.US.LONGBRIDGE");
        let session = IntradayMomentumSession {
            open: UnixNanos::from(1_000 * MINUTE),
            close: UnixNanos::from(1_003 * MINUTE),
            dividend: Decimal::ZERO,
        };
        let bars = [1_u64, 3]
            .map(|minute| {
                let timestamp = UnixNanos::from(session.open.as_u64() + minute * MINUTE);
                Bar::new(
                    bar_type(instrument_id),
                    Price::from("100.00"),
                    Price::from("101.00"),
                    Price::from("99.00"),
                    Price::from("100.50"),
                    Quantity::from(1_000),
                    timestamp,
                    timestamp,
                )
            })
            .to_vec();

        assert!(validate_coverage(&bars, &[session]).is_err());
    }
}
