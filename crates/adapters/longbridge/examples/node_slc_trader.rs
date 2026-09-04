// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
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

//! Runs a bar-signal Structure-Level-Confirmation strategy with Longbridge stocks.
//!
//! WARNING: This example submits orders. It defaults to Longbridge paper trading. Setting
//! `LONGBRIDGE_SLC_PAPERTRADING=false` routes orders to a live margin account and additionally
//! requires `LONGBRIDGE_SLC_LIVE_ACK=I_UNDERSTAND_LIVE_ORDERS`.
//!
//! The strategy trades completed five-minute bars during the US regular session. It combines
//! confirmed four-hour higher-high/higher-low or lower-high/lower-low structure, fresh supply or
//! demand zones formed before ATR-sized displacement candles, one-break reclaim/retest levels, and
//! configurable Stochastics re-entry from the 20/80 bands. Each signal submits a one-bar marketable
//! limit entry sized at its worst allowed price. Every fill receives a Longbridge-compatible
//! market-if-touched stop, while the 2R target is recalculated from average fill price and checked
//! against executable top-of-book quotes. Completed bars provide a conservative fallback trigger.
//! Per-symbol strategies share a persisted SLC-owned risk ledger. Managed stop coordinates
//! cancellation and position close before the regular session ends.
//!
//! The strategy has no guaranteed profitability. Use an isolated paper account first and inspect
//! the broker account after every shutdown.
//!
//! Run with:
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-slc-trader`
//!
//! Required environment variable:
//! - `LONGBRIDGE_OAUTH_CLIENT_ID`: OAuth 2.0 public client ID.
//!
//! Configure one or more US equities in `examples/slc_symbols.toml`. Override its location with
//! `LONGBRIDGE_SLC_CONFIG_PATH`.

use std::{
    collections::{HashMap, VecDeque},
    env,
    fmt::{Debug, Display},
    fs,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use jiff::{Timestamp, civil::Time as CivilTime, tz::TimeZone};
use longbridge::{
    Market,
    quote::{AdjustType, Period, QuoteContext, TradeSession, TradeSessions},
};
use nautilus_common::{actor::DataActor, enums::Environment, live::get_runtime};
use nautilus_core::{
    UnixNanos,
    datetime::get_timezone,
    serialization::{deserialize_decimal_from_str, serialize_decimal_as_str},
};
use nautilus_indicators::{
    average::MovingAverageType,
    indicator::Indicator,
    momentum::stochastics::{Stochastics, StochasticsDMethod},
    volatility::atr::AverageTrueRange,
};
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{
        parse::{parse_bar, parse_instrument},
        rate_limit::{MAX_QUOTE_SUBSCRIPTION_SYMBOLS, quote_api_call},
    },
};
use nautilus_model::{
    data::{Bar, BarType, QuoteTick},
    enums::{AccountType, AggregationSource, BarAggregation, OrderSide, PriceType, TimeInForce},
    events::{
        OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired, OrderFilled, OrderRejected,
        PositionClosed,
    },
    identifiers::{AccountId, ClientOrderId, InstrumentId, StrategyId, TraderId},
    instruments::{Instrument, InstrumentAny},
    orders::Order,
    types::{Currency, Price, Quantity},
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::{Date, Month, Time};
use ustr::Ustr;

const TRADER_ID: &str = "SLC-TRADER-001";
const ACCOUNT_ID: &str = "LONGBRIDGE-001";
const NODE_NAME: &str = "LONGBRIDGE-SLC-001";
const STRATEGY_ID: &str = "SLC-001";
const ORDER_ID_TAG: &str = "201";
const US_TIMEZONE: &str = "America/New_York";
const RTH_OPEN_MINUTE: u16 = 9 * 60 + 30;
const RTH_CLOSE_MINUTE: u16 = 16 * 60;
const FIVE_MINUTES: u16 = 5;
const FIVE_MINUTE_NANOS: u64 = 5 * 60 * 1_000_000_000;
const MAX_WARMUP_AGE_NANOS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;
const MAX_WARMUP_BARS: usize = 1_000;
const LIVE_ACK: &str = "I_UNDERSTAND_LIVE_ORDERS";
const DEFAULT_SYMBOL_CONFIG_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/examples/slc_symbols.toml");
const DEFAULT_PAPER_RISK_STATE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../target/longbridge-slc-paper-risk-state.toml",
);
const DEFAULT_LIVE_RISK_STATE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../target/longbridge-slc-live-risk-state.toml",
);

#[derive(Clone, Copy, Debug)]
struct SessionRules {
    entry_start_minute: u16,
    entry_end_minute: u16,
    flatten_before_close_minutes: u16,
    max_trades_per_day: usize,
}

impl SessionRules {
    /// Validates that entry and flattening rules stay inside the US regular session.
    fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.entry_start_minute >= RTH_OPEN_MINUTE,
            "entry window must start no earlier than 09:30 New York time",
        );
        anyhow::ensure!(
            self.entry_start_minute < self.entry_end_minute
                && self.entry_end_minute <= RTH_CLOSE_MINUTE,
            "entry window must be non-empty and end by 16:00 New York time",
        );
        anyhow::ensure!(
            self.flatten_before_close_minutes > 0,
            "flatten-before-close minutes must be positive",
        );
        anyhow::ensure!(
            self.max_trades_per_day > 0,
            "max trades per day must be positive",
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct AppConfig {
    instruments: Vec<SlcInstrument>,
    papertrading: bool,
    risk_amount: Decimal,
    daily_loss_limit: Decimal,
    max_open_risk: Decimal,
    max_account_notional: Decimal,
    max_open_positions: usize,
    max_order_quantity: Quantity,
    max_order_notional: Decimal,
    max_entry_slippage_ticks: u64,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
    atr_period: usize,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    pivot_span: usize,
    zone_ttl_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    stochastic_k_period: usize,
    stochastic_k_smoothing: usize,
    stochastic_d_period: usize,
    oversold: f64,
    overbought: f64,
    five_minute_warmup: usize,
    four_hour_warmup: usize,
    risk_state_path: PathBuf,
    timezone: TimeZone,
    session: SessionRules,
}

#[derive(Clone, Copy, Debug)]
struct SlcInstrument {
    instrument_id: InstrumentId,
    price_increment: Price,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolConfig {
    symbols: Vec<SymbolConfigEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolConfigEntry {
    symbol: String,
    price_increment: String,
}

impl AppConfig {
    /// Loads environment configuration and rejects unsafe or internally inconsistent limits.
    fn from_env() -> anyhow::Result<Self> {
        let papertrading = env_parse("LONGBRIDGE_SLC_PAPERTRADING", "true")?;
        validate_live_guard(
            papertrading,
            env::var("LONGBRIDGE_SLC_LIVE_ACK").ok().as_deref(),
        )?;
        validate_realtime_candlesticks()?;

        let symbol_config_path =
            env_string("LONGBRIDGE_SLC_CONFIG_PATH", DEFAULT_SYMBOL_CONFIG_PATH);
        let instruments = load_symbol_config(Path::new(&symbol_config_path))?;
        let risk_amount = env_parse("LONGBRIDGE_SLC_RISK_AMOUNT", "25")?;
        let daily_loss_limit = env_parse("LONGBRIDGE_SLC_DAILY_LOSS_LIMIT", "50")?;
        let max_open_risk = env_parse("LONGBRIDGE_SLC_MAX_OPEN_RISK", "50")?;
        let max_account_notional = env_parse("LONGBRIDGE_SLC_MAX_ACCOUNT_NOTIONAL", "5000")?;
        let max_open_positions = env_parse("LONGBRIDGE_SLC_MAX_OPEN_POSITIONS", "1")?;
        let max_order_quantity = env_parse("LONGBRIDGE_SLC_MAX_ORDER_QUANTITY", "5")?;
        let max_order_notional = env_parse("LONGBRIDGE_SLC_MAX_ORDER_NOTIONAL", "5000")?;
        let max_entry_slippage_ticks = env_parse("LONGBRIDGE_SLC_MAX_ENTRY_SLIPPAGE_TICKS", "5")?;
        let risk_reward = env_parse("LONGBRIDGE_SLC_RISK_REWARD", "2")?;
        anyhow::ensure!(risk_amount > Decimal::ZERO, "risk amount must be positive");
        anyhow::ensure!(
            daily_loss_limit > Decimal::ZERO,
            "daily loss limit must be positive",
        );
        anyhow::ensure!(
            max_open_risk > Decimal::ZERO,
            "maximum open risk must be positive"
        );
        anyhow::ensure!(
            risk_amount <= max_open_risk,
            "risk amount must not exceed maximum open risk",
        );
        anyhow::ensure!(
            max_account_notional > Decimal::ZERO,
            "maximum account notional must be positive",
        );
        anyhow::ensure!(
            max_open_positions > 0,
            "maximum open positions must be positive",
        );
        anyhow::ensure!(
            max_open_positions <= instruments.len(),
            "maximum open positions must not exceed configured instruments",
        );
        anyhow::ensure!(
            Quantity::is_positive(&max_order_quantity),
            "maximum order quantity must be positive",
        );
        anyhow::ensure!(
            max_order_notional > Decimal::ZERO,
            "maximum order notional must be positive",
        );
        anyhow::ensure!(
            max_order_notional <= max_account_notional,
            "maximum order notional must not exceed maximum account notional",
        );
        anyhow::ensure!(risk_reward > Decimal::ZERO, "risk reward must be positive");

        let atr_period = env_parse("LONGBRIDGE_SLC_ATR_PERIOD", "14")?;
        let displacement_atr_multiple = env_parse("LONGBRIDGE_SLC_DISPLACEMENT_ATR", "1.5")?;
        let displacement_close_fraction =
            env_parse("LONGBRIDGE_SLC_DISPLACEMENT_CLOSE_FRACTION", "0.25")?;
        let pivot_span = env_parse("LONGBRIDGE_SLC_PIVOT_SPAN", "2")?;
        let zone_ttl_bars = env_parse("LONGBRIDGE_SLC_ZONE_TTL_BARS", "78")?;
        let max_zones_per_side = env_parse("LONGBRIDGE_SLC_MAX_ZONES_PER_SIDE", "3")?;
        let confirmation_window_bars = env_parse("LONGBRIDGE_SLC_CONFIRMATION_WINDOW_BARS", "3")?;
        let stochastic_k_period = env_parse("LONGBRIDGE_SLC_STOCHASTIC_K_PERIOD", "5")?;
        let stochastic_k_smoothing = env_parse("LONGBRIDGE_SLC_STOCHASTIC_K_SMOOTHING", "3")?;
        let stochastic_d_period = env_parse("LONGBRIDGE_SLC_STOCHASTIC_D_PERIOD", "3")?;
        let oversold = env_parse("LONGBRIDGE_SLC_OVERSOLD", "20")?;
        let overbought = env_parse("LONGBRIDGE_SLC_OVERBOUGHT", "80")?;
        anyhow::ensure!(atr_period > 0, "ATR period must be positive");
        anyhow::ensure!(
            displacement_atr_multiple > 0.0,
            "displacement ATR multiple must be positive",
        );
        anyhow::ensure!(
            (0.0..=0.5).contains(&displacement_close_fraction),
            "displacement close fraction must be between 0 and 0.5",
        );
        anyhow::ensure!(
            pivot_span > 0 && pivot_span <= (MAX_WARMUP_BARS - 1) / 2,
            "pivot span must fit inside the maximum warmup window",
        );
        anyhow::ensure!(zone_ttl_bars > 0, "zone TTL must be positive");
        anyhow::ensure!(
            (1..=32).contains(&max_zones_per_side),
            "maximum zones per side must be between 1 and 32",
        );
        anyhow::ensure!(
            confirmation_window_bars > 0 && confirmation_window_bars <= zone_ttl_bars,
            "confirmation window must be positive and not exceed zone TTL",
        );
        anyhow::ensure!(
            stochastic_k_period > 0 && stochastic_k_smoothing > 0 && stochastic_d_period > 0,
            "stochastic periods must be positive",
        );
        anyhow::ensure!(
            0.0 < oversold && oversold < overbought && overbought < 100.0,
            "stochastic thresholds must satisfy 0 < oversold < overbought < 100",
        );

        let five_minute_warmup = env_parse("LONGBRIDGE_SLC_5M_WARMUP", "500")?;
        let four_hour_warmup = env_parse("LONGBRIDGE_SLC_4H_WARMUP", "100")?;
        let minimum_five_minute_warmup = atr_period.max(
            stochastic_k_period
                .saturating_add(stochastic_k_smoothing)
                .saturating_add(stochastic_d_period),
        );
        anyhow::ensure!(
            five_minute_warmup > minimum_five_minute_warmup
                && five_minute_warmup <= MAX_WARMUP_BARS,
            "5-minute warmup must initialize ATR and stochastic periods and not exceed {MAX_WARMUP_BARS}",
        );
        anyhow::ensure!(
            four_hour_warmup > pivot_span * 2 + 1 && four_hour_warmup <= MAX_WARMUP_BARS,
            "4-hour warmup must exceed the pivot window and not exceed {MAX_WARMUP_BARS}",
        );

        let session = SessionRules {
            entry_start_minute: env_time("LONGBRIDGE_SLC_ENTRY_START", "09:35")?,
            entry_end_minute: env_time("LONGBRIDGE_SLC_ENTRY_END", "15:30")?,
            flatten_before_close_minutes: env_parse(
                "LONGBRIDGE_SLC_FLATTEN_BEFORE_CLOSE_MINUTES",
                "5",
            )?,
            max_trades_per_day: env_parse("LONGBRIDGE_SLC_MAX_TRADES_PER_DAY", "1")?,
        };
        session.validate()?;
        let default_risk_state_path = if papertrading {
            DEFAULT_PAPER_RISK_STATE_PATH
        } else {
            DEFAULT_LIVE_RISK_STATE_PATH
        };
        let risk_state_path = PathBuf::from(env_string(
            "LONGBRIDGE_SLC_RISK_STATE_PATH",
            default_risk_state_path,
        ));

        Ok(Self {
            instruments,
            papertrading,
            risk_amount,
            daily_loss_limit,
            max_open_risk,
            max_account_notional,
            max_open_positions,
            max_order_quantity,
            max_order_notional,
            max_entry_slippage_ticks,
            risk_reward,
            stop_buffer_ticks: env_parse("LONGBRIDGE_SLC_STOP_BUFFER_TICKS", "1")?,
            atr_period,
            displacement_atr_multiple,
            displacement_close_fraction,
            pivot_span,
            zone_ttl_bars,
            max_zones_per_side,
            confirmation_window_bars,
            stochastic_k_period,
            stochastic_k_smoothing,
            stochastic_d_period,
            oversold,
            overbought,
            five_minute_warmup,
            four_hour_warmup,
            risk_state_path,
            timezone: get_timezone(US_TIMEZONE)?,
            session,
        })
    }

    /// Returns the external five-minute bar type used for signals and fallback target checks.
    fn five_minute_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{}-5-MINUTE-LAST-EXTERNAL", instrument_id).as_str())
    }

    /// Returns the external four-hour bar type used for higher-timeframe structure.
    fn four_hour_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{}-4-HOUR-LAST-EXTERNAL", instrument_id).as_str())
    }

    /// Builds the data-client configuration for every configured instrument.
    fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            instrument_price_increments: self
                .instruments
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
}

/// Returns an environment value or its explicit default without silently trimming values.
fn env_string(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Parses one typed environment value and includes its name and raw value in failures.
fn env_parse<T>(name: &str, default: &str) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    let value = env_string(name, default);
    value
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid {name}={value:?}: {e}"))
}

/// Reads and validates the external multi-symbol TOML configuration.
fn load_symbol_config(path: &Path) -> anyhow::Result<Vec<SlcInstrument>> {
    let value = fs::read_to_string(path)
        .with_context(|| format!("failed to read SLC symbol config {}", path.display()))?;
    parse_symbol_config(&value)
        .with_context(|| format!("invalid SLC symbol config {}", path.display()))
}

/// Parses unique Longbridge US symbols and exact positive price increments.
fn parse_symbol_config(value: &str) -> anyhow::Result<Vec<SlcInstrument>> {
    let config: SymbolConfig = toml::from_str(value).context("failed to parse TOML")?;
    anyhow::ensure!(
        !config.symbols.is_empty(),
        "symbols must contain at least one instrument",
    );
    anyhow::ensure!(
        config.symbols.len() <= MAX_QUOTE_SUBSCRIPTION_SYMBOLS,
        "symbol config supports at most {MAX_QUOTE_SUBSCRIPTION_SYMBOLS} instruments",
    );

    let mut instruments = Vec::with_capacity(config.symbols.len());
    for entry in config.symbols {
        let symbol = entry.symbol.trim();
        anyhow::ensure!(
            symbol.ends_with(".US"),
            "SLC live example currently requires a Longbridge US symbol such as QQQ.US: {symbol}",
        );
        let instrument_id: InstrumentId = format!("{symbol}.LONGBRIDGE")
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid Longbridge symbol {symbol:?}: {e}"))?;
        anyhow::ensure!(
            instrument_id.venue.as_str() == "LONGBRIDGE"
                && instrument_id.symbol.as_str().ends_with(".US"),
            "SLC live example currently requires US equities on venue LONGBRIDGE: {instrument_id}",
        );
        anyhow::ensure!(
            !instruments
                .iter()
                .any(|instrument: &SlcInstrument| instrument.instrument_id == instrument_id),
            "duplicate Longbridge instrument configured: {instrument_id}",
        );
        let price_increment = entry.price_increment.parse::<Price>().map_err(|e| {
            anyhow::anyhow!(
                "invalid price_increment {:?} for {symbol}: {e}",
                entry.price_increment,
            )
        })?;
        anyhow::ensure!(
            Price::is_positive(&price_increment),
            "price_increment for {symbol} must be positive",
        );
        instruments.push(SlcInstrument {
            instrument_id,
            price_increment,
        });
    }
    Ok(instruments)
}

/// Parses an `HH:MM` environment value into minutes since local midnight.
fn env_time(name: &str, default: &str) -> anyhow::Result<u16> {
    let value = env_string(name, default);
    let (hour, minute) = value
        .split_once(':')
        .with_context(|| format!("invalid {name}={value:?}, expected HH:MM"))?;
    let hour: u16 = hour
        .parse()
        .with_context(|| format!("invalid hour in {name}={value:?}"))?;
    let minute: u16 = minute
        .parse()
        .with_context(|| format!("invalid minute in {name}={value:?}"))?;
    anyhow::ensure!(
        hour < 24 && minute < 60,
        "invalid {name}={value:?}, expected HH:MM",
    );
    Ok(hour * 60 + minute)
}

/// Requires an exact second acknowledgement before the example can route live orders.
fn validate_live_guard(papertrading: bool, live_ack: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        papertrading || live_ack == Some(LIVE_ACK),
        concat!(
            "live trading requires LONGBRIDGE_SLC_LIVE_ACK=",
            "I_UNDERSTAND_LIVE_ORDERS",
        ),
    );
    Ok(())
}

/// Rejects confirmed-only pushes because they add one full bar of finalization latency.
fn validate_realtime_candlesticks() -> anyhow::Result<()> {
    let mode = env::var("LONGBRIDGE_PUSH_CANDLESTICK_MODE")
        .ok()
        .or_else(|| env::var("LONGPORT_PUSH_CANDLESTICK_MODE").ok());
    anyhow::ensure!(
        mode.as_deref() != Some("confirmed"),
        concat!(
            "SLC strategy requires Longbridge realtime candlestick pushes so it can finalize ",
            "the previous bar when the next bar starts; unset LONGBRIDGE_PUSH_CANDLESTICK_MODE",
        ),
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Trend {
    Up,
    Down,
    #[default]
    Neutral,
}

#[derive(Debug)]
struct PivotStructure {
    span: usize,
    window: VecDeque<Bar>,
    highs: VecDeque<Price>,
    lows: VecDeque<Price>,
}

impl PivotStructure {
    /// Creates a symmetric confirmed-pivot detector with the requested bars on each side.
    fn new(span: usize) -> Self {
        Self {
            span,
            window: VecDeque::with_capacity(span * 2 + 1),
            highs: VecDeque::with_capacity(2),
            lows: VecDeque::with_capacity(2),
        }
    }

    /// Adds one completed four-hour bar and confirms only the center of a full pivot window.
    fn update(&mut self, bar: Bar) {
        let window_size = self.span * 2 + 1;
        if self.window.len() == window_size {
            self.window.pop_front();
        }
        self.window.push_back(bar);
        if self.window.len() != window_size {
            return;
        }

        let center = self.window[self.span];
        let pivot_high = self
            .window
            .iter()
            .enumerate()
            .all(|(index, candidate)| index == self.span || center.high > candidate.high);
        let pivot_low = self
            .window
            .iter()
            .enumerate()
            .all(|(index, candidate)| index == self.span || center.low < candidate.low);
        if pivot_high {
            push_last_two(&mut self.highs, center.high);
        }
        if pivot_low {
            push_last_two(&mut self.lows, center.low);
        }
    }

    /// Returns whether two confirmed pivot highs and lows are available for classification.
    fn initialized(&self) -> bool {
        self.highs.len() == 2 && self.lows.len() == 2
    }

    /// Classifies structure from the last two confirmed pivot highs and lows.
    fn trend(&self) -> Trend {
        if !self.initialized() {
            return Trend::Neutral;
        }
        let high_rising = self.highs[1] > self.highs[0];
        let high_falling = self.highs[1] < self.highs[0];
        let low_rising = self.lows[1] > self.lows[0];
        let low_falling = self.lows[1] < self.lows[0];

        if high_rising && low_rising {
            Trend::Up
        } else if high_falling && low_falling {
            Trend::Down
        } else {
            Trend::Neutral
        }
    }
}

/// Retains only the two most recent confirmed pivots required for structure classification.
fn push_last_two(values: &mut VecDeque<Price>, value: Price) {
    if values.len() == 2 {
        values.pop_front();
    }
    values.push_back(value);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneKind {
    Demand,
    Supply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneState {
    Fresh,
    AwaitingConfirmation,
    BrokenOnce,
    Reclaimed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Zone {
    kind: ZoneKind,
    low: Price,
    high: Price,
    age: usize,
    state: ZoneState,
    break_count: u8,
    confirmation_armed: bool,
    confirmation_bars_left: usize,
}

impl Zone {
    /// Creates a fresh zone from the full range of the candle before displacement.
    fn from_bar(kind: ZoneKind, bar: Bar) -> Self {
        Self {
            kind,
            low: bar.low,
            high: bar.high,
            age: 0,
            state: ZoneState::Fresh,
            break_count: 0,
            confirmation_armed: false,
            confirmation_bars_left: 0,
        }
    }

    /// Returns whether any part of a completed bar trades inside the zone.
    fn intersects(self, bar: Bar) -> bool {
        bar.low <= self.high && bar.high >= self.low
    }

    /// Returns whether price decisively closes through the far side of the zone.
    fn broken(self, bar: Bar) -> bool {
        match self.kind {
            ZoneKind::Demand => bar.close < self.low,
            ZoneKind::Supply => bar.close > self.high,
        }
    }

    /// Returns whether a once-broken zone has been reclaimed from the opposite side.
    fn reclaimed(self, bar: Bar) -> bool {
        match self.kind {
            ZoneKind::Demand => bar.close > self.high,
            ZoneKind::Supply => bar.close < self.low,
        }
    }

    /// Starts the bounded stochastic confirmation window after a valid retest.
    fn begin_confirmation(&mut self, confirmation: Confirmation, window_bars: usize) {
        self.state = ZoneState::AwaitingConfirmation;
        self.confirmation_armed = confirmation.extreme;
        self.confirmation_bars_left = window_bars + 1;
    }

    /// Advances the untouched or once-broken SLC level state machine by one completed bar.
    fn observe(
        &mut self,
        bar: Bar,
        confirmation: Confirmation,
        allow_entry: bool,
        side: OrderSide,
        rules: SignalRules,
    ) -> ZoneObservation {
        self.age += 1;
        if self.age > rules.zone_ttl_bars {
            return ZoneObservation::Remove;
        }

        if self.broken(bar) {
            match self.state {
                ZoneState::BrokenOnce => return ZoneObservation::Keep,
                ZoneState::Fresh | ZoneState::AwaitingConfirmation if self.break_count == 0 => {
                    self.break_count = 1;
                    self.state = ZoneState::BrokenOnce;
                    self.confirmation_armed = false;
                    self.confirmation_bars_left = 0;
                    return ZoneObservation::Keep;
                }
                ZoneState::Fresh | ZoneState::AwaitingConfirmation | ZoneState::Reclaimed => {
                    return ZoneObservation::Remove;
                }
            }
        }

        match self.state {
            ZoneState::Fresh | ZoneState::Reclaimed if self.intersects(bar) => {
                self.begin_confirmation(confirmation, rules.confirmation_window_bars);
            }
            ZoneState::BrokenOnce if self.reclaimed(bar) => {
                self.state = ZoneState::Reclaimed;
                return ZoneObservation::Keep;
            }
            ZoneState::BrokenOnce | ZoneState::Fresh | ZoneState::Reclaimed => {
                return ZoneObservation::Keep;
            }
            ZoneState::AwaitingConfirmation => {}
        }

        self.confirmation_armed |= confirmation.extreme;
        if allow_entry && self.confirmation_armed && confirmation.reentry {
            return ZoneObservation::Signal(Signal {
                side,
                entry: bar.close,
                zone_low: self.low,
                zone_high: self.high,
                ts_event: bar.ts_event,
            });
        }

        self.confirmation_bars_left -= 1;
        if self.confirmation_bars_left == 0 {
            ZoneObservation::Remove
        } else {
            ZoneObservation::Keep
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Signal {
    side: OrderSide,
    entry: Price,
    zone_low: Price,
    zone_high: Price,
    ts_event: UnixNanos,
}

#[derive(Clone, Copy, Debug)]
struct Confirmation {
    extreme: bool,
    reentry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneObservation {
    Keep,
    Remove,
    Signal(Signal),
}

#[derive(Debug, Default)]
struct FinalBarBuffer {
    pending: Option<Bar>,
}

impl FinalBarBuffer {
    /// Replaces updates for the current timestamp and emits it only after the next bar starts.
    fn update(&mut self, bar: Bar) -> Option<Bar> {
        let Some(pending) = self.pending else {
            self.pending = Some(bar);
            return None;
        };
        if bar.ts_event < pending.ts_event {
            return None;
        }
        if bar.ts_event == pending.ts_event {
            self.pending = Some(bar);
            return None;
        }
        self.pending = Some(bar);
        Some(pending)
    }
}

#[derive(Clone, Copy, Debug)]
struct SignalRules {
    zone_ttl_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    oversold: f64,
    overbought: f64,
}

struct SlcSignalState {
    five_minute_bars: FinalBarBuffer,
    four_hour_bars: FinalBarBuffer,
    structure: PivotStructure,
    atr: AverageTrueRange,
    stochastics: Stochastics,
    previous_five_minute_bar: Option<Bar>,
    previous_k: Option<f64>,
    demand: VecDeque<Zone>,
    supply: VecDeque<Zone>,
    rules: SignalRules,
}

impl SlcSignalState {
    /// Builds per-symbol indicators and bounded zone collections from validated configuration.
    fn new(config: &AppConfig) -> Self {
        Self {
            five_minute_bars: FinalBarBuffer::default(),
            four_hour_bars: FinalBarBuffer::default(),
            structure: PivotStructure::new(config.pivot_span),
            atr: AverageTrueRange::new(
                config.atr_period,
                Some(MovingAverageType::Wilder),
                Some(true),
                None,
            ),
            stochastics: Stochastics::new_with_params(
                config.stochastic_k_period,
                config.stochastic_k_smoothing,
                config.stochastic_d_period,
                MovingAverageType::Simple,
                StochasticsDMethod::MovingAverage,
            ),
            previous_five_minute_bar: None,
            previous_k: None,
            demand: VecDeque::with_capacity(config.max_zones_per_side),
            supply: VecDeque::with_capacity(config.max_zones_per_side),
            rules: SignalRules {
                zone_ttl_bars: config.zone_ttl_bars,
                max_zones_per_side: config.max_zones_per_side,
                confirmation_window_bars: config.confirmation_window_bars,
                displacement_atr_multiple: config.displacement_atr_multiple,
                displacement_close_fraction: config.displacement_close_fraction,
                oversold: config.oversold,
                overbought: config.overbought,
            },
        }
    }

    /// Replays completed historical bars while suppressing historical entry signals.
    fn warm_up(&mut self, five_minute_bars: Vec<Bar>, four_hour_bars: Vec<Bar>) {
        for bar in four_hour_bars {
            if let Some(finalized) = self.four_hour_bars.update(bar) {
                self.structure.update(finalized);
            }
        }
        for bar in five_minute_bars {
            if let Some(finalized) = self.five_minute_bars.update(bar) {
                let _ = self.process_five_minute(finalized, false);
            }
        }
    }

    /// Returns whether every indicator and higher-timeframe structure input is initialized.
    fn ready(&self) -> bool {
        self.atr.initialized() && self.stochastics.initialized() && self.structure.initialized()
    }

    /// Finalizes the pending four-hour update when a newer timestamp arrives.
    fn finalize_four_hour(&mut self, bar: Bar) -> Option<Bar> {
        self.four_hour_bars.update(bar)
    }

    /// Updates confirmed four-hour pivot structure from one completed bar.
    fn process_four_hour(&mut self, bar: Bar) {
        self.structure.update(bar);
    }

    /// Finalizes the pending five-minute update when a newer timestamp arrives.
    fn finalize_five_minute(&mut self, bar: Bar) -> Option<Bar> {
        self.five_minute_bars.update(bar)
    }

    /// Updates levels and indicators, then returns at most one trend-aligned confirmed signal.
    fn process_five_minute(&mut self, bar: Bar, allow_signal: bool) -> Option<Signal> {
        let atr_before = self.atr.value;
        let atr_initialized = self.atr.initialized();
        self.stochastics.handle_bar(&bar);
        let current_k = self.stochastics.value_k;
        let stochastic_initialized = self.stochastics.initialized();
        let long_cross = stochastic_initialized
            && self
                .previous_k
                .is_some_and(|previous| previous <= self.rules.oversold)
            && current_k > self.rules.oversold;
        let short_cross = stochastic_initialized
            && self
                .previous_k
                .is_some_and(|previous| previous >= self.rules.overbought)
            && current_k < self.rules.overbought;

        let trend = self.structure.trend();
        let long_signal = observe_zones(
            &mut self.demand,
            bar,
            Confirmation {
                extreme: stochastic_initialized && current_k <= self.rules.oversold,
                reentry: long_cross,
            },
            trend == Trend::Up && allow_signal,
            OrderSide::Buy,
            self.rules,
        );
        let short_signal = observe_zones(
            &mut self.supply,
            bar,
            Confirmation {
                extreme: stochastic_initialized && current_k >= self.rules.overbought,
                reentry: short_cross,
            },
            trend == Trend::Down && allow_signal,
            OrderSide::Sell,
            self.rules,
        );

        if atr_initialized && let Some(previous) = self.previous_five_minute_bar {
            if is_up_displacement(previous, bar, atr_before, self.rules) {
                push_zone(
                    &mut self.demand,
                    Zone::from_bar(ZoneKind::Demand, previous),
                    self.rules.max_zones_per_side,
                );
            } else if is_down_displacement(previous, bar, atr_before, self.rules) {
                push_zone(
                    &mut self.supply,
                    Zone::from_bar(ZoneKind::Supply, previous),
                    self.rules.max_zones_per_side,
                );
            }
        }

        self.atr.handle_bar(&bar);
        self.previous_five_minute_bar = Some(bar);
        self.previous_k = Some(current_k);
        long_signal.or(short_signal)
    }
}

/// Advances every active level, preferring the most recent confirmed setup on the bar.
fn observe_zones(
    zones: &mut VecDeque<Zone>,
    bar: Bar,
    confirmation: Confirmation,
    allow_entry: bool,
    side: OrderSide,
    rules: SignalRules,
) -> Option<Signal> {
    let mut signal = None;
    for index in (0..zones.len()).rev() {
        let observation = zones[index].observe(
            bar,
            confirmation,
            allow_entry && signal.is_none(),
            side,
            rules,
        );
        match observation {
            ZoneObservation::Keep => {}
            ZoneObservation::Remove => {
                zones.remove(index);
            }
            ZoneObservation::Signal(candidate) => {
                zones.remove(index);
                signal = Some(candidate);
            }
        }
    }
    signal
}

/// Adds the newest level and evicts only the oldest level when the configured bound is full.
fn push_zone(zones: &mut VecDeque<Zone>, zone: Zone, max_zones: usize) {
    if zones.len() == max_zones {
        zones.pop_front();
    }
    zones.push_back(zone);
}

/// Returns whether an opposing base candle is followed by bullish ATR-sized displacement.
fn is_up_displacement(previous: Bar, current: Bar, atr: f64, rules: SignalRules) -> bool {
    previous.close < previous.open
        && current.close > current.open
        && displacement_body(current) >= atr * rules.displacement_atr_multiple
        && close_fraction_from_high(current) <= rules.displacement_close_fraction
}

/// Returns whether an opposing base candle is followed by bearish ATR-sized displacement.
fn is_down_displacement(previous: Bar, current: Bar, atr: f64, rules: SignalRules) -> bool {
    previous.close > previous.open
        && current.close < current.open
        && displacement_body(current) >= atr * rules.displacement_atr_multiple
        && close_fraction_from_low(current) <= rules.displacement_close_fraction
}

/// Returns the absolute candle body in quote-price units.
fn displacement_body(bar: Bar) -> f64 {
    (bar.close.as_f64() - bar.open.as_f64()).abs()
}

/// Returns the fraction of a candle range left above its close.
fn close_fraction_from_high(bar: Bar) -> f64 {
    let range = bar.high.as_f64() - bar.low.as_f64();
    if range == 0.0 {
        return 1.0;
    }
    (bar.high.as_f64() - bar.close.as_f64()) / range
}

/// Returns the fraction of a candle range left below its close.
fn close_fraction_from_low(bar: Bar) -> f64 {
    let range = bar.high.as_f64() - bar.low.as_f64();
    if range == 0.0 {
        return 1.0;
    }
    (bar.close.as_f64() - bar.low.as_f64()) / range
}

#[derive(Clone, Copy, Debug)]
struct AccountRiskLimits {
    daily_loss: Decimal,
    open_risk: Decimal,
    account_notional: Decimal,
    open_positions: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct RiskReservation {
    #[serde(
        serialize_with = "serialize_decimal_as_str",
        deserialize_with = "deserialize_decimal_from_str"
    )]
    risk: Decimal,
    #[serde(
        serialize_with = "serialize_decimal_as_str",
        deserialize_with = "deserialize_decimal_from_str"
    )]
    notional: Decimal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
struct AccountRiskState {
    version: u8,
    date: Option<String>,
    entries_by_symbol: HashMap<String, usize>,
    #[serde(
        serialize_with = "serialize_decimal_as_str",
        deserialize_with = "deserialize_decimal_from_str"
    )]
    realized_pnl: Decimal,
    halted: bool,
    reservations: HashMap<String, RiskReservation>,
}

impl Default for AccountRiskState {
    fn default() -> Self {
        Self {
            version: 1,
            date: None,
            entries_by_symbol: HashMap::new(),
            realized_pnl: Decimal::ZERO,
            halted: false,
            reservations: HashMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservationOutcome {
    Reserved,
    Rejected,
}

#[derive(Clone, Copy, Debug)]
struct AccountRiskSnapshot {
    realized_pnl: Decimal,
    halted: bool,
    open_risk: Decimal,
    account_notional: Decimal,
    open_positions: usize,
    entries_for_symbol: usize,
}

#[derive(Debug)]
struct AccountRisk {
    path: PathBuf,
    state: Mutex<AccountRiskState>,
}

impl AccountRisk {
    /// Loads fail-closed account risk state from disk or creates a clean first-run state.
    fn load(path: PathBuf) -> anyhow::Result<Self> {
        let state = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read SLC risk state {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse SLC risk state {}", path.display()))?
        } else {
            AccountRiskState::default()
        };
        validate_account_risk_state(&state)?;
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Reserves account capacity before an entry command can reach the execution engine.
    fn reserve_entry(
        &self,
        symbol: &str,
        date: jiff::civil::Date,
        reservation: RiskReservation,
        max_trades_per_symbol: usize,
        limits: AccountRiskLimits,
    ) -> anyhow::Result<(ReservationOutcome, AccountRiskSnapshot)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        roll_account_risk_date(&mut state, date);
        let snapshot = account_risk_snapshot(&state, symbol);
        let rejected = snapshot.entries_for_symbol >= max_trades_per_symbol
            || snapshot.halted
            || snapshot.realized_pnl <= -limits.daily_loss
            || snapshot.open_positions >= limits.open_positions
            || snapshot.open_risk + reservation.risk > limits.open_risk
            || snapshot.account_notional + reservation.notional > limits.account_notional
            || state.reservations.contains_key(symbol);
        if rejected {
            if *state != before {
                self.persist_or_restore(&mut state, before)?;
            }
            return Ok((ReservationOutcome::Rejected, snapshot));
        }

        *state
            .entries_by_symbol
            .entry(symbol.to_string())
            .or_default() += 1;
        state.reservations.insert(symbol.to_string(), reservation);
        self.persist_or_restore(&mut state, before)?;
        Ok((
            ReservationOutcome::Reserved,
            account_risk_snapshot(&state, symbol),
        ))
    }

    /// Releases an entry reservation and trade count when no quantity was filled.
    fn release_unfilled(&self, symbol: &str) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        let remove_entry_count = state.reservations.remove(symbol).is_some()
            && state
                .entries_by_symbol
                .get_mut(symbol)
                .is_some_and(|entries| {
                    *entries = entries.saturating_sub(1);
                    *entries == 0
                });
        if remove_entry_count {
            state.entries_by_symbol.remove(symbol);
        }
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// Releases open risk after a filled entry ends while retaining its daily trade count.
    fn release_reservation(&self, symbol: &str) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        state.reservations.remove(symbol);
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// Records realized PnL and releases risk unless an entry remainder can still refill it.
    fn record_close(
        &self,
        symbol: &str,
        date: jiff::civil::Date,
        realized_pnl: Option<Decimal>,
        release_reservation: bool,
    ) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        roll_account_risk_date(&mut state, date);
        if release_reservation {
            state.reservations.remove(symbol);
        }
        if let Some(pnl) = realized_pnl {
            state.realized_pnl += pnl;
        } else {
            state.halted = true;
        }
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// Reconciles one symbol and halts new entries when broker exposure lacks reserved risk.
    fn reconcile_symbol(
        &self,
        symbol: &str,
        date: jiff::civil::Date,
        has_exposure: bool,
    ) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        roll_account_risk_date(&mut state, date);
        if has_exposure && !state.reservations.contains_key(symbol) {
            state.halted = true;
        } else if !has_exposure {
            state.reservations.remove(symbol);
        }
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// Persists risk state before returning, restoring the in-memory copy on write failure.
    fn persist_or_restore(
        &self,
        state: &mut AccountRiskState,
        before: AccountRiskState,
    ) -> anyhow::Result<()> {
        if *state == before {
            return Ok(());
        }
        if let Err(e) = self.persist(state) {
            *state = before;
            return Err(e);
        }
        Ok(())
    }

    /// Writes the complete small state file and synchronizes it before orders can proceed.
    fn persist(&self, state: &AccountRiskState) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create SLC risk state directory {}",
                    parent.display()
                )
            })?;
        }
        let encoded = toml::to_string(state).context("failed to serialize SLC risk state")?;
        let mut file = fs::File::create(&self.path)
            .with_context(|| format!("failed to create SLC risk state {}", self.path.display()))?;
        file.write_all(encoded.as_bytes())
            .with_context(|| format!("failed to write SLC risk state {}", self.path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync SLC risk state {}", self.path.display()))
    }
}

/// Rejects corrupt or unsafe values before persisted state can control live entries.
fn validate_account_risk_state(state: &AccountRiskState) -> anyhow::Result<()> {
    anyhow::ensure!(state.version == 1, "unsupported SLC risk state version");
    anyhow::ensure!(
        state
            .reservations
            .values()
            .all(|reservation| reservation.risk > Decimal::ZERO
                && reservation.notional > Decimal::ZERO),
        "SLC risk state contains a non-positive reservation",
    );
    Ok(())
}

/// Starts a new US trading-day ledger while retaining risk reserved by overnight exposure.
fn roll_account_risk_date(state: &mut AccountRiskState, date: jiff::civil::Date) {
    let date = date.to_string();
    if state.date.as_deref() == Some(date.as_str()) {
        return;
    }
    state.date = Some(date);
    state.entries_by_symbol.clear();
    state.realized_pnl = Decimal::ZERO;
    state.halted = false;
}

/// Returns exact aggregate account risk values from the persisted reservation ledger.
fn account_risk_snapshot(state: &AccountRiskState, symbol: &str) -> AccountRiskSnapshot {
    AccountRiskSnapshot {
        realized_pnl: state.realized_pnl,
        halted: state.halted,
        open_risk: state
            .reservations
            .values()
            .map(|reservation| reservation.risk)
            .sum(),
        account_notional: state
            .reservations
            .values()
            .map(|reservation| reservation.notional)
            .sum(),
        open_positions: state.reservations.len(),
        entries_for_symbol: state
            .entries_by_symbol
            .get(symbol)
            .copied()
            .unwrap_or_default(),
    }
}

/// Returns whether two regular-session bar starts are not exactly five minutes apart.
fn has_five_minute_gap(previous: UnixNanos, current: UnixNanos) -> bool {
    current.as_u64().saturating_sub(previous.as_u64()) != FIVE_MINUTE_NANOS
}

#[derive(Clone, Debug)]
struct SlcStrategyConfig {
    instrument_id: InstrumentId,
    five_minute_bar_type: BarType,
    four_hour_bar_type: BarType,
    timezone: TimeZone,
    entry_start_minute: u16,
    entry_end_minute: u16,
    flatten_minute: u16,
    max_trades_per_day: usize,
    risk_amount: Decimal,
    account_risk_limits: AccountRiskLimits,
    max_order_quantity: Quantity,
    max_entry_slippage_ticks: u64,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingEntry {
    client_order_id: ClientOrderId,
    side: OrderSide,
    entry_limit: Price,
    stop: Price,
    signal_ts: UnixNanos,
    had_fill: bool,
}

#[derive(Debug)]
struct ActiveTrade {
    side: OrderSide,
    target: Price,
    filled_qty: Decimal,
    protected_qty: Decimal,
    fill_notional: Decimal,
}

struct SlcStrategy {
    core: StrategyCore,
    config: SlcStrategyConfig,
    instrument: InstrumentAny,
    signals: SlcSignalState,
    account_risk: Arc<AccountRisk>,
    pending_entry: Option<PendingEntry>,
    active_trade: Option<ActiveTrade>,
    current_date: Option<jiff::civil::Date>,
    last_five_minute_bar_start: Option<UnixNanos>,
    suppress_warmup_boundary_signal: bool,
    session_disabled: bool,
    exit_pending: bool,
    faulted: bool,
}

impl SlcStrategy {
    /// Creates one isolated per-symbol strategy sharing only the account risk ledger.
    fn new(
        app_config: &AppConfig,
        instrument_id: InstrumentId,
        instrument: InstrumentAny,
        five_minute_bars: Vec<Bar>,
        four_hour_bars: Vec<Bar>,
        flatten_minute: u16,
        account_risk: Arc<AccountRisk>,
    ) -> anyhow::Result<Self> {
        let five_minute_warmup_count = five_minute_bars.len();
        let four_hour_warmup_count = four_hour_bars.len();
        let mut signals = SlcSignalState::new(app_config);
        signals.warm_up(five_minute_bars, four_hour_bars);
        anyhow::ensure!(
            signals.ready(),
            "SLC warmup did not initialize indicators and 4h pivots for {instrument_id}",
        );
        log::info!(
            "[{instrument_id}] SLC warmup complete: 5m_bars={five_minute_warmup_count}, 4h_bars={four_hour_warmup_count}, initial_4h_trend={:?}",
            signals.structure.trend(),
        );
        Ok(Self {
            core: StrategyCore::new(StrategyConfig {
                strategy_id: Some(StrategyId::from(
                    format!("{STRATEGY_ID}-{}", instrument_id.symbol).as_str(),
                )),
                order_id_tag: Some(ORDER_ID_TAG.to_string()),
                external_order_claims: Some(vec![instrument_id]),
                manage_stop: true,
                market_exit_max_attempts: 300,
                market_exit_time_in_force: TimeInForce::Day,
                market_exit_reduce_only: false,
                ..Default::default()
            }),
            config: SlcStrategyConfig {
                instrument_id,
                five_minute_bar_type: AppConfig::five_minute_bar_type(instrument_id),
                four_hour_bar_type: AppConfig::four_hour_bar_type(instrument_id),
                timezone: app_config.timezone.clone(),
                entry_start_minute: app_config.session.entry_start_minute,
                entry_end_minute: app_config.session.entry_end_minute.min(flatten_minute),
                flatten_minute,
                max_trades_per_day: app_config.session.max_trades_per_day,
                risk_amount: app_config.risk_amount,
                account_risk_limits: AccountRiskLimits {
                    daily_loss: app_config.daily_loss_limit,
                    open_risk: app_config.max_open_risk,
                    account_notional: app_config.max_account_notional,
                    open_positions: app_config.max_open_positions,
                },
                max_order_quantity: app_config.max_order_quantity,
                max_entry_slippage_ticks: app_config.max_entry_slippage_ticks,
                risk_reward: app_config.risk_reward,
                stop_buffer_ticks: app_config.stop_buffer_ticks,
            },
            instrument,
            signals,
            account_risk,
            pending_entry: None,
            active_trade: None,
            current_date: None,
            last_five_minute_bar_start: None,
            suppress_warmup_boundary_signal: true,
            session_disabled: false,
            exit_pending: false,
            faulted: false,
        })
    }

    /// Returns whether this strategy owns any open order, in-flight order, or position.
    fn has_exposure(&self) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let instrument_id = self.config.instrument_id;
        let cache = self.cache();
        cache.orders_open_count(None, Some(&instrument_id), Some(&strategy_id), None, None) > 0
            || cache.orders_inflight_count(
                None,
                Some(&instrument_id),
                Some(&strategy_id),
                None,
                None,
            ) > 0
            || !cache
                .positions_open(None, Some(&instrument_id), Some(&strategy_id), None, None)
                .is_empty()
    }

    /// Returns whether this strategy owns a currently open position.
    fn has_open_position(&self) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        !self
            .cache()
            .positions_open(
                None,
                Some(&self.config.instrument_id),
                Some(&strategy_id),
                None,
                None,
            )
            .is_empty()
    }

    /// Returns whether this strategy owns an open or in-flight order.
    fn has_open_orders(&self) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let cache = self.cache();
        cache.orders_open_count(
            None,
            Some(&self.config.instrument_id),
            Some(&strategy_id),
            None,
            None,
        ) > 0
            || cache.orders_inflight_count(
                None,
                Some(&self.config.instrument_id),
                Some(&strategy_id),
                None,
                None,
            ) > 0
    }

    /// Submits a market exit for every open position owned by this strategy.
    fn close_positions(&mut self) -> anyhow::Result<()> {
        self.close_all_positions(
            self.config.instrument_id,
            None,
            None,
            Some(vec![Ustr::from("SLC_EXIT")]),
            Some(TimeInForce::Day),
            Some(false),
            Some(false),
            None,
        )
    }

    /// Disables new entries after an order failure and starts managed market exit recovery.
    fn disable_after_order_failure(&mut self, reason: &str) {
        self.faulted = true;
        log::error!(
            "[{}] SLC trading disabled after order failure: {reason}",
            self.config.instrument_id,
        );
        if let Err(e) = self.market_exit() {
            log::error!(
                "[{}] Failed to flatten SLC exposure after order failure: {e:#}",
                self.config.instrument_id,
            );
        }
    }

    /// Reserves account risk and submits a one-bar marketable limit entry at the worst allowed price.
    fn submit_signal(
        &mut self,
        signal: Signal,
        local_date: jiff::civil::Date,
    ) -> anyhow::Result<()> {
        let (stop, entry_limit) = entry_prices(
            signal,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.stop_buffer_ticks,
            self.config.max_entry_slippage_ticks,
        )?;
        let lot_size = self
            .instrument
            .lot_size()
            .unwrap_or_else(|| Quantity::from(1));
        let Some(quantity) = risk_sized_quantity(
            entry_limit,
            stop,
            self.config.risk_amount,
            self.config.max_order_quantity,
            lot_size,
        )?
        else {
            log::warn!(
                "[{}] Skipping SLC signal because risk sizing produced zero quantity",
                self.config.instrument_id,
            );
            return Ok(());
        };
        let quantity_decimal = quantity.as_decimal();
        let reservation = RiskReservation {
            risk: (entry_limit.as_decimal() - stop.as_decimal()).abs() * quantity_decimal,
            notional: entry_limit.as_decimal() * quantity_decimal,
        };
        let symbol = self.config.instrument_id.symbol.to_string();
        let (outcome, snapshot) = self.account_risk.reserve_entry(
            &symbol,
            local_date,
            reservation,
            self.config.max_trades_per_day,
            self.config.account_risk_limits,
        )?;
        if outcome == ReservationOutcome::Rejected {
            log::warn!(
                "[{}] Skipping SLC signal after account risk check: halted={}, daily_pnl={}, open_risk={}, account_notional={}, open_positions={}, symbol_entries={}",
                self.config.instrument_id,
                snapshot.halted,
                snapshot.realized_pnl,
                snapshot.open_risk,
                snapshot.account_notional,
                snapshot.open_positions,
                snapshot.entries_for_symbol,
            );
            return Ok(());
        }

        let order = self.order().limit(
            self.config.instrument_id,
            signal.side,
            quantity,
            entry_limit,
            Some(TimeInForce::Day),
            None,
            Some(false),
            Some(false),
            Some(false),
            None,
            None,
            None,
            None,
            None,
            Some(vec![Ustr::from("SLC_ENTRY")]),
            None,
        );
        let client_order_id = order.client_order_id();
        self.pending_entry = Some(PendingEntry {
            client_order_id,
            side: signal.side,
            entry_limit,
            stop,
            signal_ts: signal.ts_event,
            had_fill: false,
        });
        if let Err(e) = self.submit_order(order, None, None, None) {
            self.pending_entry = None;
            self.account_risk.release_unfilled(&symbol)?;
            return Err(e);
        }
        log::info!(
            "[{}] Submitted SLC {} entry: quantity={}, signal_close={}, entry_limit={}, stop={}, reserved_risk={}, reserved_notional={}",
            self.config.instrument_id,
            signal.side,
            quantity,
            signal.entry,
            entry_limit,
            stop,
            reservation.risk,
            reservation.notional,
        );
        Ok(())
    }

    /// Protects each entry fill immediately and derives the 2R target from average fill price.
    fn protect_entry_fill(&mut self, event: &OrderFilled) -> anyhow::Result<()> {
        let Some(pending) = self.pending_entry else {
            return Ok(());
        };
        if event.client_order_id != pending.client_order_id {
            return Ok(());
        }
        if let Some(active) = self.pending_entry.as_mut() {
            active.had_fill = true;
        }
        let exit_side = match pending.side {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
            OrderSide::NoOrderSide => anyhow::bail!("entry side is unspecified"),
        };
        anyhow::ensure!(
            match pending.side {
                OrderSide::Buy => event.last_px <= pending.entry_limit,
                OrderSide::Sell => event.last_px >= pending.entry_limit,
                OrderSide::NoOrderSide => false,
            },
            "entry fill exceeded the configured SLC slippage limit",
        );
        let previous_qty = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.filled_qty);
        let previous_notional = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.fill_notional);
        let filled_qty = previous_qty + event.last_qty.as_decimal();
        let fill_notional =
            previous_notional + event.last_px.as_decimal() * event.last_qty.as_decimal();
        let average_fill = fill_notional / filled_qty;
        let target = target_price(
            pending.side,
            average_fill,
            pending.stop,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.risk_reward,
        )?;
        let stop_order = self.order().market_if_touched(
            self.config.instrument_id,
            exit_side,
            event.last_qty,
            pending.stop,
            None,
            Some(TimeInForce::Day),
            None,
            Some(false),
            Some(false),
            None,
            None,
            None,
            None,
            Some(vec![Ustr::from("SLC_STOP")]),
            None,
        );
        self.submit_order(stop_order, None, None, None)?;
        let protected_qty = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.protected_qty)
            + event.last_qty.as_decimal();
        anyhow::ensure!(
            protected_qty == filled_qty,
            "SLC protected quantity does not equal filled quantity",
        );
        self.active_trade = Some(ActiveTrade {
            side: pending.side,
            target,
            filled_qty,
            protected_qty,
            fill_notional,
        });

        let entry_closed = self
            .cache()
            .order(&pending.client_order_id)
            .is_some_and(|order| order.is_closed());
        if entry_closed {
            self.pending_entry = None;
        }
        log::info!(
            "[{}] Protected SLC entry fill: last_quantity={}, total_quantity={}, average_fill={}, stop={}, target={}",
            self.config.instrument_id,
            event.last_qty,
            filled_qty,
            average_fill,
            pending.stop,
            target,
        );
        if self.exit_pending {
            self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        }
        Ok(())
    }

    /// Cancels an unfilled entry after the next completed five-minute bar makes it stale.
    fn cancel_stale_entry(&mut self, bar: Bar) -> anyhow::Result<()> {
        let Some(pending) = self.pending_entry else {
            return Ok(());
        };
        if bar.ts_event <= pending.signal_ts {
            return Ok(());
        }
        let cancelable = self
            .cache()
            .order(&pending.client_order_id)
            .is_some_and(|order| order.is_open() || order.is_inflight());
        if cancelable {
            log::warn!(
                "[{}] Canceling stale SLC entry {} after one completed bar",
                self.config.instrument_id,
                pending.client_order_id,
            );
            self.cancel_order(pending.client_order_id, None, None)?;
        }
        Ok(())
    }

    /// Returns whether a completed bar traded through the actual-fill-based 2R target.
    fn target_reached(&self, bar: Bar) -> bool {
        !self.exit_pending
            && self
                .active_trade
                .as_ref()
                .is_some_and(|active| match active.side {
                    OrderSide::Buy => bar.high >= active.target,
                    OrderSide::Sell => bar.low <= active.target,
                    OrderSide::NoOrderSide => false,
                })
    }

    /// Clears a terminal entry and releases risk only when no fill created exposure.
    fn clear_terminal_entry(&mut self, client_order_id: ClientOrderId) -> anyhow::Result<bool> {
        let Some(pending) = self
            .pending_entry
            .filter(|pending| pending.client_order_id == client_order_id)
        else {
            return Ok(false);
        };
        self.pending_entry = None;
        if self.active_trade.is_none() && !self.has_open_position() {
            if pending.had_fill {
                self.account_risk
                    .release_reservation(self.config.instrument_id.symbol.as_str())?;
            } else {
                self.account_risk
                    .release_unfilled(self.config.instrument_id.symbol.as_str())?;
            }
        }
        Ok(true)
    }

    /// Cancels all entry and stop orders before submitting a position-closing market order.
    fn request_exit(&mut self, reason: &str) -> anyhow::Result<()> {
        self.exit_pending = true;
        log::info!(
            "[{}] Requesting SLC position exit: {reason}",
            self.config.instrument_id,
        );
        if self.has_open_orders() {
            self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        } else {
            self.finish_exit()?;
        }
        Ok(())
    }

    /// Submits the closing order only after every prior order is confirmed closed.
    fn finish_exit(&mut self) -> anyhow::Result<()> {
        if !self.exit_pending || self.has_open_orders() {
            return Ok(());
        }
        if !self.has_open_position() {
            if let Some(pending) = self.pending_entry {
                self.clear_terminal_entry(pending.client_order_id)?;
            }
            self.exit_pending = false;
            return Ok(());
        }
        log::info!(
            "[{}] All SLC stop orders canceled; submitting market exit",
            self.config.instrument_id,
        );
        self.close_positions()
    }
}

impl Debug for SlcStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(SlcStrategy))
            .field("config", &self.config)
            .field("session_disabled", &self.session_disabled)
            .field("faulted", &self.faulted)
            .finish_non_exhaustive()
    }
}

nautilus_strategy!(SlcStrategy, {
    // Protects entry fills before processing any later lifecycle event
    fn on_order_filled(&mut self, event: &OrderFilled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        if let Err(e) = self.protect_entry_fill(event) {
            self.disable_after_order_failure(&format!(
                "failed to protect entry fill {}: {e:#}",
                event.client_order_id,
            ));
        }
    }

    // Releases unfilled entry risk and disables trading after a venue rejection
    fn on_order_rejected(&mut self, event: OrderRejected) {
        if event.instrument_id == self.config.instrument_id {
            if let Err(e) = self.clear_terminal_entry(event.client_order_id) {
                log::error!(
                    "[{}] Failed to release rejected SLC entry risk: {e:#}",
                    self.config.instrument_id,
                );
            }
            self.disable_after_order_failure(&event.reason);
        }
    }

    // Treats local risk denial as a terminal strategy fault
    fn on_order_denied(&mut self, event: OrderDenied) {
        if event.instrument_id == self.config.instrument_id {
            if let Err(e) = self.clear_terminal_entry(event.client_order_id) {
                log::error!(
                    "[{}] Failed to release denied SLC entry risk: {e:#}",
                    self.config.instrument_id,
                );
            }
            self.disable_after_order_failure(&event.reason);
        }
    }

    // Distinguishes an expiring entry remainder from an expiring protective order
    fn on_order_expired(&mut self, event: OrderExpired) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        match self.clear_terminal_entry(event.client_order_id) {
            Ok(true) => log::warn!(
                "[{}] SLC entry expired before its remaining quantity filled",
                self.config.instrument_id,
            ),
            Ok(false) if self.has_exposure() => {
                self.disable_after_order_failure(
                    "a live SLC order expired while exposure remained",
                );
            }
            Ok(false) => {}
            Err(e) => self.disable_after_order_failure(&format!(
                "failed to release expired SLC entry risk: {e:#}",
            )),
        }
    }

    // Advances cancel-then-close exits and tolerates the expected stale-entry cancel
    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        if self.exit_pending {
            if let Err(e) = self.finish_exit() {
                self.disable_after_order_failure(&format!("failed to submit SLC exit: {e:#}"));
            }
        } else if self
            .pending_entry
            .is_some_and(|pending| pending.client_order_id == event.client_order_id)
        {
            if let Err(e) = self.clear_terminal_entry(event.client_order_id) {
                self.disable_after_order_failure(&format!(
                    "failed to release canceled SLC entry risk: {e:#}",
                ));
            } else {
                log::info!(
                    "[{}] SLC entry remainder canceled; filled exposure remains protected={}",
                    self.config.instrument_id,
                    self.active_trade.is_some(),
                );
            }
        } else if !self.is_exiting() && self.has_open_position() {
            self.disable_after_order_failure("a protective SLC stop was canceled unexpectedly");
        }
    }

    // Escalates a rejected cancel to the framework managed-exit reconciler
    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.faulted = true;
            self.exit_pending = false;
            log::error!(
                "[{}] SLC order cancellation was rejected; switching to managed market-exit recovery: {}",
                self.config.instrument_id,
                event.reason,
            );
            if !self.is_exiting()
                && let Err(e) = self.market_exit()
            {
                log::error!(
                    "[{}] Failed to start managed SLC exit after cancel rejection: {e:#}",
                    self.config.instrument_id,
                );
            }
        }
    }

    // Persists account PnL while retaining risk for any still-live entry remainder
    fn on_position_closed(&mut self, event: PositionClosed) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        let entry_remainder_open = self.pending_entry.is_some_and(|pending| {
            self.cache()
                .order(&pending.client_order_id)
                .is_some_and(|order| order.is_open() || order.is_inflight())
        });
        if !entry_remainder_open {
            self.pending_entry = None;
        }
        self.active_trade = None;
        self.exit_pending = entry_remainder_open;
        if let Err(e) = self.cancel_all_orders(self.config.instrument_id, None, None, None) {
            log::error!(
                "[{}] Failed to cancel residual SLC orders after position close: {e:#}",
                self.config.instrument_id,
            );
            self.faulted = true;
        }
        let local_date = event
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone())
            .date();
        let realized_pnl = event.realized_pnl.map(|pnl| pnl.as_decimal());
        match self.account_risk.record_close(
            self.config.instrument_id.symbol.as_str(),
            local_date,
            realized_pnl,
            !entry_remainder_open,
        ) {
            Ok(snapshot) => {
                self.session_disabled |= snapshot.halted;
                log::info!(
                    "[{}] SLC position closed: realized_pnl={:?}, account_halted={}, account_daily_pnl={}, open_risk={}, account_notional={}, open_positions={}",
                    self.config.instrument_id,
                    realized_pnl,
                    snapshot.halted,
                    snapshot.realized_pnl,
                    snapshot.open_risk,
                    snapshot.account_notional,
                    snapshot.open_positions,
                );
            }
            Err(e) => {
                self.faulted = true;
                log::error!(
                    "[{}] Failed to persist SLC account risk after close: {e:#}",
                    self.config.instrument_id,
                );
            }
        }
    }
});

impl DataActor for SlcStrategy {
    /// Validates data contracts, restores shared risk state, and starts both bar subscriptions.
    fn on_start(&mut self) -> anyhow::Result<()> {
        validate_bar_type(self.config.five_minute_bar_type, 5, BarAggregation::Minute)?;
        validate_bar_type(self.config.four_hour_bar_type, 4, BarAggregation::Hour)?;
        anyhow::ensure!(
            self.config.entry_start_minute < self.config.entry_end_minute,
            "effective entry window ends before it starts on this trading day",
        );
        self.cache().try_instrument(&self.config.instrument_id)?;
        let local_date = Timestamp::now()
            .to_zoned(self.config.timezone.clone())
            .date();
        let has_exposure = self.has_exposure();
        let snapshot = self.account_risk.reconcile_symbol(
            self.config.instrument_id.symbol.as_str(),
            local_date,
            has_exposure,
        )?;
        self.current_date = Some(local_date);
        self.session_disabled = snapshot.halted;
        self.subscribe_bars(self.config.five_minute_bar_type, None, None);
        self.subscribe_bars(self.config.four_hour_bar_type, None, None);
        self.subscribe_quotes(self.config.instrument_id, None, None);
        log::info!(
            "[{}] SLC subscriptions active: quotes=true, 5m={}, 4h={}, entry_window={:02}:{:02}-{:02}:{:02}, flatten={:02}:{:02}, account_halted={}, account_daily_pnl={}, open_risk={}, account_notional={}, open_positions={}, symbol_entries={}",
            self.config.instrument_id,
            self.config.five_minute_bar_type,
            self.config.four_hour_bar_type,
            self.config.entry_start_minute / 60,
            self.config.entry_start_minute % 60,
            self.config.entry_end_minute / 60,
            self.config.entry_end_minute % 60,
            self.config.flatten_minute / 60,
            self.config.flatten_minute % 60,
            snapshot.halted,
            snapshot.realized_pnl,
            snapshot.open_risk,
            snapshot.account_notional,
            snapshot.open_positions,
            snapshot.entries_for_symbol,
        );
        if has_exposure {
            log::warn!(
                "[{}] Flattening reconciled SLC exposure before accepting new signals",
                self.config.instrument_id,
            );
            self.request_exit("reconciled startup exposure")?;
        }
        Ok(())
    }

    /// Removes subscriptions after managed-stop has reconciled orders and positions.
    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.config.five_minute_bar_type, None, None);
        self.unsubscribe_bars(self.config.four_hour_bar_type, None, None);
        self.unsubscribe_quotes(self.config.instrument_id, None, None);
        Ok(())
    }

    /// Starts the existing cancel-then-close exit when the executable quote reaches 2R.
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if quote.instrument_id != self.config.instrument_id
            || self.faulted
            || self.exit_pending
            || self.is_exiting()
            || !self.has_open_position()
        {
            return Ok(());
        }
        let Some(active) = self.active_trade.as_ref() else {
            return Ok(());
        };
        if !quote_reaches_target(active.side, active.target, quote) {
            return Ok(());
        }
        log::info!(
            "[{}] Realtime 2R target reached: side={}, target={}, bid={}, ask={}, ts_event={}",
            self.config.instrument_id,
            active.side,
            active.target,
            quote.bid_price,
            quote.ask_price,
            quote.ts_event,
        );
        self.request_exit("executable top-of-book quote reached the actual-fill-based 2R target")
    }

    /// Routes completed bars through structure, data-integrity, risk, and execution gates.
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if bar.bar_type == self.config.four_hour_bar_type {
            let Some(finalized) = self.signals.finalize_four_hour(*bar) else {
                return Ok(());
            };
            let local = finalized
                .ts_event
                .to_datetime_utc()
                .to_zoned(self.config.timezone.clone());
            let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
            if (RTH_OPEN_MINUTE..RTH_CLOSE_MINUTE).contains(&minute) {
                self.signals.process_four_hour(finalized);
                log::info!(
                    "[{}] 4h structure updated: start={}, open={}, high={}, low={}, close={}, trend={:?}",
                    self.config.instrument_id,
                    local,
                    finalized.open,
                    finalized.high,
                    finalized.low,
                    finalized.close,
                    self.signals.structure.trend(),
                );
            }
            return Ok(());
        }
        if bar.bar_type != self.config.five_minute_bar_type {
            return Ok(());
        }
        let Some(finalized) = self.signals.finalize_five_minute(*bar) else {
            return Ok(());
        };
        let suppress_signal = self.suppress_warmup_boundary_signal;
        self.suppress_warmup_boundary_signal = false;
        let local = finalized
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
        let close_minute = minute.saturating_add(FIVE_MINUTES);
        if !(RTH_OPEN_MINUTE..RTH_CLOSE_MINUTE).contains(&minute) {
            return Ok(());
        }
        let new_date = self.current_date != Some(local.date());
        if new_date {
            self.current_date = Some(local.date());
            self.last_five_minute_bar_start = None;
            self.session_disabled = false;
            let has_exposure = self.has_exposure();
            let snapshot = self.account_risk.reconcile_symbol(
                self.config.instrument_id.symbol.as_str(),
                local.date(),
                has_exposure,
            )?;
            self.session_disabled = snapshot.halted;
            if has_exposure {
                self.request_exit("unexpected overnight exposure")?;
            }
        }
        if let Some(previous) = self.last_five_minute_bar_start
            && has_five_minute_gap(previous, finalized.ts_event)
        {
            self.session_disabled = true;
            log::error!(
                "[{}] SLC 5m data gap detected: previous={}, current={}; disabling entries for the session",
                self.config.instrument_id,
                previous,
                finalized.ts_event,
            );
            if self.has_exposure() {
                self.request_exit("five-minute market data gap")?;
            }
        }
        self.last_five_minute_bar_start = Some(finalized.ts_event);
        let within_entry_window = close_minute >= self.config.entry_start_minute
            && close_minute <= self.config.entry_end_minute;
        let allow_signal = within_entry_window
            && !suppress_signal
            && !self.faulted
            && !self.session_disabled
            && !self.exit_pending
            && !self.has_exposure();
        let signal = self.signals.process_five_minute(finalized, allow_signal);
        log::info!(
            "[{}] 5m bar collected: start={}, open={}, high={}, low={}, close={}, volume={}, 4h_trend={:?}, atr={:.6}, stochastic_k={:.2}, demand_zones={}, supply_zones={}, data_ready={}, entries_disabled={}",
            self.config.instrument_id,
            local,
            finalized.open,
            finalized.high,
            finalized.low,
            finalized.close,
            finalized.volume,
            self.signals.structure.trend(),
            self.signals.atr.value,
            self.signals.stochastics.value_k,
            self.signals.demand.len(),
            self.signals.supply.len(),
            self.signals.ready(),
            self.session_disabled,
        );
        self.cancel_stale_entry(finalized)?;
        if close_minute >= self.config.flatten_minute {
            self.session_disabled = true;
            self.request_exit("pre-close risk cutoff")?;
            return Ok(());
        }
        if self.target_reached(finalized) {
            self.request_exit("five-minute bar traded through the actual-fill-based 2R target")?;
            return Ok(());
        }
        if self.faulted
            || self.exit_pending
            || self.session_disabled
            || !within_entry_window
            || self.has_exposure()
        {
            return Ok(());
        }
        if let Some(signal) = signal
            && let Err(e) = self.submit_signal(signal, local.date())
        {
            self.faulted = true;
            return Err(e);
        }
        Ok(())
    }
}

/// Validates the exact external LAST bar contract required by the SLC state machine.
fn validate_bar_type(
    bar_type: BarType,
    step: usize,
    aggregation: BarAggregation,
) -> anyhow::Result<()> {
    let spec = bar_type.spec();
    anyhow::ensure!(
        spec.step.get() == step
            && spec.aggregation == aggregation
            && spec.price_type == PriceType::Last
            && bar_type.aggregation_source() == AggregationSource::External,
        "invalid SLC bar type {bar_type}",
    );
    Ok(())
}

/// Returns the zone stop and marketable entry limit at the configured worst slippage.
fn entry_prices(
    signal: Signal,
    increment: Price,
    precision: u8,
    stop_buffer_ticks: u64,
    max_entry_slippage_ticks: u64,
) -> anyhow::Result<(Price, Price)> {
    let increment = increment.as_decimal();
    let stop_buffer = increment * Decimal::from(stop_buffer_ticks);
    let entry_buffer = increment * Decimal::from(max_entry_slippage_ticks);
    let (stop, entry_limit) = match signal.side {
        OrderSide::Buy => {
            let stop = signal.zone_low.as_decimal() - stop_buffer;
            let entry_limit = signal.entry.as_decimal() + entry_buffer;
            anyhow::ensure!(stop < entry_limit, "long stop must be below entry limit");
            (stop, entry_limit)
        }
        OrderSide::Sell => {
            let stop = signal.zone_high.as_decimal() + stop_buffer;
            let entry_limit = signal.entry.as_decimal() - entry_buffer;
            anyhow::ensure!(stop > entry_limit, "short stop must be above entry limit");
            (stop, entry_limit)
        }
        OrderSide::NoOrderSide => anyhow::bail!("signal side is unspecified"),
    };
    anyhow::ensure!(stop > Decimal::ZERO, "stop price must be positive");
    anyhow::ensure!(entry_limit > Decimal::ZERO, "entry limit must be positive");
    Ok((
        Price::from_decimal_dp(stop, precision)?,
        Price::from_decimal_dp(entry_limit, precision)?,
    ))
}

/// Returns a tick-aligned target at or beyond the configured reward multiple from actual fill.
fn target_price(
    side: OrderSide,
    entry: Decimal,
    stop: Price,
    increment: Price,
    precision: u8,
    risk_reward: Decimal,
) -> anyhow::Result<Price> {
    let stop = stop.as_decimal();
    let increment = increment.as_decimal();
    let target = match side {
        OrderSide::Buy => {
            let risk = entry - stop;
            anyhow::ensure!(risk > Decimal::ZERO, "long fill must be above stop");
            ((entry + risk * risk_reward) / increment).ceil() * increment
        }
        OrderSide::Sell => {
            let risk = stop - entry;
            anyhow::ensure!(risk > Decimal::ZERO, "short fill must be below stop");
            ((entry - risk * risk_reward) / increment).floor() * increment
        }
        OrderSide::NoOrderSide => anyhow::bail!("entry side is unspecified"),
    };
    anyhow::ensure!(target > Decimal::ZERO, "target price must be positive");
    Ok(Price::from_decimal_dp(target, precision)?)
}

/// Returns whether the immediately executable quote side has reached the target.
fn quote_reaches_target(side: OrderSide, target: Price, quote: &QuoteTick) -> bool {
    match side {
        OrderSide::Buy => quote.bid_price >= target,
        OrderSide::Sell => quote.ask_price <= target,
        OrderSide::NoOrderSide => false,
    }
}

/// Returns the largest lot-aligned quantity whose worst-price stop risk fits the budget.
fn risk_sized_quantity(
    entry: Price,
    stop: Price,
    risk_amount: Decimal,
    max_quantity: Quantity,
    lot_size: Quantity,
) -> anyhow::Result<Option<Quantity>> {
    let risk_per_share = (entry.as_decimal() - stop.as_decimal()).abs();
    anyhow::ensure!(
        risk_per_share > Decimal::ZERO,
        "risk per share must be positive"
    );
    let lot = lot_size.as_decimal();
    anyhow::ensure!(lot > Decimal::ZERO, "instrument lot size must be positive");
    let risk_quantity = ((risk_amount / risk_per_share) / lot).floor() * lot;
    let capped = risk_quantity.min(max_quantity.as_decimal());
    let normalized = (capped / lot).floor() * lot;
    if normalized <= Decimal::ZERO {
        return Ok(None);
    }
    Ok(Some(Quantity::from_decimal(normalized)?))
}

struct PreparedInputs {
    instrument_id: InstrumentId,
    instrument: InstrumentAny,
    five_minute_bars: Vec<Bar>,
    four_hour_bars: Vec<Bar>,
    market_close: Timestamp,
    market_close_minute: u16,
}

/// Loads exact instrument metadata and complete warmup histories before the live node starts.
async fn prepare_inputs(
    config: &AppConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<PreparedInputs>> {
    let symbols = config
        .instruments
        .iter()
        .map(|instrument| instrument.instrument_id.symbol.as_str())
        .collect::<Vec<_>>();
    let static_info = quote_api_call(context.static_info(symbols.clone()))
        .await
        .context("failed to request Longbridge static security info")?;
    let mut static_info_by_symbol = static_info
        .into_iter()
        .map(|info| (info.symbol.clone(), info))
        .collect::<HashMap<_, _>>();

    let (market_close, market_close_minute) = current_us_market_close(context).await?;
    let mut prepared = Vec::with_capacity(config.instruments.len());

    for configured in &config.instruments {
        let instrument_id = configured.instrument_id;
        let symbol = instrument_id.symbol.as_str();
        let static_security_info = static_info_by_symbol.remove(symbol).with_context(|| {
            format!("Longbridge did not return exact static security info for {symbol}")
        })?;
        let instrument = parse_instrument(
            &static_security_info,
            configured.price_increment,
            UnixNanos::default(),
        )?;
        anyhow::ensure!(
            instrument.quote_currency() == Currency::USD(),
            "SLC risk controls currently require a USD-quoted equity: {instrument_id}",
        );

        let five_minute_bars = load_warmup_bars(
            context,
            symbol,
            Period::FiveMinute,
            AppConfig::five_minute_bar_type(instrument_id),
            config.five_minute_warmup,
        )
        .await?;
        let four_hour_bars = load_warmup_bars(
            context,
            symbol,
            Period::FourHour,
            AppConfig::four_hour_bar_type(instrument_id),
            config.four_hour_warmup,
        )
        .await?;

        prepared.push(PreparedInputs {
            instrument_id,
            instrument,
            five_minute_bars,
            four_hour_bars,
            market_close,
            market_close_minute,
        });
    }

    Ok(prepared)
}

/// Loads a complete, deduplicated, recent regular-session warmup for one bar period.
async fn load_warmup_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    count: usize,
) -> anyhow::Result<Vec<Bar>> {
    let mut bars = quote_api_call(context.candlesticks(
        symbol,
        period,
        count,
        AdjustType::NoAdjust,
        TradeSessions::Intraday,
    ))
    .await
    .with_context(|| format!("failed to request {period:?} warmup bars for {symbol}"))?
    .into_iter()
    .map(|candlestick| parse_bar(bar_type, candlestick, UnixNanos::default()))
    .collect::<anyhow::Result<Vec<_>>>()?;
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    anyhow::ensure!(
        bars.len() >= count,
        "Longbridge returned {} of {count} required {period:?} warmup bars for {symbol}",
        bars.len(),
    );
    let latest = bars
        .last()
        .expect("warmup count was validated above")
        .ts_event;
    let now = UnixNanos::from(Timestamp::now());
    anyhow::ensure!(
        latest <= now && now.as_u64().saturating_sub(latest.as_u64()) <= MAX_WARMUP_AGE_NANOS,
        "Longbridge returned stale or future {period:?} warmup data for {symbol}: latest={latest}",
    );
    Ok(bars)
}

/// Returns today's authoritative Longbridge US regular-session close and local minute.
async fn current_us_market_close(context: &QuoteContext) -> anyhow::Result<(Timestamp, u16)> {
    let now = Timestamp::now();
    let market_date = us_market_date(now)?;
    let trading_days = quote_api_call(context.trading_days(Market::US, market_date, market_date))
        .await
        .context("failed to query the current US trading day from Longbridge")?;
    if !trading_days.trading_days.contains(&market_date) {
        anyhow::bail!("{market_date} is not a US trading day");
    }

    let close_time = if trading_days.half_trading_days.contains(&market_date) {
        time::macros::time!(13:00)
    } else {
        quote_api_call(context.trading_session())
            .await
            .context("failed to query the current US trading session from Longbridge")?
            .into_iter()
            .find(|session| session.market == Market::US)
            .and_then(|session| {
                session
                    .trade_sessions
                    .into_iter()
                    .find(|session| session.trade_session == TradeSession::Intraday)
            })
            .map(|session| session.end_time)
            .context("Longbridge did not return a US regular trading session")?
    };
    let market_close_minute = u16::from(close_time.hour()) * 60 + u16::from(close_time.minute());
    Ok((us_market_close_at(now, close_time)?, market_close_minute))
}

/// Converts a timestamp to its US market calendar date.
fn us_market_date(now: Timestamp) -> anyhow::Result<Date> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let local_date = now.to_zoned(timezone).date();
    Ok(Date::from_calendar_date(
        i32::from(local_date.year()),
        Month::try_from(u8::try_from(local_date.month())?)?,
        u8::try_from(local_date.day())?,
    )?)
}

/// Resolves a local US market close across daylight-saving transitions.
fn us_market_close_at(now: Timestamp, close_time: Time) -> anyhow::Result<Timestamp> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let local_date = now.to_zoned(timezone.clone()).date();
    let civil_close_time = CivilTime::new(
        i8::try_from(close_time.hour())?,
        i8::try_from(close_time.minute())?,
        i8::try_from(close_time.second())?,
        i32::try_from(close_time.nanosecond())?,
    )?;
    timezone
        .to_ambiguous_timestamp(local_date.to_datetime(civil_close_time))
        .unambiguous()
        .context("US market close must resolve to one timestamp")
}

/// Stops the node at the session close after strategies have used their pre-close exit window.
fn schedule_market_close_stop(node: &LiveNode, market_close: Timestamp) -> anyhow::Result<()> {
    let delay = Duration::try_from(Timestamp::now().duration_until(market_close))
        .context("US market close must be in the future")?;
    let handle = node.handle();
    log::info!("Longbridge SLC node will stop at market close {market_close}");
    get_runtime().spawn(async move {
        tokio::time::sleep(delay).await;
        handle.stop();
    });
    Ok(())
}

/// Builds Longbridge execution settings with live routing and outside-RTH trading disabled by default.
fn execution_config(config: &AppConfig) -> LongbridgeExecClientConfig {
    LongbridgeExecClientConfig {
        account_type: AccountType::Margin,
        papertrading: config.papertrading,
        outside_rth: false,
        ..Default::default()
    }
}

/// Adds engine-level order throttles and exact per-order notional limits.
fn risk_engine_config(config: &AppConfig) -> LiveRiskEngineConfig {
    LiveRiskEngineConfig {
        bypass: false,
        max_order_submit_rate: "6/00:00:01".to_string(),
        max_order_modify_rate: "6/00:00:01".to_string(),
        max_notional_per_order: config
            .instruments
            .iter()
            .map(|instrument| {
                (
                    instrument.instrument_id.to_string(),
                    config.max_order_notional.to_string(),
                )
            })
            .collect(),
        ..Default::default()
    }
}

/// Prepares all symbols before constructing the reconciled live trading node.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AppConfig::from_env()?;
    let account_risk = Arc::new(AccountRisk::load(config.risk_state_path.clone())?);
    log::info!(
        "SLC account risk state loaded from {}",
        config.risk_state_path.display(),
    );
    let prepared = {
        let sdk_config = config.data_config().sdk_config().await?;
        let (context, _receiver) = QuoteContext::new(sdk_config);
        prepare_inputs(&config, &context).await?
    };
    let market_close = prepared
        .first()
        .context("SLC requires at least one prepared instrument")?
        .market_close;
    let market_close_minute = prepared
        .first()
        .expect("prepared instruments checked above")
        .market_close_minute;
    let flatten_minute = market_close_minute
        .checked_sub(config.session.flatten_before_close_minutes)
        .context("flatten buffer exceeds the current US trading session")?;
    anyhow::ensure!(
        config.session.entry_start_minute < flatten_minute,
        "entry window starts after today's pre-close flatten time",
    );

    let environment = if config.papertrading {
        Environment::Sandbox
    } else {
        Environment::Live
    };
    let trader_id = TraderId::from(TRADER_ID);
    let account_id = AccountId::from(ACCOUNT_ID);
    let exec_engine_config = LiveExecEngineConfig {
        reconciliation_lookback_mins: Some(24 * 60),
        reconciliation_instrument_ids: Some(
            config
                .instruments
                .iter()
                .map(|instrument| instrument.instrument_id.to_string())
                .collect(),
        ),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(15.0),
        ..Default::default()
    };
    let mut node = LiveNode::builder(trader_id, environment)?
        .with_name(NODE_NAME.to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_exec_engine_config(exec_engine_config)
        .with_risk_engine_config(risk_engine_config(&config))
        .with_reconciliation(true)
        .with_delay_post_stop_secs(10)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(config.data_config()),
        )?
        .add_exec_client(
            None,
            Box::new(LongbridgeExecutionClientFactory::new(trader_id, account_id)),
            Box::new(execution_config(&config)),
        )?
        .build()?;

    for prepared in prepared {
        node.kernel()
            .cache()
            .borrow_mut()
            .add_instrument(prepared.instrument.clone())?;
        node.add_strategy(SlcStrategy::new(
            &config,
            prepared.instrument_id,
            prepared.instrument,
            prepared.five_minute_bars,
            prepared.four_hour_bars,
            flatten_minute,
            Arc::clone(&account_risk),
        )?)?;
    }
    schedule_market_close_stop(&node, market_close)?;
    node.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(
        bar_type: BarType,
        open: &str,
        high: &str,
        low: &str,
        close: &str,
        timestamp: u64,
    ) -> Bar {
        Bar::new(
            bar_type,
            Price::from(open),
            Price::from(high),
            Price::from(low),
            Price::from(close),
            Quantity::from(100),
            UnixNanos::from(timestamp),
            UnixNanos::from(timestamp),
        )
    }

    fn five_minute_bar(open: &str, high: &str, low: &str, close: &str, timestamp: u64) -> Bar {
        bar(
            BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            open,
            high,
            low,
            close,
            timestamp,
        )
    }

    fn quote(bid: &str, ask: &str, timestamp: u64) -> QuoteTick {
        QuoteTick::new(
            InstrumentId::from("QQQ.US.LONGBRIDGE"),
            Price::from(bid),
            Price::from(ask),
            Quantity::from(100),
            Quantity::from(100),
            UnixNanos::from(timestamp),
            UnixNanos::from(timestamp),
        )
    }

    fn signal_rules() -> SignalRules {
        SignalRules {
            zone_ttl_bars: 78,
            max_zones_per_side: 3,
            confirmation_window_bars: 3,
            displacement_atr_multiple: 1.5,
            displacement_close_fraction: 0.25,
            oversold: 20.0,
            overbought: 80.0,
        }
    }

    #[rstest::rstest]
    fn test_parse_multiple_symbol_configs() {
        let instruments = parse_symbol_config(
            r#"
                [[symbols]]
                symbol = "QQQ.US"
                price_increment = "0.01"

                [[symbols]]
                symbol = "AAPL.US"
                price_increment = "0.01"

                [[symbols]]
                symbol = "MSFT.US"
                price_increment = "0.01"
            "#,
        )
        .unwrap();

        assert_eq!(instruments.len(), 3);
        assert_eq!(instruments[0].instrument_id.symbol.as_str(), "QQQ.US");
        assert_eq!(instruments[1].instrument_id.symbol.as_str(), "AAPL.US");
        assert_eq!(instruments[2].instrument_id.symbol.as_str(), "MSFT.US");
        assert_eq!(instruments[0].price_increment, Price::from("0.01"));
    }

    #[rstest::rstest]
    fn test_parse_symbol_config_rejects_duplicates() {
        assert!(
            parse_symbol_config(
                r#"
                    [[symbols]]
                    symbol = "QQQ.US"
                    price_increment = "0.01"

                    [[symbols]]
                    symbol = "QQQ.US"
                    price_increment = "0.01"
                "#,
            )
            .is_err()
        );
    }

    #[rstest::rstest]
    fn test_parse_symbol_config_rejects_non_us_equities() {
        assert!(
            parse_symbol_config(
                r#"
                    [[symbols]]
                    symbol = "0700.HK"
                    price_increment = "0.001"
                "#,
            )
            .is_err()
        );
    }

    #[rstest::rstest]
    fn test_live_guard_requires_explicit_acknowledgement() {
        assert!(validate_live_guard(true, None).is_ok());
        assert!(validate_live_guard(false, None).is_err());
        assert!(validate_live_guard(false, Some(LIVE_ACK)).is_ok());
    }

    #[rstest::rstest]
    fn test_final_bar_buffer_replaces_updates_and_emits_once() {
        let mut buffer = FinalBarBuffer::default();
        let partial = five_minute_bar("100", "101", "99", "100", 1);
        let updated = five_minute_bar("100", "102", "99", "101", 1);
        let next = five_minute_bar("101", "103", "100", "102", 2);

        assert_eq!(buffer.update(partial), None);
        assert_eq!(buffer.update(updated), None);
        assert_eq!(buffer.update(next), Some(updated));
    }

    #[rstest::rstest]
    fn test_five_minute_gap_detection() {
        let first = UnixNanos::from(1_000_000_000);

        assert!(!has_five_minute_gap(
            first,
            UnixNanos::from(first.as_u64() + FIVE_MINUTE_NANOS),
        ));
        assert!(has_five_minute_gap(
            first,
            UnixNanos::from(first.as_u64() + FIVE_MINUTE_NANOS * 2),
        ));
    }

    #[rstest::rstest]
    fn test_supply_and_demand_displacement_rules() {
        let rules = signal_rules();
        let bearish_base = five_minute_bar("100.0", "101.0", "98.0", "99.0", 1);
        let bullish_displacement = five_minute_bar("99.0", "104.0", "99.0", "103.5", 2);
        let bullish_base = five_minute_bar("100.0", "102.0", "99.0", "101.0", 3);
        let bearish_displacement = five_minute_bar("101.0", "101.0", "96.0", "96.5", 4);

        assert!(is_up_displacement(
            bearish_base,
            bullish_displacement,
            2.0,
            rules,
        ));
        assert!(is_down_displacement(
            bullish_base,
            bearish_displacement,
            2.0,
            rules,
        ));
    }

    #[rstest::rstest]
    fn test_zone_confirmation_expires_after_configured_window() {
        let rules = SignalRules {
            confirmation_window_bars: 1,
            ..signal_rules()
        };
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch = five_minute_bar("102", "102", "100", "101", 2);
        let later = five_minute_bar("102", "103", "102", "103", 3);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Demand, source)]);
        let no_confirmation = Confirmation {
            extreme: false,
            reentry: false,
        };

        assert_eq!(
            observe_zones(
                &mut zones,
                touch,
                no_confirmation,
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert_eq!(zones.len(), 1);
        assert_eq!(
            observe_zones(
                &mut zones,
                later,
                no_confirmation,
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_zone_requires_extreme_after_touch_before_reentry() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch_and_reentry = five_minute_bar("102", "102", "100", "101", 2);
        let extreme = five_minute_bar("101", "102", "100", "101", 3);
        let confirmed = five_minute_bar("101", "102", "100", "101", 4);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Demand, source)]);

        assert_eq!(
            observe_zones(
                &mut zones,
                touch_and_reentry,
                Confirmation {
                    extreme: false,
                    reentry: true,
                },
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert_eq!(
            observe_zones(
                &mut zones,
                extreme,
                Confirmation {
                    extreme: true,
                    reentry: false,
                },
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert!(
            observe_zones(
                &mut zones,
                confirmed,
                Confirmation {
                    extreme: false,
                    reentry: true,
                },
                true,
                OrderSide::Buy,
                rules,
            )
            .is_some(),
        );
    }

    #[rstest::rstest]
    fn test_broken_supply_reclaims_and_confirms_once() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let broken = five_minute_bar("100", "103", "100", "102", 2);
        let still_above = five_minute_bar("102", "104", "101", "103", 3);
        let reclaimed = five_minute_bar("102", "102", "98", "98.5", 4);
        let retest = five_minute_bar("98", "100", "97", "99.5", 5);
        let confirmed = five_minute_bar("99", "100", "97", "98", 6);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Supply, source)]);
        let idle = Confirmation {
            extreme: false,
            reentry: false,
        };

        assert_eq!(
            observe_zones(&mut zones, broken, idle, true, OrderSide::Sell, rules),
            None,
        );
        assert_eq!(zones[0].state, ZoneState::BrokenOnce);
        assert_eq!(
            observe_zones(&mut zones, still_above, idle, true, OrderSide::Sell, rules,),
            None,
        );
        assert_eq!(
            observe_zones(&mut zones, reclaimed, idle, true, OrderSide::Sell, rules,),
            None,
        );
        assert_eq!(zones[0].state, ZoneState::Reclaimed);
        assert_eq!(
            observe_zones(
                &mut zones,
                retest,
                Confirmation {
                    extreme: true,
                    reentry: false,
                },
                true,
                OrderSide::Sell,
                rules,
            ),
            None,
        );
        let signal = observe_zones(
            &mut zones,
            confirmed,
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Sell,
            rules,
        );

        assert_eq!(signal.map(|signal| signal.side), Some(OrderSide::Sell));
        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_reclaimed_supply_is_invalid_after_second_break() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let broken = five_minute_bar("100", "103", "100", "102", 2);
        let reclaimed = five_minute_bar("102", "102", "98", "98.5", 3);
        let second_break = five_minute_bar("99", "103", "99", "102", 4);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Supply, source)]);
        let idle = Confirmation {
            extreme: false,
            reentry: false,
        };

        let _ = observe_zones(&mut zones, broken, idle, true, OrderSide::Sell, rules);
        let _ = observe_zones(&mut zones, reclaimed, idle, true, OrderSide::Sell, rules);
        let _ = observe_zones(&mut zones, second_break, idle, true, OrderSide::Sell, rules);

        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_entry_target_and_risk_sizing_use_exact_decimals() {
        let signal = Signal {
            side: OrderSide::Buy,
            entry: Price::from("100.00"),
            zone_low: Price::from("98.00"),
            zone_high: Price::from("99.00"),
            ts_event: UnixNanos::from(1),
        };
        let (stop, entry_limit) = entry_prices(signal, Price::from("0.01"), 2, 1, 5).unwrap();
        let quantity = risk_sized_quantity(
            entry_limit,
            stop,
            Decimal::from(25),
            Quantity::from(20),
            Quantity::from(1),
        )
        .unwrap();
        let target = target_price(
            OrderSide::Buy,
            Decimal::from_str("100.03").unwrap(),
            stop,
            Price::from("0.01"),
            2,
            Decimal::from(2),
        )
        .unwrap();

        assert_eq!(stop, Price::from("97.99"));
        assert_eq!(entry_limit, Price::from("100.05"));
        assert_eq!(target, Price::from("104.11"));
        assert_eq!(quantity, Some(Quantity::from(12)));
    }

    #[rstest::rstest]
    fn test_realtime_target_uses_executable_quote_side() {
        let target = Price::from("100.00");

        assert!(!quote_reaches_target(
            OrderSide::Buy,
            target,
            &quote("99.99", "100.05", 1),
        ));
        assert!(quote_reaches_target(
            OrderSide::Buy,
            target,
            &quote("100.00", "100.05", 2),
        ));
        assert!(!quote_reaches_target(
            OrderSide::Sell,
            target,
            &quote("99.95", "100.01", 3),
        ));
        assert!(quote_reaches_target(
            OrderSide::Sell,
            target,
            &quote("99.95", "100.00", 4),
        ));
    }

    #[rstest::rstest]
    fn test_account_daily_loss_persists_across_restart() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()),
        ));
        let date = "2026-09-01".parse().unwrap();
        let limits = AccountRiskLimits {
            daily_loss: Decimal::from(50),
            open_risk: Decimal::from(100),
            account_notional: Decimal::from(10_000),
            open_positions: 1,
        };
        let reservation = RiskReservation {
            risk: Decimal::from(25),
            notional: Decimal::from(1_000),
        };
        let risk = AccountRisk::load(path.clone()).unwrap();

        assert_eq!(
            risk.reserve_entry("QQQ.US", date, reservation, 1, limits)
                .unwrap()
                .0,
            ReservationOutcome::Reserved,
        );
        assert_eq!(
            risk.reserve_entry("AAPL.US", date, reservation, 1, limits)
                .unwrap()
                .0,
            ReservationOutcome::Rejected,
        );
        risk.record_close("QQQ.US", date, Some(Decimal::from(-50)), true)
            .unwrap();
        drop(risk);
        let restored = AccountRisk::load(path.clone()).unwrap();
        let (outcome, snapshot) = restored
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();

        assert_eq!(outcome, ReservationOutcome::Rejected);
        assert_eq!(snapshot.realized_pnl, Decimal::from(-50));
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    fn test_missing_realized_pnl_halts_entries_until_next_session() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-missing-pnl-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()).as_u64(),
        ));
        let date = "2026-09-01".parse().unwrap();
        let next_date = "2026-09-02".parse().unwrap();
        let limits = AccountRiskLimits {
            daily_loss: Decimal::from(50),
            open_risk: Decimal::from(100),
            account_notional: Decimal::from(10_000),
            open_positions: 1,
        };
        let reservation = RiskReservation {
            risk: Decimal::from(25),
            notional: Decimal::from(1_000),
        };
        let risk = AccountRisk::load(path.clone()).unwrap();

        risk.reserve_entry("QQQ.US", date, reservation, 1, limits)
            .unwrap();
        let halted = risk.record_close("QQQ.US", date, None, true).unwrap();
        let same_day = risk
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();
        let next_day = risk
            .reserve_entry("AAPL.US", next_date, reservation, 1, limits)
            .unwrap();

        assert!(halted.halted);
        assert_eq!(same_day.0, ReservationOutcome::Rejected);
        assert_eq!(next_day.0, ReservationOutcome::Reserved);
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    fn test_unreserved_reconciled_exposure_halts_account() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-untracked-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()).as_u64(),
        ));
        let date = "2026-09-01".parse().unwrap();
        let risk = AccountRisk::load(path.clone()).unwrap();

        let snapshot = risk.reconcile_symbol("QQQ.US", date, true).unwrap();

        assert!(snapshot.halted);
        assert_eq!(snapshot.open_positions, 0);
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    #[case("2026-06-29T14:00:00Z", 16, "2026-06-29T20:00:00Z")]
    #[case("2026-01-16T15:00:00Z", 16, "2026-01-16T21:00:00Z")]
    fn test_us_market_close_handles_dst(
        #[case] now: &str,
        #[case] close_hour: u8,
        #[case] expected: &str,
    ) {
        let now = now.parse::<Timestamp>().unwrap();
        let close_time = Time::from_hms(close_hour, 0, 0).unwrap();

        assert_eq!(
            us_market_close_at(now, close_time).unwrap(),
            expected.parse::<Timestamp>().unwrap(),
        );
    }
}
