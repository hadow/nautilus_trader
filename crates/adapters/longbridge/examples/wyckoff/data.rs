// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! 独立加载 Wyckoff 研究配置和 Longbridge 历史数据。

use std::{collections::BTreeSet, fs, path::Path, str::FromStr, time::Duration};

use anyhow::Context;
use jiff::{Timestamp, tz::TimeZone};
use longbridge::{
    Market,
    quote::{AdjustType, Candlestick, Period, QuoteContext, TradeSessions},
};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_longbridge::{
    LongbridgeDataClientConfig,
    common::{
        parse::{parse_bar_with_price_precision, parse_instrument},
        rate_limit::quote_api_call_with_retry,
    },
};
use nautilus_model::{
    data::{Bar, BarType},
    identifiers::InstrumentId,
    instruments::{Instrument, InstrumentAny},
    types::{Money, Price, Quantity},
};
use rust_decimal::Decimal;
use serde::Deserialize;
use time::{Date, Month};

const US_TIMEZONE: &str = "America/New_York";
const FIVE_MINUTE_NANOS: u64 = 5 * 60 * 1_000_000_000;
const HISTORY_CHUNK_DAYS: i64 = 7;
const TRADING_DAYS_CHUNK_DAYS: i64 = 28;

#[derive(Clone, Debug)]
pub(crate) struct ResearchConfig {
    pub(crate) oauth_client_id: String,
    pub(crate) oauth_callback_port: u16,
    pub(crate) download_start: Timestamp,
    pub(crate) in_sample_start: Timestamp,
    pub(crate) validation_start: Timestamp,
    pub(crate) out_of_sample_start: Timestamp,
    pub(crate) end: Timestamp,
    pub(crate) timeout_secs: u64,
    pub(crate) minimum_trades: usize,
    pub(crate) starting_balance: Money,
    pub(crate) risk_amount: Decimal,
    pub(crate) daily_loss_limit: Decimal,
    pub(crate) max_open_positions: usize,
    pub(crate) max_trades_per_day: usize,
    pub(crate) max_trades_per_symbol: usize,
    pub(crate) max_order_quantity: Quantity,
    pub(crate) max_order_notional: Decimal,
    pub(crate) entry_start_minute: u16,
    pub(crate) entry_end_minute: u16,
    pub(crate) flatten_minute: u16,
    pub(crate) stop_buffer_atr: f64,
    pub(crate) spread_bps: Decimal,
    pub(crate) slippage_bps: Decimal,
    pub(crate) round_trip_cost_per_share: Decimal,
    pub(crate) symbols: Vec<ResearchInstrument>,
    pub(crate) timezone: TimeZone,
}

impl ResearchConfig {
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read Wyckoff config {}", path.display()))?;
        let file: ResearchFile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse Wyckoff config {}", path.display()))?;
        let config = Self {
            oauth_client_id: nonempty(
                "longbridge.oauth_client_id",
                &file.longbridge.oauth_client_id,
            )?,
            oauth_callback_port: file.longbridge.oauth_callback_port,
            download_start: parse("study.download_start", &file.study.download_start)?,
            in_sample_start: parse("study.in_sample_start", &file.study.in_sample_start)?,
            validation_start: parse("study.validation_start", &file.study.validation_start)?,
            out_of_sample_start: parse(
                "study.out_of_sample_start",
                &file.study.out_of_sample_start,
            )?,
            end: parse("study.end", &file.study.end)?,
            timeout_secs: file.study.timeout_secs,
            minimum_trades: file.study.minimum_trades,
            starting_balance: parse(
                "execution.starting_balance",
                &file.execution.starting_balance,
            )?,
            risk_amount: parse("execution.risk_amount", &file.execution.risk_amount)?,
            daily_loss_limit: parse(
                "execution.daily_loss_limit",
                &file.execution.daily_loss_limit,
            )?,
            max_open_positions: file.execution.max_open_positions,
            max_trades_per_day: file.execution.max_trades_per_day,
            max_trades_per_symbol: file.execution.max_trades_per_symbol,
            max_order_quantity: parse(
                "execution.max_order_quantity",
                &file.execution.max_order_quantity,
            )?,
            max_order_notional: parse(
                "execution.max_order_notional",
                &file.execution.max_order_notional,
            )?,
            entry_start_minute: parse_clock("execution.entry_start", &file.execution.entry_start)?,
            entry_end_minute: parse_clock("execution.entry_end", &file.execution.entry_end)?,
            flatten_minute: parse_clock("execution.flatten_time", &file.execution.flatten_time)?,
            stop_buffer_atr: file.execution.stop_buffer_atr,
            spread_bps: parse("execution.spread_bps", &file.execution.spread_bps)?,
            slippage_bps: parse("execution.slippage_bps", &file.execution.slippage_bps)?,
            round_trip_cost_per_share: parse(
                "execution.round_trip_cost_per_share",
                &file.execution.round_trip_cost_per_share,
            )?,
            symbols: file
                .symbols
                .into_iter()
                .map(ResearchInstrument::try_from)
                .collect::<anyhow::Result<_>>()?,
            timezone: get_timezone(US_TIMEZONE)?,
        };
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            oauth_client_id: Some(self.oauth_client_id.clone()),
            oauth_callback_port: self.oauth_callback_port,
            instrument_price_increments: self
                .symbols
                .iter()
                .map(|instrument| {
                    (
                        instrument.instrument_id.to_string(),
                        instrument.price_increment.to_string(),
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.oauth_callback_port > 0,
            "OAuth callback port must be positive"
        );
        anyhow::ensure!(
            self.download_start < self.in_sample_start
                && self.in_sample_start < self.validation_start
                && self.validation_start < self.out_of_sample_start
                && self.out_of_sample_start < self.end,
            "study timestamps must satisfy download < IS < validation < OOS < end",
        );
        anyhow::ensure!(self.timeout_secs > 0, "study timeout must be positive");
        anyhow::ensure!(self.minimum_trades > 0, "minimum trades must be positive");
        anyhow::ensure!(
            Money::is_positive(&self.starting_balance),
            "starting balance must be positive"
        );
        anyhow::ensure!(
            self.risk_amount > Decimal::ZERO,
            "risk amount must be positive"
        );
        anyhow::ensure!(
            self.daily_loss_limit >= self.risk_amount,
            "daily loss limit must be at least one planned risk",
        );
        anyhow::ensure!(
            self.max_open_positions > 0,
            "maximum open positions must be positive"
        );
        anyhow::ensure!(
            self.max_trades_per_day > 0,
            "maximum daily trades must be positive"
        );
        anyhow::ensure!(
            self.max_trades_per_symbol > 0 && self.max_trades_per_symbol <= self.max_trades_per_day,
            "maximum symbol trades must be positive and no greater than account daily trades",
        );
        anyhow::ensure!(
            Quantity::is_positive(&self.max_order_quantity),
            "maximum quantity must be positive"
        );
        anyhow::ensure!(
            self.max_order_notional > Decimal::ZERO,
            "maximum notional must be positive"
        );
        anyhow::ensure!(
            9 * 60 + 30 <= self.entry_start_minute
                && self.entry_start_minute < self.entry_end_minute
                && self.entry_end_minute < self.flatten_minute
                && self.flatten_minute <= 16 * 60,
            "entry and flatten times must form a valid regular US session",
        );
        anyhow::ensure!(
            self.stop_buffer_atr.is_finite() && (0.0..=1.0).contains(&self.stop_buffer_atr),
            "stop ATR buffer must be finite and between 0 and 1",
        );
        anyhow::ensure!(
            self.spread_bps >= Decimal::ZERO
                && self.slippage_bps >= Decimal::ZERO
                && self.round_trip_cost_per_share >= Decimal::ZERO,
            "execution costs must be non-negative",
        );
        anyhow::ensure!(!self.symbols.is_empty(), "at least one symbol is required");
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResearchInstrument {
    pub(crate) instrument_id: InstrumentId,
    pub(crate) price_increment: Price,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedSymbol {
    pub(crate) configured: ResearchInstrument,
    pub(crate) instrument: InstrumentAny,
    pub(crate) bars: Vec<Bar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResearchFile {
    longbridge: LongbridgeSettings,
    study: StudySettings,
    execution: ExecutionSettings,
    symbols: Vec<SymbolSettings>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongbridgeSettings {
    oauth_client_id: String,
    oauth_callback_port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StudySettings {
    download_start: String,
    in_sample_start: String,
    validation_start: String,
    out_of_sample_start: String,
    end: String,
    timeout_secs: u64,
    minimum_trades: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionSettings {
    starting_balance: String,
    risk_amount: String,
    daily_loss_limit: String,
    max_open_positions: usize,
    max_trades_per_day: usize,
    max_trades_per_symbol: usize,
    max_order_quantity: String,
    max_order_notional: String,
    entry_start: String,
    entry_end: String,
    flatten_time: String,
    stop_buffer_atr: f64,
    spread_bps: String,
    slippage_bps: String,
    round_trip_cost_per_share: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolSettings {
    symbol: String,
    price_increment: String,
}

impl TryFrom<SymbolSettings> for ResearchInstrument {
    type Error = anyhow::Error;

    fn try_from(value: SymbolSettings) -> Result<Self, Self::Error> {
        let symbol = value.symbol.trim();
        anyhow::ensure!(!symbol.is_empty(), "symbol must not be empty");
        let instrument_id = InstrumentId::from(format!("{symbol}.LONGBRIDGE").as_str());
        let price_increment = parse("symbols.price_increment", &value.price_increment)?;
        anyhow::ensure!(
            Price::is_positive(&price_increment),
            "price increment must be positive"
        );
        Ok(Self {
            instrument_id,
            price_increment,
        })
    }
}

/// 在总超时内下载全部标的的静态定义和未复权 RTH 5 分钟 Bar
pub(crate) async fn prepare_data(config: &ResearchConfig) -> anyhow::Result<Vec<PreparedSymbol>> {
    tokio::time::timeout(Duration::from_secs(config.timeout_secs), async {
        let sdk_config = config.data_config().sdk_config().await?;
        let (context, _receiver) = QuoteContext::new(sdk_config);
        let half_days = load_half_days(&context, config.download_start, config.end).await?;
        let symbols = config
            .symbols
            .iter()
            .map(|instrument| instrument.instrument_id.symbol.as_str())
            .collect::<Vec<_>>();
        let static_info = quote_api_call_with_retry(|| context.static_info(symbols.clone()))
            .await
            .context("failed to request Longbridge static security info")?;
        let mut static_info = static_info
            .into_iter()
            .map(|info| (info.symbol.clone(), info))
            .collect::<std::collections::HashMap<_, _>>();
        let mut prepared = Vec::with_capacity(config.symbols.len());
        for (index, configured) in config.symbols.iter().copied().enumerate() {
            let symbol = configured.instrument_id.symbol.as_str();
            println!(
                "[{}/{}] [{symbol}] downloading causal 5m research data",
                index + 1,
                config.symbols.len(),
            );
            let info = static_info
                .remove(symbol)
                .with_context(|| format!("Longbridge returned no static info for {symbol}"))?;
            let instrument =
                parse_instrument(&info, configured.price_increment, UnixNanos::default())?;
            let bars = download_bars(
                &context,
                symbol,
                bar_type(configured.instrument_id),
                config.download_start,
                config.end,
                instrument.price_precision(),
                &half_days,
            )
            .await?;
            println!("[{symbol}] downloaded {} completed RTH bars", bars.len());
            prepared.push(PreparedSymbol {
                configured,
                instrument,
                bars,
            });
        }
        Ok(prepared)
    })
    .await
    .with_context(|| {
        format!(
            "Wyckoff data download exceeded {} seconds",
            config.timeout_secs
        )
    })?
}

pub(crate) fn bar_type(instrument_id: InstrumentId) -> BarType {
    BarType::from(format!("{instrument_id}-5-MINUTE-LAST-EXTERNAL").as_str())
}

async fn download_bars(
    context: &QuoteContext,
    symbol: &str,
    bar_type: BarType,
    start: Timestamp,
    end: Timestamp,
    price_precision: u8,
    half_days: &BTreeSet<Date>,
) -> anyhow::Result<Vec<Bar>> {
    let mut candlesticks = Vec::new();
    let mut cursor = us_market_date(start)?;
    let end_date = us_market_date(end)?;
    while cursor <= end_date {
        let chunk_end = cursor
            .checked_add(time::Duration::days(HISTORY_CHUNK_DAYS - 1))
            .unwrap_or(end_date)
            .min(end_date);
        candlesticks.extend(
            quote_api_call_with_retry(|| {
                context.history_candlesticks_by_date(
                    symbol,
                    Period::FiveMinute,
                    AdjustType::NoAdjust,
                    Some(cursor),
                    Some(chunk_end),
                    TradeSessions::Intraday,
                )
            })
            .await
            .with_context(|| {
                format!("failed to download {symbol} from {cursor} through {chunk_end}")
            })?,
        );
        if chunk_end == end_date {
            break;
        }
        cursor = chunk_end
            .next_day()
            .context("historical date range exceeded the supported calendar")?;
    }
    parse_bars(
        symbol,
        bar_type,
        candlesticks,
        start,
        end,
        price_precision,
        half_days,
    )
}

fn parse_bars(
    symbol: &str,
    bar_type: BarType,
    candlesticks: Vec<Candlestick>,
    start: Timestamp,
    end: Timestamp,
    price_precision: u8,
    half_days: &BTreeSet<Date>,
) -> anyhow::Result<Vec<Bar>> {
    let start = UnixNanos::from(start);
    let end = UnixNanos::from(end);
    let mut bars = candlesticks
        .into_iter()
        .map(|candlestick| {
            parse_bar_with_price_precision(
                bar_type,
                candlestick,
                UnixNanos::default(),
                price_precision,
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    let mut completed = Vec::with_capacity(bars.len());
    for mut bar in bars {
        let Some(close) = bar.ts_event.checked_add(FIVE_MINUTE_NANOS) else {
            continue;
        };
        if bar.ts_event < start || close > end {
            continue;
        }
        let timestamp = Timestamp::from_nanosecond(i128::from(bar.ts_event.as_u64()))?;
        if half_days.contains(&us_market_date(timestamp)?) {
            continue;
        }
        bar.ts_init = close;
        completed.push(bar);
    }
    anyhow::ensure!(
        !completed.is_empty(),
        "Longbridge returned no completed RTH bars for {symbol}: start={start}, end={end}",
    );
    Ok(completed)
}

async fn load_half_days(
    context: &QuoteContext,
    start: Timestamp,
    end: Timestamp,
) -> anyhow::Result<BTreeSet<Date>> {
    let mut half_days = BTreeSet::new();
    let mut cursor = us_market_date(start)?;
    let end_date = us_market_date(end)?;
    while cursor <= end_date {
        let chunk_end = cursor
            .checked_add(time::Duration::days(TRADING_DAYS_CHUNK_DAYS - 1))
            .unwrap_or(end_date)
            .min(end_date);
        half_days.extend(
            quote_api_call_with_retry(|| context.trading_days(Market::US, cursor, chunk_end))
                .await
                .with_context(|| {
                    format!(
                        "failed to query US half trading days from {cursor} through {chunk_end}"
                    )
                })?
                .half_trading_days,
        );
        if chunk_end == end_date {
            break;
        }
        cursor = chunk_end
            .next_day()
            .context("trading-day date range exceeded the supported calendar")?;
    }
    Ok(half_days)
}

fn us_market_date(timestamp: Timestamp) -> anyhow::Result<Date> {
    let local = timestamp.to_zoned(get_timezone(US_TIMEZONE)?);
    Date::from_calendar_date(
        i32::from(local.year()),
        Month::try_from(u8::try_from(local.month())?)?,
        u8::try_from(local.day())?,
    )
    .map_err(Into::into)
}

fn parse<T>(name: &str, value: &str) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid {name}={value:?}: {e}"))
}

fn nonempty(name: &str, value: &str) -> anyhow::Result<String> {
    let value = value.trim().to_string();
    anyhow::ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value)
}

fn parse_clock(name: &str, value: &str) -> anyhow::Result<u16> {
    let (hour, minute) = value
        .split_once(':')
        .with_context(|| format!("invalid {name}={value:?}, expected HH:MM"))?;
    let hour: u16 = parse(name, hour)?;
    let minute: u16 = parse(name, minute)?;
    anyhow::ensure!(hour < 24 && minute < 60, "invalid {name}={value:?}");
    Ok(hour * 60 + minute)
}
