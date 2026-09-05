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

//! Runs live and backtest Structure-Level-Confirmation strategies with Longbridge stocks.
//!
//! WARNING: This example submits orders. It defaults to Longbridge paper trading. Setting
//! `LONGBRIDGE_SLC_PAPERTRADING=false` routes orders to a live margin account and additionally
//! requires `LONGBRIDGE_SLC_LIVE_ACK=I_UNDERSTAND_LIVE_ORDERS`.
//!
//! The strategy trades completed five-minute bars during the US regular session. It combines
//! confirmed four-hour higher-high/higher-low or lower-high/lower-low structure, fresh supply or
//! demand zones formed before one-to-three-bar ATR-sized displacement moves, one-break
//! reclaim/retest levels, and
//! configurable Stochastics re-entry from the 20/80 bands. Each signal submits a one-bar marketable
//! limit entry sized at its worst allowed price. Every fill receives a broker-hosted Longbridge
//! market-if-touched stop in live trading. Backtests submit a linked stop-market and resting 2R
//! limit target for every fill. Live targets are recalculated from average fill price and checked
//! against executable top-of-book quotes, with completed bars as a conservative fallback trigger.
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
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
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
    quote::{AdjustType, Candlestick, Period, QuoteContext, TradeSession, TradeSessions},
};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::{actor::DataActor, enums::Environment, live::get_runtime};
use nautilus_core::{
    UUID4, UnixNanos,
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
        parse::{parse_bar_with_price_precision, parse_instrument},
        rate_limit::{MAX_QUOTE_SUBSCRIPTION_SYMBOLS, quote_api_call_with_retry},
    },
};
use nautilus_model::{
    data::{Bar, BarType, Data, QuoteTick},
    enums::{
        AccountType, AggregationSource, BarAggregation, BookType, ContingencyType, OmsType,
        OrderSide, OrderType, PriceType, TimeInForce, TriggerType,
    },
    events::{
        OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired, OrderFilled, OrderRejected,
        PositionClosed,
    },
    identifiers::{AccountId, ClientOrderId, InstrumentId, OrderListId, StrategyId, TraderId},
    instruments::{Instrument, InstrumentAny},
    orders::{LimitOrder, Order, OrderAny, StopMarketOrder},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use ustr::Ustr;

const TRADER_ID: &str = "SLC-TRADER-001";
const ACCOUNT_ID: &str = "LONGBRIDGE-001";
const NODE_NAME: &str = "LONGBRIDGE-SLC-001";
const STRATEGY_ID: &str = "SLC-001";
const US_TIMEZONE: &str = "America/New_York";
const RTH_OPEN_MINUTE: u16 = 9 * 60 + 30;
const RTH_CLOSE_MINUTE: u16 = 16 * 60;
const FIVE_MINUTES: u16 = 5;
const FIVE_MINUTE_NANOS: u64 = 5 * 60 * 1_000_000_000;
const FOUR_HOUR_NANOS: u64 = 4 * 60 * 60 * 1_000_000_000;
const HISTORY_CHUNK_DAYS: i64 = 7;
const TRADING_DAYS_CHUNK_DAYS: i64 = 28;
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
    displacement_max_bars: usize,
    pivot_span: usize,
    zone_ttl_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    confirmation_max_distance_atr: f64,
    stochastic_k_period: usize,
    stochastic_k_smoothing: usize,
    stochastic_d_period: usize,
    oversold: f64,
    overbought: f64,
    five_minute_warmup: usize,
    four_hour_warmup: usize,
    minimum_target_time_minutes: u16,
    time_stop_bars: u64,
    time_stop_minimum_mfe_r: Decimal,
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
    fn from_env(live: bool) -> anyhow::Result<Self> {
        let papertrading = env_parse("LONGBRIDGE_SLC_PAPERTRADING", "true")?;
        if live {
            validate_live_guard(
                papertrading,
                env::var("LONGBRIDGE_SLC_LIVE_ACK").ok().as_deref(),
            )?;
            validate_realtime_candlesticks()?;
        }

        let symbol_config_path =
            env_string("LONGBRIDGE_SLC_CONFIG_PATH", DEFAULT_SYMBOL_CONFIG_PATH);
        let instruments = load_symbol_config(Path::new(&symbol_config_path))?;
        let risk_amount = env_parse("LONGBRIDGE_SLC_RISK_AMOUNT", "25")?;
        let daily_loss_limit = env_parse("LONGBRIDGE_SLC_DAILY_LOSS_LIMIT", "50")?;
        let max_open_risk = env_parse("LONGBRIDGE_SLC_MAX_OPEN_RISK", "50")?;
        let max_account_notional = env_parse("LONGBRIDGE_SLC_MAX_ACCOUNT_NOTIONAL", "5000")?;
        let max_open_positions = env_parse("LONGBRIDGE_SLC_MAX_OPEN_POSITIONS", "2")?;
        let max_order_quantity = env_parse("LONGBRIDGE_SLC_MAX_ORDER_QUANTITY", "500")?;
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

        let atr_period: usize = env_parse("LONGBRIDGE_SLC_ATR_PERIOD", "14")?;
        let displacement_atr_multiple = env_parse("LONGBRIDGE_SLC_DISPLACEMENT_ATR", "1.0")?;
        let displacement_close_fraction =
            env_parse("LONGBRIDGE_SLC_DISPLACEMENT_CLOSE_FRACTION", "0.35")?;
        let displacement_max_bars = env_parse("LONGBRIDGE_SLC_DISPLACEMENT_MAX_BARS", "3")?;
        let pivot_span = env_parse("LONGBRIDGE_SLC_PIVOT_SPAN", "2")?;
        let zone_ttl_bars = env_parse("LONGBRIDGE_SLC_ZONE_TTL_BARS", "234")?;
        let max_zones_per_side = env_parse("LONGBRIDGE_SLC_MAX_ZONES_PER_SIDE", "8")?;
        let confirmation_window_bars = env_parse("LONGBRIDGE_SLC_CONFIRMATION_WINDOW_BARS", "3")?;
        let confirmation_max_distance_atr =
            env_parse("LONGBRIDGE_SLC_CONFIRMATION_MAX_DISTANCE_ATR", "0.35")?;
        let stochastic_k_period: usize = env_parse("LONGBRIDGE_SLC_STOCHASTIC_K_PERIOD", "5")?;
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
            (1..=12).contains(&displacement_max_bars),
            "displacement maximum bars must be between 1 and 12",
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
            (0.0..=2.0).contains(&confirmation_max_distance_atr),
            "confirmation maximum distance ATR must be between 0 and 2",
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
        let four_hour_warmup = env_parse("LONGBRIDGE_SLC_4H_WARMUP", "60")?;
        let minimum_five_minute_warmup = atr_period.max(
            stochastic_k_period
                .saturating_add(stochastic_k_smoothing)
                .saturating_add(stochastic_d_period),
        );
        anyhow::ensure!(
            five_minute_warmup > minimum_five_minute_warmup && five_minute_warmup < MAX_WARMUP_BARS,
            "5-minute warmup must initialize ATR and stochastic periods and be less than {MAX_WARMUP_BARS}",
        );
        anyhow::ensure!(
            four_hour_warmup > pivot_span * 2 + 1 && four_hour_warmup < MAX_WARMUP_BARS,
            "4-hour warmup must exceed the pivot window and be less than {MAX_WARMUP_BARS}",
        );

        let session = SessionRules {
            entry_start_minute: env_time("LONGBRIDGE_SLC_ENTRY_START", "09:35")?,
            entry_end_minute: env_time("LONGBRIDGE_SLC_ENTRY_END", "15:30")?,
            flatten_before_close_minutes: env_parse(
                "LONGBRIDGE_SLC_FLATTEN_BEFORE_CLOSE_MINUTES",
                "10",
            )?,
            max_trades_per_day: env_parse("LONGBRIDGE_SLC_MAX_TRADES_PER_DAY", "20")?,
        };
        session.validate()?;
        let minimum_target_time_minutes =
            env_parse("LONGBRIDGE_SLC_MINIMUM_TARGET_TIME_MINUTES", "60")?;
        let time_stop_bars = env_parse("LONGBRIDGE_SLC_TIME_STOP_BARS", "9")?;
        let time_stop_minimum_mfe_r = env_parse("LONGBRIDGE_SLC_TIME_STOP_MINIMUM_MFE_R", "0.5")?;
        anyhow::ensure!(
            minimum_target_time_minutes > 0,
            "minimum target time minutes must be positive",
        );
        anyhow::ensure!(time_stop_bars > 0, "time stop bars must be positive");
        anyhow::ensure!(
            time_stop_minimum_mfe_r >= Decimal::ZERO && time_stop_minimum_mfe_r < risk_reward,
            "time stop minimum MFE R must be non-negative and below the profit target",
        );
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
            displacement_max_bars,
            pivot_span,
            zone_ttl_bars,
            max_zones_per_side,
            confirmation_window_bars,
            confirmation_max_distance_atr,
            stochastic_k_period,
            stochastic_k_smoothing,
            stochastic_d_period,
            oversold,
            overbought,
            five_minute_warmup,
            four_hour_warmup,
            minimum_target_time_minutes,
            time_stop_bars,
            time_stop_minimum_mfe_r,
            risk_state_path,
            timezone: get_timezone(US_TIMEZONE)?,
            session,
        })
    }

    /// Returns the external five-minute bar type used for signals and fallback target checks.
    fn five_minute_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{instrument_id}-5-MINUTE-LAST-EXTERNAL").as_str())
    }

    /// Returns the external four-hour bar type used for higher-timeframe structure.
    fn four_hour_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{instrument_id}-4-HOUR-LAST-EXTERNAL").as_str())
    }

    /// Returns the per-entry notional cap after reserving equal capacity for each usable slot.
    fn per_position_notional_limit(&self) -> Decimal {
        per_position_notional_limit(
            self.max_account_notional,
            self.max_open_positions.min(self.instruments.len()),
            self.max_order_notional,
        )
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

#[derive(Clone, Debug)]
struct SlcBacktestConfig {
    strategy: AppConfig,
    start: Timestamp,
    end: Timestamp,
    walk_forward_split: Option<Timestamp>,
    risk_rewards: Vec<Decimal>,
    starting_balance: Money,
    timeout_secs: u64,
    log_bars: bool,
    round_trip_cost_per_share: Decimal,
}

impl SlcBacktestConfig {
    /// Loads the historical window and simulation-only controls around the shared SLC settings.
    fn from_env() -> anyhow::Result<Self> {
        let mut strategy = AppConfig::from_env(false)?;
        let start = env_parse("LONGBRIDGE_SLC_BACKTEST_START", "2026-08-03T00:00:00Z")?;
        let end = env_parse("LONGBRIDGE_SLC_BACKTEST_END", "2026-08-31T23:59:59Z")?;
        anyhow::ensure!(
            start < end,
            "LONGBRIDGE_SLC_BACKTEST_START must be before LONGBRIDGE_SLC_BACKTEST_END",
        );
        let walk_forward_split = env::var("LONGBRIDGE_SLC_BACKTEST_WALK_FORWARD_SPLIT")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| {
                value.parse::<Timestamp>().map_err(|error| {
                    anyhow::anyhow!(
                        "invalid LONGBRIDGE_SLC_BACKTEST_WALK_FORWARD_SPLIT={value:?}: {error}",
                    )
                })
            })
            .transpose()?;
        if let Some(split) = walk_forward_split {
            anyhow::ensure!(
                start < split && split < end,
                "walk-forward split must be strictly inside the backtest interval",
            );
        }
        let risk_rewards = env::var("LONGBRIDGE_SLC_BACKTEST_RISK_REWARDS").map_or_else(
            |_| Ok(vec![strategy.risk_reward]),
            |value| parse_decimal_grid(&value),
        )?;
        anyhow::ensure!(
            risk_rewards
                .iter()
                .all(|risk_reward| *risk_reward > strategy.time_stop_minimum_mfe_r),
            "every backtest risk reward must exceed the time-stop minimum MFE R",
        );
        let starting_balance =
            env_parse("LONGBRIDGE_SLC_BACKTEST_STARTING_BALANCE", "100_000 USD")?;
        anyhow::ensure!(
            Money::is_positive(&starting_balance),
            "LONGBRIDGE_SLC_BACKTEST_STARTING_BALANCE must be positive",
        );
        let timeout_secs = env_parse("LONGBRIDGE_SLC_BACKTEST_TIMEOUT_SECS", "300")?;
        anyhow::ensure!(
            timeout_secs > 0,
            "LONGBRIDGE_SLC_BACKTEST_TIMEOUT_SECS must be positive",
        );
        let round_trip_cost_per_share =
            env_parse("LONGBRIDGE_SLC_BACKTEST_ROUND_TRIP_COST_PER_SHARE", "0.01")?;
        anyhow::ensure!(
            round_trip_cost_per_share >= Decimal::ZERO,
            "LONGBRIDGE_SLC_BACKTEST_ROUND_TRIP_COST_PER_SHARE must be non-negative",
        );
        strategy.risk_state_path = env::temp_dir().join(format!(
            "nautilus-slc-backtest-risk-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()),
        ));
        Ok(Self {
            strategy,
            start,
            end,
            walk_forward_split,
            risk_rewards,
            starting_balance,
            timeout_secs,
            log_bars: env_parse("LONGBRIDGE_SLC_BACKTEST_LOG_BARS", "false")?,
            round_trip_cost_per_share,
        })
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

/// Parses a positive, deterministic Decimal grid for exit-parameter comparisons.
fn parse_decimal_grid(value: &str) -> anyhow::Result<Vec<Decimal>> {
    let mut values = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<Decimal>()
                .map_err(|error| anyhow::anyhow!("invalid risk reward {value:?}: {error}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(!values.is_empty(), "risk reward grid must not be empty");
    anyhow::ensure!(
        values.iter().all(|value| *value > Decimal::ZERO),
        "risk reward grid values must be positive",
    );
    values.sort_unstable();
    values.dedup();
    Ok(values)
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct Zone {
    kind: ZoneKind,
    low: Price,
    high: Price,
    age: usize,
    state: ZoneState,
    break_count: u8,
    confirmation_armed: bool,
    confirmation_bars_left: usize,
    atr_at_creation: f64,
    displacement_strength_atr: f64,
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
            atr_at_creation: (bar.high.as_f64() - bar.low.as_f64()).max(f64::EPSILON),
            displacement_strength_atr: 0.0,
        }
    }

    /// Creates a level carrying the normalized quality of its displacement move.
    fn from_displacement(kind: ZoneKind, bar: Bar, atr: f64, strength_atr: f64) -> Self {
        Self {
            atr_at_creation: atr,
            displacement_strength_atr: strength_atr,
            ..Self::from_bar(kind, bar)
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
        // A re-entry cross proves the immediately preceding %K value was already beyond the band.
        self.confirmation_armed = confirmation.extreme || confirmation.reentry;
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
        let distance_atr = zone_distance_atr(*self, bar.close, self.atr_at_creation);
        if allow_entry
            && self.confirmation_armed
            && confirmation.reentry
            && distance_atr <= rules.confirmation_max_distance_atr
        {
            return ZoneObservation::Signal(Signal {
                side,
                level: if self.break_count == 0 {
                    SignalLevel::Fresh
                } else {
                    SignalLevel::Reclaimed
                },
                entry: bar.close,
                zone_low: self.low,
                zone_high: self.high,
                level_age_bars: u64::try_from(self.age).unwrap_or(u64::MAX),
                confirmation_bars: u64::try_from(
                    rules.confirmation_window_bars + 1 - self.confirmation_bars_left,
                )
                .unwrap_or(u64::MAX),
                distance_atr,
                zone_width_atr: (self.high.as_f64() - self.low.as_f64()) / self.atr_at_creation,
                displacement_strength_atr: self.displacement_strength_atr,
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct Signal {
    side: OrderSide,
    level: SignalLevel,
    entry: Price,
    zone_low: Price,
    zone_high: Price,
    level_age_bars: u64,
    confirmation_bars: u64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    ts_event: UnixNanos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SignalLevel {
    Fresh,
    Reclaimed,
}

impl Display for SignalLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh => write!(f, "fresh"),
            Self::Reclaimed => write!(f, "reclaimed"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Confirmation {
    extreme: bool,
    reentry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
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

    /// Takes the last already-confirmed historical bar after a finite warmup replay.
    fn take(&mut self) -> Option<Bar> {
        self.pending.take()
    }
}

#[derive(Clone, Copy, Debug)]
struct SignalRules {
    zone_ttl_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    confirmation_max_distance_atr: f64,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    displacement_max_bars: usize,
    oversold: f64,
    overbought: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SignalFunnel {
    five_minute_bars: u64,
    directional_bars: u64,
    zones_created: u64,
    level_touches: u64,
    stochastic_extremes: u64,
    stochastic_reentries: u64,
    signals: u64,
}

impl SignalFunnel {
    /// Records confirmation events once per side and bar, rather than once per overlapping level.
    fn record_confirmation(
        &mut self,
        zones: &VecDeque<Zone>,
        bar: Bar,
        confirmation: Confirmation,
    ) {
        let touched = zones.iter().any(|zone| {
            matches!(zone.state, ZoneState::Fresh | ZoneState::Reclaimed)
                && !zone.broken(bar)
                && zone.intersects(bar)
        });
        let active = touched
            || zones
                .iter()
                .any(|zone| zone.state == ZoneState::AwaitingConfirmation && !zone.broken(bar));
        self.level_touches += u64::from(touched);
        self.stochastic_extremes += u64::from(active && confirmation.extreme);
        self.stochastic_reentries += u64::from(active && confirmation.reentry);
    }
}

struct SlcSignalState {
    five_minute_bars: FinalBarBuffer,
    four_hour_bars: FinalBarBuffer,
    structure: PivotStructure,
    atr: AverageTrueRange,
    stochastics: Stochastics,
    recent_five_minute_bars: VecDeque<Bar>,
    last_demand_source: Option<UnixNanos>,
    last_supply_source: Option<UnixNanos>,
    previous_k: Option<f64>,
    demand: VecDeque<Zone>,
    supply: VecDeque<Zone>,
    funnel: SignalFunnel,
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
            recent_five_minute_bars: VecDeque::with_capacity(config.displacement_max_bars + 1),
            last_demand_source: None,
            last_supply_source: None,
            previous_k: None,
            demand: VecDeque::with_capacity(config.max_zones_per_side),
            supply: VecDeque::with_capacity(config.max_zones_per_side),
            funnel: SignalFunnel::default(),
            rules: SignalRules {
                zone_ttl_bars: config.zone_ttl_bars,
                max_zones_per_side: config.max_zones_per_side,
                confirmation_window_bars: config.confirmation_window_bars,
                confirmation_max_distance_atr: config.confirmation_max_distance_atr,
                displacement_atr_multiple: config.displacement_atr_multiple,
                displacement_close_fraction: config.displacement_close_fraction,
                displacement_max_bars: config.displacement_max_bars,
                oversold: config.oversold,
                overbought: config.overbought,
            },
        }
    }

    /// Replays completed historical bars while suppressing historical entry signals.
    fn warm_up(
        &mut self,
        five_minute_bars: Vec<Bar>,
        four_hour_bars: Vec<Bar>,
        finalize_last: bool,
    ) {
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
        if finalize_last {
            if let Some(finalized) = self.four_hour_bars.take() {
                self.structure.update(finalized);
            }
            if let Some(finalized) = self.five_minute_bars.take() {
                let _ = self.process_five_minute(finalized, false);
            }
        }
    }

    /// Returns whether the five-minute indicators are initialized for signal evaluation.
    fn indicators_initialized(&self) -> bool {
        self.atr.initialized() && self.stochastics.initialized()
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
        self.funnel.five_minute_bars += 1;
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
        self.funnel.directional_bars += u64::from(trend != Trend::Neutral);
        let long_confirmation = Confirmation {
            extreme: stochastic_initialized && current_k <= self.rules.oversold,
            reentry: long_cross,
        };
        let short_confirmation = Confirmation {
            extreme: stochastic_initialized && current_k >= self.rules.overbought,
            reentry: short_cross,
        };
        self.funnel
            .record_confirmation(&self.demand, bar, long_confirmation);
        self.funnel
            .record_confirmation(&self.supply, bar, short_confirmation);
        let long_signal = observe_zones(
            &mut self.demand,
            bar,
            long_confirmation,
            trend == Trend::Up && allow_signal,
            OrderSide::Buy,
            self.rules,
        );
        let short_signal = observe_zones(
            &mut self.supply,
            bar,
            short_confirmation,
            trend == Trend::Down && allow_signal,
            OrderSide::Sell,
            self.rules,
        );

        if self
            .recent_five_minute_bars
            .back()
            .is_some_and(|previous| has_five_minute_gap(previous.ts_event, bar.ts_event))
        {
            self.recent_five_minute_bars.clear();
        }
        self.recent_five_minute_bars.push_back(bar);
        while self.recent_five_minute_bars.len() > self.rules.displacement_max_bars + 1 {
            self.recent_five_minute_bars.pop_front();
        }
        if atr_initialized
            && let Some((kind, source, displacement_strength_atr)) =
                displacement_zone(&self.recent_five_minute_bars, atr_before, self.rules)
        {
            let last_source = match kind {
                ZoneKind::Demand => &mut self.last_demand_source,
                ZoneKind::Supply => &mut self.last_supply_source,
            };
            if *last_source != Some(source.ts_event) {
                let zones = match kind {
                    ZoneKind::Demand => &mut self.demand,
                    ZoneKind::Supply => &mut self.supply,
                };
                push_zone(
                    zones,
                    Zone::from_displacement(kind, source, atr_before, displacement_strength_atr),
                    self.rules.max_zones_per_side,
                );
                *last_source = Some(source.ts_event);
                self.funnel.zones_created += 1;
            }
        }

        self.atr.handle_bar(&bar);
        self.previous_k = Some(current_k);
        let signal = long_signal.or(short_signal);
        self.funnel.signals += u64::from(signal.is_some());
        signal
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

/// Finds the most recent opposing candle before an ATR-sized one-to-three-bar price expansion.
fn displacement_zone(
    bars: &VecDeque<Bar>,
    atr: f64,
    rules: SignalRules,
) -> Option<(ZoneKind, Bar, f64)> {
    if atr <= 0.0 {
        return None;
    }
    let current = *bars.back()?;
    let first_source = bars.len().saturating_sub(rules.displacement_max_bars + 1);
    for source_index in (first_source..bars.len().saturating_sub(1)).rev() {
        let source = bars[source_index];
        let mut impulse_high = f64::NEG_INFINITY;
        let mut impulse_low = f64::INFINITY;
        for impulse in bars.iter().skip(source_index + 1) {
            impulse_high = impulse_high.max(impulse.high.as_f64());
            impulse_low = impulse_low.min(impulse.low.as_f64());
        }
        let impulse_range = impulse_high - impulse_low;
        if impulse_range <= 0.0 {
            continue;
        }
        let required_move = atr * rules.displacement_atr_multiple;
        if source.close < source.open
            && current.close > source.high
            && current.close.as_f64() - source.close.as_f64() >= required_move
            && (impulse_high - current.close.as_f64()) / impulse_range
                <= rules.displacement_close_fraction
        {
            return Some((
                ZoneKind::Demand,
                source,
                (current.close.as_f64() - source.close.as_f64()) / atr,
            ));
        }
        if source.close > source.open
            && current.close < source.low
            && source.close.as_f64() - current.close.as_f64() >= required_move
            && (current.close.as_f64() - impulse_low) / impulse_range
                <= rules.displacement_close_fraction
        {
            return Some((
                ZoneKind::Supply,
                source,
                (source.close.as_f64() - current.close.as_f64()) / atr,
            ));
        }
    }
    None
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum RiskRejectionReason {
    ZeroQuantity,
    SymbolTradeLimit,
    AccountHalted,
    DailyLoss,
    OpenPositions,
    OpenRisk,
    AccountNotional,
    ExistingSymbolReservation,
}

impl Display for RiskRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQuantity => write!(f, "zero_quantity"),
            Self::SymbolTradeLimit => write!(f, "symbol_trade_limit"),
            Self::AccountHalted => write!(f, "account_halted"),
            Self::DailyLoss => write!(f, "daily_loss_limit"),
            Self::OpenPositions => write!(f, "open_positions_limit"),
            Self::OpenRisk => write!(f, "open_risk_limit"),
            Self::AccountNotional => write!(f, "account_notional_limit"),
            Self::ExistingSymbolReservation => write!(f, "existing_symbol_reservation"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservationOutcome {
    Reserved,
    Rejected(RiskRejectionReason),
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
        let rejection = if snapshot.entries_for_symbol >= max_trades_per_symbol {
            Some(RiskRejectionReason::SymbolTradeLimit)
        } else if snapshot.halted {
            Some(RiskRejectionReason::AccountHalted)
        } else if snapshot.realized_pnl <= -limits.daily_loss {
            Some(RiskRejectionReason::DailyLoss)
        } else if snapshot.open_positions >= limits.open_positions {
            Some(RiskRejectionReason::OpenPositions)
        } else if snapshot.open_risk + reservation.risk > limits.open_risk {
            Some(RiskRejectionReason::OpenRisk)
        } else if snapshot.account_notional + reservation.notional > limits.account_notional {
            Some(RiskRejectionReason::AccountNotional)
        } else if state.reservations.contains_key(symbol) {
            Some(RiskRejectionReason::ExistingSymbolReservation)
        } else {
            None
        };
        if let Some(reason) = rejection {
            if *state != before {
                self.persist_or_restore(&mut state, before)?;
            }
            return Ok((ReservationOutcome::Rejected(reason), snapshot));
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

/// Requests the pre-close exit once, and only while the strategy still owns exposure.
fn should_request_preclose_exit(
    close_minute: u16,
    flatten_minute: u16,
    has_exposure: bool,
    exit_pending: bool,
) -> bool {
    close_minute >= flatten_minute && has_exposure && !exit_pending
}

/// Returns the close-to-zone distance normalized by the current ATR.
fn zone_distance_atr(zone: Zone, close: Price, atr: f64) -> f64 {
    if atr <= 0.0 {
        return f64::INFINITY;
    }
    let distance = if close < zone.low {
        zone.low.as_f64() - close.as_f64()
    } else if close > zone.high {
        close.as_f64() - zone.high.as_f64()
    } else {
        0.0
    };
    distance / atr
}

/// Returns whether stale exposure lacks the configured favorable excursion.
fn should_request_time_stop(
    bars_held: u64,
    mfe_r: Decimal,
    time_stop_bars: u64,
    minimum_mfe_r: Decimal,
    has_exposure: bool,
    exit_pending: bool,
) -> bool {
    has_exposure && !exit_pending && bars_held >= time_stop_bars && mfe_r < minimum_mfe_r
}

/// Caps one entry to an equal share of account notional so one order cannot consume every slot.
fn per_position_notional_limit(
    account_notional: Decimal,
    open_positions: usize,
    order_notional: Decimal,
) -> Decimal {
    let open_positions = u64::try_from(open_positions).expect("validated position limit fits u64");
    order_notional.min(account_notional / Decimal::from(open_positions))
}

/// Preserves stop-loss semantics across Nautilus simulation and Longbridge conditional orders.
fn protective_stop_order_type(is_backtest: bool) -> OrderType {
    if is_backtest {
        OrderType::StopMarket
    } else {
        OrderType::MarketIfTouched
    }
}

/// Builds an exit-only OUO pair so either backtest fill resizes or cancels its sibling.
fn backtest_protective_orders(
    event: &OrderFilled,
    exit_side: OrderSide,
    stop: Price,
    target: Price,
    order_list_id: OrderListId,
    stop_order_id: ClientOrderId,
    target_order_id: ClientOrderId,
) -> anyhow::Result<Vec<OrderAny>> {
    let stop_order = StopMarketOrder::new_checked(
        event.trader_id,
        event.strategy_id,
        event.instrument_id,
        stop_order_id,
        exit_side,
        event.last_qty,
        stop,
        TriggerType::Default,
        TimeInForce::Day,
        None,
        true,
        false,
        None,
        None,
        None,
        Some(ContingencyType::Ouo),
        Some(order_list_id),
        Some(vec![target_order_id]),
        None,
        None,
        None,
        None,
        Some(vec![Ustr::from("SLC_STOP")]),
        UUID4::new(),
        event.ts_init,
    )?;
    let target_order = LimitOrder::new_checked(
        event.trader_id,
        event.strategy_id,
        event.instrument_id,
        target_order_id,
        exit_side,
        event.last_qty,
        target,
        TimeInForce::Day,
        None,
        false,
        true,
        false,
        None,
        None,
        None,
        Some(ContingencyType::Ouo),
        Some(order_list_id),
        Some(vec![stop_order_id]),
        None,
        None,
        None,
        None,
        Some(vec![Ustr::from("SLC_TARGET")]),
        UUID4::new(),
        event.ts_init,
    )?;
    Ok(vec![
        OrderAny::StopMarket(stop_order),
        OrderAny::Limit(target_order),
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TradeExitReason {
    Target,
    Stop,
    TimeStop,
    PreClose,
    RiskExit,
    Mixed,
    Unknown,
}

impl TradeExitReason {
    /// Combines partial exits without hiding trades filled by more than one exit path.
    fn combine(current: Option<Self>, next: Self) -> Self {
        match current {
            None => next,
            Some(current) if current == next => current,
            Some(_) => Self::Mixed,
        }
    }
}

impl Display for TradeExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Target => write!(f, "target"),
            Self::Stop => write!(f, "stop"),
            Self::TimeStop => write!(f, "time_stop"),
            Self::PreClose => write!(f, "pre_close"),
            Self::RiskExit => write!(f, "risk_exit"),
            Self::Mixed => write!(f, "mixed"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ClosedTradeStatistics {
    side: OrderSide,
    level: SignalLevel,
    level_age_bars: u64,
    confirmation_bars: u64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    entry_minute: u16,
    holding_bars: u64,
    mfe_r: Decimal,
    mae_r: Decimal,
    exit_reason: TradeExitReason,
    realized_pnl: Option<Decimal>,
    estimated_cost: Decimal,
    initial_risk: Decimal,
    risk_utilization: Decimal,
    r_multiple: Option<Decimal>,
    close_ts: UnixNanos,
    ambiguous_exit_bar: bool,
}

impl ClosedTradeStatistics {
    /// Returns realized PnL after the configured round-trip per-share cost estimate.
    fn cost_adjusted_pnl(self) -> Option<Decimal> {
        self.realized_pnl.map(|pnl| pnl - self.estimated_cost)
    }

    /// Reprices target wins on path-ambiguous bars as full-risk losses for robust selection.
    fn conservative_pnl(self) -> Option<Decimal> {
        self.cost_adjusted_pnl().map(|pnl| {
            if self.ambiguous_exit_bar && self.exit_reason == TradeExitReason::Target {
                -self.initial_risk - self.estimated_cost
            } else {
                pnl
            }
        })
    }
}

#[derive(Debug, Default)]
struct SymbolRunStatistics {
    funnel: SignalFunnel,
    entries_submitted: u64,
    risk_rejections: BTreeMap<RiskRejectionReason, u64>,
    trades: Vec<ClosedTradeStatistics>,
}

#[derive(Debug, Default)]
struct RunStatistics {
    symbols: HashMap<InstrumentId, SymbolRunStatistics>,
}

impl RunStatistics {
    /// Returns deterministic diagnostic lines for console output after a run.
    fn lines(&self) -> Vec<String> {
        let all_trades = self
            .symbols
            .values()
            .flat_map(|symbol| symbol.trades.iter());
        let total = TradeAggregate::from_trades(all_trades.clone());
        let entries_submitted = self
            .symbols
            .values()
            .map(|symbol| symbol.entries_submitted)
            .sum::<u64>();
        let risk_rejections = self
            .symbols
            .values()
            .flat_map(|symbol| symbol.risk_rejections.values())
            .sum::<u64>();
        let mut lines = vec![format!(
            "SLC diagnostics total: entries_submitted={entries_submitted}, risk_rejections={risk_rejections}, {}",
            total.summary(),
        )];

        let mut rejections: BTreeMap<RiskRejectionReason, u64> = BTreeMap::new();
        for symbol in self.symbols.values() {
            for (reason, count) in &symbol.risk_rejections {
                *rejections.entry(*reason).or_default() += count;
            }
        }
        for (reason, count) in rejections {
            lines.push(format!(
                "SLC risk rejection: reason={reason}, count={count}",
            ));
        }

        let mut exits: BTreeMap<TradeExitReason, Vec<&ClosedTradeStatistics>> = BTreeMap::new();
        for trade in all_trades {
            exits.entry(trade.exit_reason).or_default().push(trade);
        }
        for (reason, trades) in exits {
            lines.push(format!(
                "SLC exit cohort: exit_reason={reason}, {}",
                TradeAggregate::from_trades(trades.into_iter()).summary(),
            ));
        }

        let mut time_cohorts: BTreeMap<u16, Vec<&ClosedTradeStatistics>> = BTreeMap::new();
        for trade in self
            .symbols
            .values()
            .flat_map(|symbol| symbol.trades.iter())
        {
            time_cohorts
                .entry((trade.entry_minute / 30) * 30)
                .or_default()
                .push(trade);
        }
        for (minute, trades) in time_cohorts {
            lines.push(format!(
                "SLC entry-time cohort: bucket={:02}:{:02}, {}",
                minute / 60,
                minute % 60,
                TradeAggregate::from_trades(trades.into_iter()).summary(),
            ));
        }

        let mut symbols = self
            .symbols
            .iter()
            .map(|(instrument_id, statistics)| (instrument_id.to_string(), statistics))
            .collect::<Vec<_>>();
        symbols.sort_by(|left, right| left.0.cmp(&right.0));
        for (instrument_id, statistics) in symbols {
            let rejected = statistics.risk_rejections.values().sum::<u64>();
            lines.push(format!(
                "[{instrument_id}] SLC diagnostics: 5m_bars={}, directional_4h_bars={}, zones_created={}, level_touches={}, stochastic_extremes={}, stochastic_reentries={}, signals={}, entries_submitted={}, risk_rejections={}, {}",
                statistics.funnel.five_minute_bars,
                statistics.funnel.directional_bars,
                statistics.funnel.zones_created,
                statistics.funnel.level_touches,
                statistics.funnel.stochastic_extremes,
                statistics.funnel.stochastic_reentries,
                statistics.funnel.signals,
                statistics.entries_submitted,
                rejected,
                TradeAggregate::from_trades(statistics.trades.iter()).summary(),
            ));
            let mut cohorts: BTreeMap<(String, SignalLevel), Vec<&ClosedTradeStatistics>> =
                BTreeMap::new();
            for trade in &statistics.trades {
                cohorts
                    .entry((trade.side.to_string(), trade.level))
                    .or_default()
                    .push(trade);
            }
            for ((side, level), trades) in cohorts {
                lines.push(format!(
                    "[{instrument_id}] SLC trade cohort: side={side}, level={level}, confirmation=stochastic_reentry, {}",
                    TradeAggregate::from_trades(trades.into_iter()).summary(),
                ));
            }
        }
        lines
    }
}

/// Computes daily cost-adjusted Sharpe, penalizing unknown OHLC paths before parameter selection.
fn conservative_cost_adjusted_sharpe(
    statistics: &RunStatistics,
    trading_days: &BTreeSet<String>,
    starting_balance: Money,
    timezone: &TimeZone,
) -> Option<f64> {
    let mut pnl_by_day: BTreeMap<String, Decimal> = BTreeMap::new();
    for trade in statistics
        .symbols
        .values()
        .flat_map(|symbol| symbol.trades.iter())
    {
        let pnl = trade.conservative_pnl()?;
        let date = trade
            .close_ts
            .to_datetime_utc()
            .to_zoned(timezone.clone())
            .date()
            .to_string();
        *pnl_by_day.entry(date).or_default() += pnl;
    }

    let mut equity = starting_balance.as_decimal();
    let mut returns = Vec::with_capacity(trading_days.len());
    for date in trading_days {
        if equity <= Decimal::ZERO {
            return None;
        }
        let pnl = pnl_by_day.get(date).copied().unwrap_or_default();
        returns.push((pnl / equity).to_f64()?);
        equity += pnl;
    }
    annualized_sharpe(&returns)
}

#[derive(Debug, Default)]
struct TradeAggregate {
    trades: u64,
    wins: u64,
    cost_adjusted_wins: u64,
    realized_pnl: Decimal,
    estimated_cost: Decimal,
    cost_adjusted_pnl: Decimal,
    conservative_pnl: Decimal,
    r_sum: Decimal,
    cost_adjusted_r_sum: Decimal,
    r_count: u64,
    initial_risk_sum: Decimal,
    risk_utilization_sum: Decimal,
    mfe_r_sum: Decimal,
    mae_r_sum: Decimal,
    holding_bars: u64,
    level_age_bars: u64,
    confirmation_bars: u64,
    distance_atr_sum: f64,
    zone_width_atr_sum: f64,
    displacement_strength_atr_sum: f64,
    ambiguous_exit_bars: u64,
}

impl TradeAggregate {
    /// Aggregates exact trade values while retaining missing-PnL observations in the trade count.
    fn from_trades<'a>(trades: impl Iterator<Item = &'a ClosedTradeStatistics>) -> Self {
        let mut aggregate = Self::default();
        for trade in trades {
            aggregate.trades += 1;
            aggregate.initial_risk_sum += trade.initial_risk;
            aggregate.risk_utilization_sum += trade.risk_utilization;
            aggregate.mfe_r_sum += trade.mfe_r;
            aggregate.mae_r_sum += trade.mae_r;
            aggregate.holding_bars += trade.holding_bars;
            aggregate.level_age_bars += trade.level_age_bars;
            aggregate.confirmation_bars += trade.confirmation_bars;
            aggregate.distance_atr_sum += trade.distance_atr;
            aggregate.zone_width_atr_sum += trade.zone_width_atr;
            aggregate.displacement_strength_atr_sum += trade.displacement_strength_atr;
            aggregate.estimated_cost += trade.estimated_cost;
            aggregate.ambiguous_exit_bars += u64::from(trade.ambiguous_exit_bar);
            if let Some(pnl) = trade.realized_pnl {
                let cost_adjusted_pnl = trade
                    .cost_adjusted_pnl()
                    .expect("realized PnL checked above");
                aggregate.realized_pnl += pnl;
                aggregate.cost_adjusted_pnl += cost_adjusted_pnl;
                aggregate.conservative_pnl += trade
                    .conservative_pnl()
                    .expect("realized PnL checked above");
                aggregate.wins += u64::from(pnl > Decimal::ZERO);
                aggregate.cost_adjusted_wins += u64::from(cost_adjusted_pnl > Decimal::ZERO);
                if trade.initial_risk > Decimal::ZERO {
                    aggregate.cost_adjusted_r_sum += cost_adjusted_pnl / trade.initial_risk;
                }
            }
            if let Some(r_multiple) = trade.r_multiple {
                aggregate.r_sum += r_multiple;
                aggregate.r_count += 1;
            }
        }
        aggregate
    }

    /// Formats the compact performance metrics shared by total and cohort reports.
    fn summary(&self) -> String {
        let win_rate = average(self.wins * 100, self.trades);
        let cost_adjusted_win_rate = average(self.cost_adjusted_wins * 100, self.trades);
        let average_r = decimal_average(self.r_sum, self.r_count);
        let average_cost_adjusted_r = decimal_average(self.cost_adjusted_r_sum, self.r_count);
        let average_initial_risk = decimal_average(self.initial_risk_sum, self.trades);
        let average_risk_utilization = decimal_average(self.risk_utilization_sum, self.trades);
        let average_mfe_r = decimal_average(self.mfe_r_sum, self.trades);
        let average_mae_r = decimal_average(self.mae_r_sum, self.trades);
        let average_holding_bars = average(self.holding_bars, self.trades);
        let average_level_age_bars = average(self.level_age_bars, self.trades);
        let average_confirmation_bars = average(self.confirmation_bars, self.trades);
        let average_distance_atr = float_average(self.distance_atr_sum, self.trades);
        let average_zone_width_atr = float_average(self.zone_width_atr_sum, self.trades);
        let average_displacement_atr =
            float_average(self.displacement_strength_atr_sum, self.trades);
        format!(
            "trades={}, wins={}, win_rate_pct={win_rate}, cost_adjusted_win_rate_pct={cost_adjusted_win_rate}, realized_pnl={}, estimated_cost={}, cost_adjusted_pnl={}, conservative_pnl={}, average_r={average_r}, average_cost_adjusted_r={average_cost_adjusted_r}, average_initial_risk={average_initial_risk}, average_risk_utilization={average_risk_utilization}, average_mfe_r={average_mfe_r}, average_mae_r={average_mae_r}, average_holding_bars={average_holding_bars}, average_level_age_bars={average_level_age_bars}, average_confirmation_bars={average_confirmation_bars}, average_distance_atr={average_distance_atr}, average_zone_width_atr={average_zone_width_atr}, average_displacement_atr={average_displacement_atr}, ambiguous_exit_bars={}",
            self.trades,
            self.wins,
            self.realized_pnl,
            self.estimated_cost,
            self.cost_adjusted_pnl,
            self.conservative_pnl,
            self.ambiguous_exit_bars,
        )
    }
}

/// Returns a stable four-decimal average or `n/a` when no observation is available.
fn decimal_average(total: Decimal, count: u64) -> String {
    if count == 0 {
        "n/a".to_string()
    } else {
        (total / Decimal::from(count)).round_dp(4).to_string()
    }
}

/// Returns an integer numerator average as a percentage-compatible decimal string.
fn average(total: u64, count: u64) -> String {
    decimal_average(Decimal::from(total), count)
}

/// Returns a stable four-decimal floating-point average for normalized setup diagnostics.
fn float_average(total: f64, count: u64) -> String {
    if count == 0 {
        "n/a".to_string()
    } else {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        format!("{:.4}", total / f64::from(count))
    }
}

/// Computes a zero-risk-rate annualized sample Sharpe from daily returns.
fn annualized_sharpe(returns: &[f64]) -> Option<f64> {
    if returns.len() < 2 {
        return None;
    }
    let count = u32::try_from(returns.len()).ok()?;
    let mean = returns.iter().sum::<f64>() / f64::from(count);
    let variance = returns
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / f64::from(count - 1);
    (variance > f64::EPSILON).then(|| mean / variance.sqrt() * 252.0_f64.sqrt())
}

/// Applies the predeclared IS/OOS degradation rule without rewarding losing samples.
fn walk_forward_verdict(in_sample_sharpe: f64, out_of_sample_sharpe: f64) -> &'static str {
    if in_sample_sharpe <= 0.0 {
        "reject_non_positive_is"
    } else if out_of_sample_sharpe <= 0.0 {
        "reject_non_positive_oos"
    } else if out_of_sample_sharpe < in_sample_sharpe * 0.5 {
        "possible_overfit"
    } else {
        "acceptable"
    }
}

/// Detects OHLC bars whose unknown intrabar path reaches both protective prices.
fn bar_reaches_stop_and_target(stop: Price, target: Price, bar: Bar) -> bool {
    let lower = stop.min(target);
    let upper = stop.max(target);
    bar.low <= lower && bar.high >= upper
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
    max_order_notional: Decimal,
    max_entry_slippage_ticks: u64,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
    time_stop_bars: u64,
    time_stop_minimum_mfe_r: Decimal,
    round_trip_cost_per_share: Decimal,
    log_bars: bool,
}

#[derive(Clone, Copy, Debug)]
struct PendingEntry {
    client_order_id: ClientOrderId,
    side: OrderSide,
    level: SignalLevel,
    level_age_bars: u64,
    confirmation_bars: u64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    entry_limit: Price,
    stop: Price,
    signal_ts: UnixNanos,
    had_fill: bool,
}

#[derive(Debug)]
struct ActiveTrade {
    side: OrderSide,
    level: SignalLevel,
    level_age_bars: u64,
    confirmation_bars: u64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    entry_minute: u16,
    stop: Price,
    target: Price,
    first_fill_ts: UnixNanos,
    filled_qty: Decimal,
    protected_qty: Decimal,
    fill_notional: Decimal,
    initial_risk: Decimal,
    maximum_favorable_excursion: Decimal,
    maximum_adverse_excursion: Decimal,
    bars_held: u64,
    exit_reason: Option<TradeExitReason>,
}

impl ActiveTrade {
    /// Returns the average entry price across all fills.
    fn average_fill(&self) -> Decimal {
        self.fill_notional / self.filled_qty
    }

    /// Records favorable and adverse movement at one executable or traded price.
    fn observe_price(&mut self, price: Price) {
        let entry = self.average_fill();
        let price = price.as_decimal();
        let (favorable, adverse) = match self.side {
            OrderSide::Buy => (price - entry, entry - price),
            OrderSide::Sell => (entry - price, price - entry),
            OrderSide::NoOrderSide => return,
        };
        self.maximum_favorable_excursion = self
            .maximum_favorable_excursion
            .max(favorable.max(Decimal::ZERO));
        self.maximum_adverse_excursion = self
            .maximum_adverse_excursion
            .max(adverse.max(Decimal::ZERO));
    }

    /// Records both extremes of one completed post-entry bar.
    fn observe_bar(&mut self, bar: Bar) {
        self.observe_price(bar.high);
        self.observe_price(bar.low);
        self.bars_held += 1;
    }

    /// Returns maximum favorable excursion normalized by actual initial risk per share.
    fn mfe_r(&self) -> Decimal {
        self.normalized_excursion(self.maximum_favorable_excursion)
    }

    /// Returns maximum adverse excursion normalized by actual initial risk per share.
    fn mae_r(&self) -> Decimal {
        self.normalized_excursion(self.maximum_adverse_excursion)
    }

    /// Normalizes a per-share excursion without dividing by zero after exceptional fills.
    fn normalized_excursion(&self, excursion: Decimal) -> Decimal {
        if self.initial_risk <= Decimal::ZERO || self.filled_qty <= Decimal::ZERO {
            Decimal::ZERO
        } else {
            excursion / (self.initial_risk / self.filled_qty)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct AmbiguityProbe {
    stop: Price,
    target: Price,
    close_ts: UnixNanos,
}

struct SlcStrategy {
    core: StrategyCore,
    config: SlcStrategyConfig,
    instrument: InstrumentAny,
    signals: SlcSignalState,
    backtest_four_hour_bars: Option<VecDeque<Bar>>,
    account_risk: Arc<AccountRisk>,
    run_statistics: Arc<Mutex<RunStatistics>>,
    pending_entry: Option<PendingEntry>,
    active_trade: Option<ActiveTrade>,
    ambiguity_probe: Option<AmbiguityProbe>,
    last_five_minute_bar: Option<Bar>,
    current_date: Option<jiff::civil::Date>,
    last_five_minute_bar_start: Option<UnixNanos>,
    suppress_warmup_boundary_signal: bool,
    session_disabled: bool,
    exit_pending: bool,
    faulted: bool,
    risk_rejections: u64,
    entries_submitted: u64,
}

struct SlcRunConfig {
    flatten_minute: u16,
    backtest_four_hour_bars: Option<Vec<Bar>>,
    round_trip_cost_per_share: Decimal,
    log_bars: bool,
    run_statistics: Arc<Mutex<RunStatistics>>,
}

/// Builds the stable per-symbol routing identity and shared strategy behavior.
fn slc_strategy_config(instrument_id: InstrumentId) -> StrategyConfig {
    StrategyConfig {
        strategy_id: Some(StrategyId::from(
            format!("{STRATEGY_ID}-{}", instrument_id.symbol).as_str(),
        )),
        external_order_claims: Some(vec![instrument_id]),
        manage_stop: true,
        market_exit_max_attempts: 300,
        market_exit_time_in_force: TimeInForce::Day,
        market_exit_reduce_only: false,
        ..Default::default()
    }
}

impl SlcStrategy {
    /// Creates one isolated per-symbol strategy sharing only the account risk ledger.
    fn new(
        app_config: &AppConfig,
        instrument_id: InstrumentId,
        instrument: InstrumentAny,
        five_minute_bars: Vec<Bar>,
        four_hour_bars: Vec<Bar>,
        account_risk: Arc<AccountRisk>,
        run_config: SlcRunConfig,
    ) -> anyhow::Result<Self> {
        let five_minute_bar_type = AppConfig::five_minute_bar_type(instrument_id);
        let four_hour_bar_type = AppConfig::four_hour_bar_type(instrument_id);
        let latest_entry_minute = run_config
            .flatten_minute
            .checked_sub(app_config.minimum_target_time_minutes)
            .context("minimum target time exceeds the available trading session")?;
        let entry_end_minute = app_config.session.entry_end_minute.min(latest_entry_minute);
        anyhow::ensure!(
            app_config.session.entry_start_minute < entry_end_minute,
            "minimum target time leaves no valid SLC entry window",
        );
        let five_minute_warmup_count = five_minute_bars.len();
        let four_hour_warmup_count = four_hour_bars.len();
        let mut signals = SlcSignalState::new(app_config);
        signals.warm_up(
            five_minute_bars,
            four_hour_bars,
            run_config.backtest_four_hour_bars.is_some(),
        );
        anyhow::ensure!(
            signals.indicators_initialized(),
            "SLC warmup did not initialize 5m indicators for {instrument_id}: bar_type={five_minute_bar_type}, received_bars={five_minute_warmup_count}, atr_initialized={}, stochastic_initialized={}",
            signals.atr.initialized(),
            signals.stochastics.initialized(),
        );
        if !signals.structure.initialized() {
            log::warn!(
                "[{instrument_id}] SLC 4h structure is not initialized: bar_type={four_hour_bar_type}, received_bars={four_hour_warmup_count}, confirmed_pivot_highs={}, confirmed_pivot_lows={}; the 4h trend remains Neutral and cannot authorize entries",
                signals.structure.highs.len(),
                signals.structure.lows.len(),
            );
        }
        log::info!(
            "[{instrument_id}] SLC warmup complete: 5m_bar_type={five_minute_bar_type}, 5m_bars={five_minute_warmup_count}, indicators_initialized={}, 4h_bar_type={four_hour_bar_type}, 4h_bars={four_hour_warmup_count}, structure_initialized={}, confirmed_pivot_highs={}, confirmed_pivot_lows={}, initial_4h_trend={:?}",
            signals.indicators_initialized(),
            signals.structure.initialized(),
            signals.structure.highs.len(),
            signals.structure.lows.len(),
            signals.structure.trend(),
        );
        signals.funnel = SignalFunnel::default();
        run_config
            .run_statistics
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC run statistics mutex was poisoned"))?
            .symbols
            .entry(instrument_id)
            .or_default();
        Ok(Self {
            core: StrategyCore::new(slc_strategy_config(instrument_id)),
            config: SlcStrategyConfig {
                instrument_id,
                five_minute_bar_type,
                four_hour_bar_type,
                timezone: app_config.timezone.clone(),
                entry_start_minute: app_config.session.entry_start_minute,
                entry_end_minute,
                flatten_minute: run_config.flatten_minute,
                max_trades_per_day: app_config.session.max_trades_per_day,
                risk_amount: app_config.risk_amount,
                account_risk_limits: AccountRiskLimits {
                    daily_loss: app_config.daily_loss_limit,
                    open_risk: app_config.max_open_risk,
                    account_notional: app_config.max_account_notional,
                    open_positions: app_config.max_open_positions,
                },
                max_order_quantity: app_config.max_order_quantity,
                max_order_notional: app_config.per_position_notional_limit(),
                max_entry_slippage_ticks: app_config.max_entry_slippage_ticks,
                risk_reward: app_config.risk_reward,
                stop_buffer_ticks: app_config.stop_buffer_ticks,
                time_stop_bars: app_config.time_stop_bars,
                time_stop_minimum_mfe_r: app_config.time_stop_minimum_mfe_r,
                round_trip_cost_per_share: run_config.round_trip_cost_per_share,
                log_bars: run_config.log_bars,
            },
            instrument,
            signals,
            backtest_four_hour_bars: run_config.backtest_four_hour_bars.map(VecDeque::from),
            account_risk,
            run_statistics: Arc::clone(&run_config.run_statistics),
            pending_entry: None,
            active_trade: None,
            ambiguity_probe: None,
            last_five_minute_bar: None,
            current_date: None,
            last_five_minute_bar_start: None,
            suppress_warmup_boundary_signal: true,
            session_disabled: false,
            exit_pending: false,
            faulted: false,
            risk_rejections: 0,
            entries_submitted: 0,
        })
    }

    /// Updates non-critical diagnostics without allowing a poisoned lock to stop trading.
    fn update_run_statistics(&self, update: impl FnOnce(&mut SymbolRunStatistics)) {
        match self.run_statistics.lock() {
            Ok(mut statistics) => update(
                statistics
                    .symbols
                    .entry(self.config.instrument_id)
                    .or_default(),
            ),
            Err(_) => log::error!(
                "[{}] Failed to update SLC run statistics because the mutex was poisoned",
                self.config.instrument_id,
            ),
        }
    }

    /// Records the first account-risk gate which rejected a valid SLC signal.
    fn record_risk_rejection(&mut self, reason: RiskRejectionReason) {
        self.risk_rejections += 1;
        self.update_run_statistics(|statistics| {
            *statistics.risk_rejections.entry(reason).or_default() += 1;
        });
    }

    /// Preserves the exit cause across partial fills and detects mixed exit paths.
    fn mark_exit_reason(&mut self, reason: TradeExitReason) {
        if let Some(active) = self.active_trade.as_mut() {
            active.exit_reason = Some(TradeExitReason::combine(active.exit_reason, reason));
        }
    }

    /// Classifies a protective or managed exit fill from the order's strategy-owned tag.
    fn record_exit_fill(&mut self, event: &OrderFilled) {
        let reason = self
            .cache()
            .order(&event.client_order_id)
            .and_then(|order| {
                let tags = order.tags()?;
                if tags.iter().any(|tag| tag.as_str() == "SLC_TARGET") {
                    Some(TradeExitReason::Target)
                } else if tags.iter().any(|tag| tag.as_str() == "SLC_STOP") {
                    Some(TradeExitReason::Stop)
                } else if tags.iter().any(|tag| tag.as_str() == "SLC_EXIT") {
                    Some(TradeExitReason::RiskExit)
                } else {
                    None
                }
            });
        if let Some(reason) = reason
            && let Some(active) = self.active_trade.as_mut()
        {
            active.observe_price(event.last_px);
            if reason != TradeExitReason::RiskExit || active.exit_reason.is_none() {
                active.exit_reason = Some(TradeExitReason::combine(active.exit_reason, reason));
            }
        }
    }

    /// Marks a backtest exit as path-ambiguous when one OHLC bar reaches both exit prices.
    fn inspect_ambiguous_exit_bar(&mut self, bar: Bar) {
        let Some(probe) = self.ambiguity_probe else {
            return;
        };
        if bar.ts_init < probe.close_ts {
            return;
        }
        self.ambiguity_probe = None;
        if bar.ts_init != probe.close_ts
            || !bar_reaches_stop_and_target(probe.stop, probe.target, bar)
        {
            return;
        }
        self.mark_ambiguous_exit_bar(probe, bar);
    }

    /// Updates the matching closed trade after an ambiguous bar is identified.
    fn mark_ambiguous_exit_bar(&self, probe: AmbiguityProbe, bar: Bar) {
        self.update_run_statistics(|statistics| {
            if let Some(trade) = statistics
                .trades
                .iter_mut()
                .rev()
                .find(|trade| trade.close_ts == probe.close_ts)
            {
                trade.ambiguous_exit_bar = true;
            }
        });
        log::warn!(
            "[{}] SLC backtest exit is intrabar ambiguous: bar_type={}, bar_close={}, stop={}, target={}, low={}, high={}",
            self.config.instrument_id,
            bar.bar_type,
            bar.ts_init,
            probe.stop,
            probe.target,
            bar.low,
            bar.high,
        );
    }

    /// Applies one confirmed four-hour bar to structure without involving order matching.
    fn process_finalized_four_hour_bar(&mut self, bar: Bar) -> anyhow::Result<()> {
        let local = bar
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
        if (RTH_OPEN_MINUTE..RTH_CLOSE_MINUTE).contains(&minute) {
            self.signals.process_four_hour(bar);
            if self.config.log_bars {
                log::info!(
                    "[{}] 4h structure updated: start={}, open={}, high={}, low={}, close={}, structure_initialized={}, confirmed_pivot_highs={}, confirmed_pivot_lows={}, trend={:?}",
                    self.config.instrument_id,
                    local,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    self.signals.structure.initialized(),
                    self.signals.structure.highs.len(),
                    self.signals.structure.lows.len(),
                    self.signals.structure.trend(),
                );
            }
        }
        Ok(())
    }

    /// Advances historical four-hour bars only when their next period has begun on the replay clock.
    fn advance_backtest_four_hour_bars(&mut self, timestamp: UnixNanos) -> anyhow::Result<()> {
        loop {
            let Some(next) = self
                .backtest_four_hour_bars
                .as_ref()
                .and_then(|bars| bars.front().copied())
                .filter(|bar| bar.ts_event <= timestamp)
            else {
                return Ok(());
            };
            self.backtest_four_hour_bars
                .as_mut()
                .expect("backtest bars checked above")
                .pop_front();
            if let Some(finalized) = self.signals.finalize_four_hour(next) {
                self.process_finalized_four_hour_bar(finalized)?;
            }
        }
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
        self.mark_exit_reason(TradeExitReason::RiskExit);
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
            self.config.max_order_notional,
            lot_size,
        )?
        else {
            self.record_risk_rejection(RiskRejectionReason::ZeroQuantity);
            log::warn!(
                "[{}] Skipping SLC signal: risk_rejection={}, level={}, entry_limit={}, stop={}, risk_budget={}, max_quantity={}, max_notional={}, lot_size={}",
                self.config.instrument_id,
                RiskRejectionReason::ZeroQuantity,
                signal.level,
                entry_limit,
                stop,
                self.config.risk_amount,
                self.config.max_order_quantity,
                self.config.max_order_notional,
                lot_size,
            );
            return Ok(());
        };
        let quantity_decimal = quantity.as_decimal();
        let reservation = RiskReservation {
            risk: (entry_limit.as_decimal() - stop.as_decimal()).abs() * quantity_decimal,
            notional: entry_limit.as_decimal() * quantity_decimal,
        };
        anyhow::ensure!(
            reservation.notional <= self.config.max_order_notional,
            "SLC quantity sizing exceeded configured order notional",
        );
        let symbol = self.config.instrument_id.symbol.to_string();
        let (outcome, snapshot) = self.account_risk.reserve_entry(
            &symbol,
            local_date,
            reservation,
            self.config.max_trades_per_day,
            self.config.account_risk_limits,
        )?;
        if let ReservationOutcome::Rejected(reason) = outcome {
            self.record_risk_rejection(reason);
            log::warn!(
                "[{}] Skipping SLC signal: risk_rejection={}, level={}, candidate_risk={}, candidate_notional={}, halted={}, daily_pnl={}, daily_loss_limit={}, open_risk={}, open_risk_limit={}, account_notional={}, account_notional_limit={}, open_positions={}, open_positions_limit={}, symbol_entries={}, symbol_trade_limit={}",
                self.config.instrument_id,
                reason,
                signal.level,
                reservation.risk,
                reservation.notional,
                snapshot.halted,
                snapshot.realized_pnl,
                self.config.account_risk_limits.daily_loss,
                snapshot.open_risk,
                self.config.account_risk_limits.open_risk,
                snapshot.account_notional,
                self.config.account_risk_limits.account_notional,
                snapshot.open_positions,
                self.config.account_risk_limits.open_positions,
                snapshot.entries_for_symbol,
                self.config.max_trades_per_day,
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
            level: signal.level,
            level_age_bars: signal.level_age_bars,
            confirmation_bars: signal.confirmation_bars,
            distance_atr: signal.distance_atr,
            zone_width_atr: signal.zone_width_atr,
            displacement_strength_atr: signal.displacement_strength_atr,
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
        self.entries_submitted += 1;
        self.update_run_statistics(|statistics| statistics.entries_submitted += 1);
        log::info!(
            "[{}] Submitted SLC {} entry: level={}, confirmation=stochastic_reentry, level_age_bars={}, confirmation_bars={}, distance_atr={:.4}, zone_width_atr={:.4}, displacement_atr={:.4}, quantity={}, signal_close={}, entry_limit={}, stop={}, reserved_risk={}, reserved_notional={}",
            self.config.instrument_id,
            signal.side,
            signal.level,
            signal.level_age_bars,
            signal.confirmation_bars,
            signal.distance_atr,
            signal.zone_width_atr,
            signal.displacement_strength_atr,
            quantity,
            signal.entry,
            entry_limit,
            stop,
            reservation.risk,
            reservation.notional,
        );
        Ok(())
    }

    /// Protects each fill immediately; backtests rest a linked per-fill 2R target at the venue.
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
        let previous_initial_risk = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.initial_risk);
        let exit_reason = self
            .active_trade
            .as_ref()
            .and_then(|active| active.exit_reason);
        let maximum_favorable_excursion = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.maximum_favorable_excursion);
        let maximum_adverse_excursion = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.maximum_adverse_excursion);
        let bars_held = self
            .active_trade
            .as_ref()
            .map_or(0, |active| active.bars_held);
        let fill_local = event
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let fill_minute =
            u16::try_from(fill_local.hour())? * 60 + u16::try_from(fill_local.minute())?;
        let entry_minute = self
            .active_trade
            .as_ref()
            .map_or(fill_minute, |active| active.entry_minute);
        let first_fill_ts = self.active_trade.as_ref().map_or(event.ts_event, |active| {
            active.first_fill_ts.min(event.ts_event)
        });
        let filled_qty = previous_qty + event.last_qty.as_decimal();
        let fill_notional =
            previous_notional + event.last_px.as_decimal() * event.last_qty.as_decimal();
        let initial_risk = previous_initial_risk
            + (event.last_px.as_decimal() - pending.stop.as_decimal()).abs()
                * event.last_qty.as_decimal();
        let average_fill = fill_notional / filled_qty;
        let target = target_price(
            pending.side,
            average_fill,
            pending.stop,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.risk_reward,
        )?;
        let is_backtest = self.backtest_four_hour_bars.is_some();
        let protective_target = if is_backtest {
            target_price(
                pending.side,
                event.last_px.as_decimal(),
                pending.stop,
                self.instrument.price_increment(),
                self.instrument.price_precision(),
                self.config.risk_reward,
            )?
        } else {
            target
        };
        match protective_stop_order_type(is_backtest) {
            OrderType::StopMarket => {
                let (order_list_id, stop_order_id, target_order_id) = {
                    let orders = self.order();
                    (
                        orders.generate_order_list_id(),
                        orders.generate_client_order_id(),
                        orders.generate_client_order_id(),
                    )
                };
                let protective_orders = backtest_protective_orders(
                    event,
                    exit_side,
                    pending.stop,
                    protective_target,
                    order_list_id,
                    stop_order_id,
                    target_order_id,
                )?;
                self.submit_order_list(protective_orders, event.position_id, None, None)?;
            }
            OrderType::MarketIfTouched => {
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
            }
            _ => unreachable!("unsupported SLC protective stop order type"),
        }
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
            level: pending.level,
            level_age_bars: pending.level_age_bars,
            confirmation_bars: pending.confirmation_bars,
            distance_atr: pending.distance_atr,
            zone_width_atr: pending.zone_width_atr,
            displacement_strength_atr: pending.displacement_strength_atr,
            entry_minute,
            stop: pending.stop,
            target,
            first_fill_ts,
            filled_qty,
            protected_qty,
            fill_notional,
            initial_risk,
            maximum_favorable_excursion,
            maximum_adverse_excursion,
            bars_held,
            exit_reason,
        });

        let entry_closed = self
            .cache()
            .order(&pending.client_order_id)
            .is_some_and(|order| order.is_closed());
        if entry_closed {
            self.pending_entry = None;
        }
        log::info!(
            "[{}] Protected SLC entry fill: last_quantity={}, total_quantity={}, average_fill={}, stop={}, target={}, target_execution={}",
            self.config.instrument_id,
            event.last_qty,
            filled_qty,
            average_fill,
            pending.stop,
            protective_target,
            if is_backtest {
                "resting OUO limit"
            } else {
                "executable quote trigger"
            },
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

    /// Returns whether a completed live bar reached the quote-trigger fallback target.
    fn target_reached(&self, bar: Bar) -> bool {
        !self.exit_pending
            && self.active_trade.as_ref().is_some_and(|active| {
                bar_reaches_target(active.side, active.target, active.first_fill_ts, bar)
            })
    }

    /// Recognizes the normal OUO sibling cancellation after its paired exit order fills.
    fn expected_protective_cancel(&self, client_order_id: ClientOrderId) -> bool {
        let Some(order) = self.cache().order(&client_order_id) else {
            return false;
        };
        order.contingency_type() == Some(ContingencyType::Ouo)
            && order.linked_order_ids().is_some_and(|linked_order_ids| {
                linked_order_ids.iter().any(|linked_order_id| {
                    self.cache()
                        .order(linked_order_id)
                        .is_some_and(|linked_order| !linked_order.filled_qty().is_zero())
                })
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

    /// Cancels all entry and protective orders before submitting a position-closing market order.
    fn request_exit(&mut self, reason: TradeExitReason, detail: &str) -> anyhow::Result<()> {
        self.mark_exit_reason(reason);
        self.exit_pending = true;
        log::info!(
            "[{}] Requesting SLC position exit: exit_reason={reason}, detail={detail}",
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
            "[{}] All SLC protective orders canceled; submitting market exit",
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
        self.record_exit_fill(event);
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
                    "a protective SLC order expired while exposure remained",
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
        } else if self.expected_protective_cancel(event.client_order_id) {
            log::info!(
                "[{}] SLC protective OUO sibling canceled after its paired exit filled: {}",
                self.config.instrument_id,
                event.client_order_id,
            );
        } else if !self.is_exiting() && self.has_open_position() {
            self.disable_after_order_failure("a protective SLC order was canceled unexpectedly");
        }
    }

    // Escalates a rejected cancel to the framework managed-exit reconciler
    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.faulted = true;
            self.exit_pending = false;
            self.mark_exit_reason(TradeExitReason::RiskExit);
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
        let active_trade = self.active_trade.take();
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
        let mut exit_reason = TradeExitReason::Unknown;
        let mut level = None;
        let mut initial_risk = None;
        let mut risk_utilization = None;
        let mut r_multiple = None;
        let mut holding_bars = None;
        let mut mfe_r = None;
        let mut mae_r = None;
        let mut estimated_cost = None;
        if let Some(active) = active_trade {
            exit_reason = active.exit_reason.unwrap_or(TradeExitReason::Unknown);
            level = Some(active.level);
            initial_risk = Some(active.initial_risk);
            let utilization = active.initial_risk / self.config.risk_amount;
            let trade_mfe_r = active.mfe_r();
            let trade_mae_r = active.mae_r();
            let trade_estimated_cost = active.filled_qty * self.config.round_trip_cost_per_share;
            risk_utilization = Some(utilization);
            holding_bars = Some(active.bars_held);
            mfe_r = Some(trade_mfe_r);
            mae_r = Some(trade_mae_r);
            estimated_cost = Some(trade_estimated_cost);
            r_multiple = if active.initial_risk > Decimal::ZERO {
                realized_pnl.map(|pnl| pnl / active.initial_risk)
            } else {
                None
            };
            let close_ts = event.ts_closed.unwrap_or(event.ts_event);
            self.update_run_statistics(|statistics| {
                statistics.trades.push(ClosedTradeStatistics {
                    side: active.side,
                    level: active.level,
                    level_age_bars: active.level_age_bars,
                    confirmation_bars: active.confirmation_bars,
                    distance_atr: active.distance_atr,
                    zone_width_atr: active.zone_width_atr,
                    displacement_strength_atr: active.displacement_strength_atr,
                    entry_minute: active.entry_minute,
                    holding_bars: active.bars_held,
                    mfe_r: trade_mfe_r,
                    mae_r: trade_mae_r,
                    exit_reason,
                    realized_pnl,
                    estimated_cost: trade_estimated_cost,
                    initial_risk: active.initial_risk,
                    risk_utilization: utilization,
                    r_multiple,
                    close_ts,
                    ambiguous_exit_bar: false,
                });
            });
            if self.backtest_four_hour_bars.is_some()
                && matches!(exit_reason, TradeExitReason::Target | TradeExitReason::Stop)
            {
                let probe = AmbiguityProbe {
                    stop: active.stop,
                    target: active.target,
                    close_ts,
                };
                if let Some(bar) = self
                    .last_five_minute_bar
                    .filter(|bar| bar.ts_init == close_ts)
                {
                    if bar_reaches_stop_and_target(probe.stop, probe.target, bar) {
                        self.mark_ambiguous_exit_bar(probe, bar);
                    }
                } else {
                    self.ambiguity_probe = Some(probe);
                }
            }
        }
        match self.account_risk.record_close(
            self.config.instrument_id.symbol.as_str(),
            local_date,
            realized_pnl,
            !entry_remainder_open,
        ) {
            Ok(snapshot) => {
                self.session_disabled |= snapshot.halted;
                log::info!(
                    "[{}] SLC position closed: exit_reason={}, level={}, realized_pnl={:?}, estimated_cost={}, initial_risk={}, risk_utilization={}, actual_r={}, holding_bars={}, mfe_r={}, mae_r={}, account_halted={}, account_daily_pnl={}, open_risk={}, account_notional={}, open_positions={}",
                    self.config.instrument_id,
                    exit_reason,
                    level.map_or_else(|| "unknown".to_string(), |level| level.to_string()),
                    realized_pnl,
                    estimated_cost.map_or_else(|| "n/a".to_string(), |cost| cost.to_string()),
                    initial_risk.map_or_else(|| "n/a".to_string(), |risk| risk.to_string()),
                    risk_utilization
                        .map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    r_multiple
                        .map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    holding_bars.map_or_else(|| "n/a".to_string(), |bars| bars.to_string()),
                    mfe_r.map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    mae_r.map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
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
    /// Validates data contracts, restores shared risk state, and starts market-data subscriptions.
    fn on_start(&mut self) -> anyhow::Result<()> {
        validate_bar_type(self.config.five_minute_bar_type, 5, BarAggregation::Minute)?;
        validate_bar_type(self.config.four_hour_bar_type, 4, BarAggregation::Hour)?;
        anyhow::ensure!(
            self.config.entry_start_minute < self.config.entry_end_minute,
            "effective entry window ends before it starts on this trading day",
        );
        self.cache().try_instrument(&self.config.instrument_id)?;
        self.subscribe_bars(self.config.five_minute_bar_type, None, None);
        if self.backtest_four_hour_bars.is_some() {
            log::info!(
                "[{}] SLC backtest active: 5m={}, historical_4h_bars={}, entry_window={:02}:{:02}-{:02}:{:02}, flatten={:02}:{:02}, time_stop_bars={}, time_stop_minimum_mfe_r={}, estimated_round_trip_cost_per_share={}",
                self.config.instrument_id,
                self.config.five_minute_bar_type,
                self.backtest_four_hour_bars
                    .as_ref()
                    .map_or(0, VecDeque::len),
                self.config.entry_start_minute / 60,
                self.config.entry_start_minute % 60,
                self.config.entry_end_minute / 60,
                self.config.entry_end_minute % 60,
                self.config.flatten_minute / 60,
                self.config.flatten_minute % 60,
                self.config.time_stop_bars,
                self.config.time_stop_minimum_mfe_r,
                self.config.round_trip_cost_per_share,
            );
            return Ok(());
        }
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
        self.subscribe_bars(self.config.four_hour_bar_type, None, None);
        self.subscribe_quotes(self.config.instrument_id, None, None);
        log::info!(
            "[{}] SLC subscriptions active: quotes=true, 5m={}, 4h={}, entry_window={:02}:{:02}-{:02}:{:02}, flatten={:02}:{:02}, time_stop_bars={}, time_stop_minimum_mfe_r={}, account_halted={}, account_daily_pnl={}, open_risk={}, account_notional={}, open_positions={}, symbol_entries={}",
            self.config.instrument_id,
            self.config.five_minute_bar_type,
            self.config.four_hour_bar_type,
            self.config.entry_start_minute / 60,
            self.config.entry_start_minute % 60,
            self.config.entry_end_minute / 60,
            self.config.entry_end_minute % 60,
            self.config.flatten_minute / 60,
            self.config.flatten_minute % 60,
            self.config.time_stop_bars,
            self.config.time_stop_minimum_mfe_r,
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
            self.request_exit(TradeExitReason::RiskExit, "reconciled startup exposure")?;
        }
        Ok(())
    }

    /// Removes market-data subscriptions after managed stop reconciles orders and positions.
    fn on_stop(&mut self) -> anyhow::Result<()> {
        log::info!(
            "[{}] SLC signal funnel: 5m_bars={}, directional_4h_bars={}, zones_created={}, level_touches={}, stochastic_extremes={}, stochastic_reentries={}, signals={}, risk_rejections={}, entries_submitted={}",
            self.config.instrument_id,
            self.signals.funnel.five_minute_bars,
            self.signals.funnel.directional_bars,
            self.signals.funnel.zones_created,
            self.signals.funnel.level_touches,
            self.signals.funnel.stochastic_extremes,
            self.signals.funnel.stochastic_reentries,
            self.signals.funnel.signals,
            self.risk_rejections,
            self.entries_submitted,
        );
        self.unsubscribe_bars(self.config.five_minute_bar_type, None, None);
        if self.backtest_four_hour_bars.is_none() {
            self.unsubscribe_bars(self.config.four_hour_bar_type, None, None);
            self.unsubscribe_quotes(self.config.instrument_id, None, None);
        }
        Ok(())
    }

    /// Starts the existing cancel-then-close exit when the executable quote reaches 2R.
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if quote.instrument_id != self.config.instrument_id
            || self.faulted
            || self.exit_pending
            || self.is_exiting()
        {
            return Ok(());
        }
        let (side, target) = {
            let Some(active) = self.active_trade.as_mut() else {
                return Ok(());
            };
            let executable_price = match active.side {
                OrderSide::Buy => quote.bid_price,
                OrderSide::Sell => quote.ask_price,
                OrderSide::NoOrderSide => return Ok(()),
            };
            active.observe_price(executable_price);
            (active.side, active.target)
        };
        if !quote_reaches_target(side, target, quote) {
            return Ok(());
        }
        log::info!(
            "[{}] Realtime 2R target reached: side={}, target={}, bid={}, ask={}, ts_event={}",
            self.config.instrument_id,
            side,
            target,
            quote.bid_price,
            quote.ask_price,
            quote.ts_event,
        );
        self.request_exit(
            TradeExitReason::Target,
            "executable top-of-book quote reached the actual-fill-based 2R target",
        )
    }

    /// Routes completed bars through structure, data-integrity, risk, and execution gates.
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if bar.bar_type == self.config.four_hour_bar_type {
            let Some(finalized) = self.signals.finalize_four_hour(*bar) else {
                return Ok(());
            };
            self.process_finalized_four_hour_bar(finalized)?;
            return Ok(());
        }
        if bar.bar_type != self.config.five_minute_bar_type {
            return Ok(());
        }
        let finalized = if self.backtest_four_hour_bars.is_some() {
            self.advance_backtest_four_hour_bars(bar.ts_init)?;
            *bar
        } else {
            let Some(finalized) = self.signals.finalize_five_minute(*bar) else {
                return Ok(());
            };
            finalized
        };
        self.last_five_minute_bar = Some(finalized);
        self.inspect_ambiguous_exit_bar(finalized);
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
                self.request_exit(TradeExitReason::RiskExit, "unexpected overnight exposure")?;
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
                self.request_exit(TradeExitReason::RiskExit, "five-minute market data gap")?;
            }
        }
        self.last_five_minute_bar_start = Some(finalized.ts_event);
        if let Some(active) = self
            .active_trade
            .as_mut()
            .filter(|active| finalized.ts_event >= active.first_fill_ts)
        {
            active.observe_bar(finalized);
        }
        let within_entry_window = close_minute >= self.config.entry_start_minute
            && close_minute <= self.config.entry_end_minute;
        let allow_signal = within_entry_window
            && !suppress_signal
            && !self.faulted
            && !self.session_disabled
            && !self.exit_pending
            && !self.has_exposure();
        let signal = self.signals.process_five_minute(finalized, allow_signal);
        let funnel = self.signals.funnel;
        self.update_run_statistics(|statistics| statistics.funnel = funnel);
        if self.config.log_bars {
            log::info!(
                "[{}] 5m bar collected: start={}, open={}, high={}, low={}, close={}, volume={}, 4h_trend={:?}, atr={:.6}, stochastic_k={:.2}, demand_zones={}, supply_zones={}, indicators_initialized={}, structure_initialized={}, session_disabled={}",
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
                self.signals.indicators_initialized(),
                self.signals.structure.initialized(),
                self.session_disabled,
            );
        }
        if close_minute >= self.config.flatten_minute {
            self.session_disabled = true;
            if should_request_preclose_exit(
                close_minute,
                self.config.flatten_minute,
                self.has_exposure(),
                self.exit_pending,
            ) {
                self.request_exit(TradeExitReason::PreClose, "pre-close risk cutoff")?;
            }
            return Ok(());
        }
        if let Some(active) = self.active_trade.as_ref() {
            let bars_held = active.bars_held;
            let mfe_r = active.mfe_r();
            if should_request_time_stop(
                bars_held,
                mfe_r,
                self.config.time_stop_bars,
                self.config.time_stop_minimum_mfe_r,
                self.has_exposure(),
                self.exit_pending,
            ) {
                log::info!(
                    "[{}] SLC time stop reached: bars_held={}, mfe_r={}, required_mfe_r={}",
                    self.config.instrument_id,
                    bars_held,
                    mfe_r.round_dp(4),
                    self.config.time_stop_minimum_mfe_r,
                );
                self.request_exit(
                    TradeExitReason::TimeStop,
                    "position failed to make the configured favorable excursion",
                )?;
                return Ok(());
            }
        }
        self.cancel_stale_entry(finalized)?;
        if self.backtest_four_hour_bars.is_none() && self.target_reached(finalized) {
            self.request_exit(
                TradeExitReason::Target,
                "five-minute bar traded through the actual-fill-based 2R target",
            )?;
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

/// Returns whether a completed bar not predating the first fill reached the target.
fn bar_reaches_target(side: OrderSide, target: Price, first_fill_ts: UnixNanos, bar: Bar) -> bool {
    bar.ts_event >= first_fill_ts
        && match side {
            OrderSide::Buy => bar.high >= target,
            OrderSide::Sell => bar.low <= target,
            OrderSide::NoOrderSide => false,
        }
}

/// Returns the largest lot-aligned quantity fitting risk, quantity, and worst-price notional caps.
fn risk_sized_quantity(
    entry: Price,
    stop: Price,
    risk_amount: Decimal,
    max_quantity: Quantity,
    max_notional: Decimal,
    lot_size: Quantity,
) -> anyhow::Result<Option<Quantity>> {
    anyhow::ensure!(entry.as_decimal() > Decimal::ZERO, "entry must be positive");
    let risk_per_share = (entry.as_decimal() - stop.as_decimal()).abs();
    anyhow::ensure!(
        risk_per_share > Decimal::ZERO,
        "risk per share must be positive"
    );
    let lot = lot_size.as_decimal();
    anyhow::ensure!(lot > Decimal::ZERO, "instrument lot size must be positive");
    anyhow::ensure!(
        max_notional > Decimal::ZERO,
        "maximum notional must be positive"
    );
    let risk_quantity = ((risk_amount / risk_per_share) / lot).floor() * lot;
    let notional_quantity = ((max_notional / entry.as_decimal()) / lot).floor() * lot;
    let capped = risk_quantity
        .min(max_quantity.as_decimal())
        .min(notional_quantity);
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

/// Loads exact Longbridge instrument metadata once for every configured symbol.
async fn load_instruments(
    config: &AppConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<(InstrumentId, InstrumentAny)>> {
    let symbols = config
        .instruments
        .iter()
        .map(|instrument| instrument.instrument_id.symbol.as_str())
        .collect::<Vec<_>>();
    let static_info = quote_api_call_with_retry(|| context.static_info(symbols.clone()))
        .await
        .context("failed to request Longbridge static security info")?;
    let mut static_info_by_symbol = static_info
        .into_iter()
        .map(|info| (info.symbol.clone(), info))
        .collect::<HashMap<_, _>>();
    let mut instruments = Vec::with_capacity(config.instruments.len());
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
        instruments.push((instrument_id, instrument));
    }
    Ok(instruments)
}

/// Loads exact instrument metadata and complete warmup histories before the live node starts.
async fn prepare_inputs(
    config: &AppConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<PreparedInputs>> {
    let (market_close, market_close_minute) = current_us_market_close(context).await?;
    let mut prepared = Vec::with_capacity(config.instruments.len());

    for (instrument_id, instrument) in load_instruments(config, context).await? {
        let symbol = instrument_id.symbol.as_str();
        let five_minute_bars = load_warmup_bars(
            context,
            symbol,
            Period::FiveMinute,
            AppConfig::five_minute_bar_type(instrument_id),
            config.five_minute_warmup,
            instrument.price_precision(),
        )
        .await?;
        let four_hour_bars = load_warmup_bars(
            context,
            symbol,
            Period::FourHour,
            AppConfig::four_hour_bar_type(instrument_id),
            config.four_hour_warmup,
            instrument.price_precision(),
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
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let request_count = count
        .checked_add(1)
        .filter(|request_count| *request_count <= MAX_WARMUP_BARS)
        .context("warmup count must leave room for one in-progress Longbridge bar")?;
    let candlesticks = quote_api_call_with_retry(|| {
        context.candlesticks(
            symbol,
            period,
            request_count,
            AdjustType::NoAdjust,
            TradeSessions::Intraday,
        )
    })
    .await
    .with_context(|| format!("failed to request {period:?} warmup bars for {symbol}"))?;
    let bars = parse_warmup_bars(
        symbol,
        period,
        bar_type,
        candlesticks,
        count,
        OffsetDateTime::now_utc(),
        price_precision,
    )?;
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

/// Removes in-progress candles, then parses, orders, and validates one warmup response.
fn parse_warmup_bars(
    symbol: &str,
    period: Period,
    bar_type: BarType,
    candlesticks: Vec<Candlestick>,
    count: usize,
    now: OffsetDateTime,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let bar_duration = match period {
        Period::FiveMinute => time::Duration::minutes(5),
        Period::FourHour => time::Duration::hours(4),
        _ => anyhow::bail!("unsupported SLC warmup period: {period:?}"),
    };
    let mut bars = candlesticks
        .into_iter()
        .filter(|candlestick| {
            candlestick
                .timestamp
                .checked_add(bar_duration)
                .is_some_and(|closed_at| closed_at <= now)
        })
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
    anyhow::ensure!(
        bars.len() >= count,
        "Longbridge returned {} of {count} required {period:?} warmup bars for {symbol}",
        bars.len(),
    );
    if bars.len() > count {
        bars = bars.split_off(bars.len() - count);
    }
    Ok(bars)
}

#[derive(Clone)]
struct PreparedBacktestInputs {
    instrument_id: InstrumentId,
    instrument: InstrumentAny,
    five_minute_warmup: Vec<Bar>,
    four_hour_warmup: Vec<Bar>,
    five_minute_bars: Vec<Bar>,
    four_hour_bars: Vec<Bar>,
}

#[derive(Clone, Copy, Debug)]
struct BacktestEvaluation {
    risk_reward: Decimal,
    trades: u64,
    conservative_pnl: Decimal,
    conservative_cost_adjusted_sharpe: Option<f64>,
    engine_sharpe: Option<f64>,
}

impl BacktestEvaluation {
    /// Produces one grep-friendly comparison record for target selection and review.
    fn summary(self, sample: &str) -> String {
        format!(
            "SLC parameter evaluation: sample={sample}, risk_reward={}, trades={}, conservative_pnl={}, conservative_cost_adjusted_sharpe={}, engine_sharpe={}",
            self.risk_reward,
            self.trades,
            self.conservative_pnl,
            format_optional_metric(self.conservative_cost_adjusted_sharpe),
            format_optional_metric(self.engine_sharpe),
        )
    }
}

/// Formats optional floating-point metrics without replacing unavailable evidence with zero.
fn format_optional_metric(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| format!("{value:.4}"))
}

/// Selects only by conservative IS Sharpe, then PnL and the smaller target for deterministic ties.
fn select_in_sample_winner(
    evaluations: &[BacktestEvaluation],
) -> anyhow::Result<BacktestEvaluation> {
    evaluations
        .iter()
        .copied()
        .filter(|evaluation| evaluation.conservative_cost_adjusted_sharpe.is_some())
        .max_by(|left, right| {
            left.conservative_cost_adjusted_sharpe
                .expect("filtered above")
                .total_cmp(
                    &right
                        .conservative_cost_adjusted_sharpe
                        .expect("filtered above"),
                )
                .then_with(|| left.conservative_pnl.cmp(&right.conservative_pnl))
                .then_with(|| right.risk_reward.cmp(&left.risk_reward))
        })
        .context("no target candidate produced a defined in-sample Sharpe")
}

/// Returns the most recent complete bars before the OOS boundary as indicator warmup.
fn split_warmup_bars(
    initial_warmup: &[Bar],
    replay_bars: &[Bar],
    split: UnixNanos,
    count: usize,
    instrument_id: InstrumentId,
    bar_type: BarType,
) -> anyhow::Result<Vec<Bar>> {
    let mut bars = initial_warmup
        .iter()
        .chain(replay_bars)
        .copied()
        .filter(|bar| bar.ts_event < split)
        .collect::<Vec<_>>();
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    anyhow::ensure!(
        bars.len() >= count,
        "walk-forward warmup requires {count} bars for {instrument_id}: bar_type={bar_type}, available={}",
        bars.len(),
    );
    Ok(bars.split_off(bars.len() - count))
}

/// Splits every symbol at the same timestamp without allowing OOS bars into IS selection.
fn split_walk_forward_inputs(
    inputs: &[PreparedBacktestInputs],
    config: &AppConfig,
    split: Timestamp,
) -> anyhow::Result<(Vec<PreparedBacktestInputs>, Vec<PreparedBacktestInputs>)> {
    let split = UnixNanos::from(split);
    let mut in_sample = Vec::with_capacity(inputs.len());
    let mut out_of_sample = Vec::with_capacity(inputs.len());
    for input in inputs {
        let mut train = input.clone();
        train.five_minute_bars.retain(|bar| bar.ts_event < split);
        train.four_hour_bars.retain(|bar| bar.ts_event < split);

        let test = PreparedBacktestInputs {
            instrument_id: input.instrument_id,
            instrument: input.instrument.clone(),
            five_minute_warmup: split_warmup_bars(
                &input.five_minute_warmup,
                &input.five_minute_bars,
                split,
                config.five_minute_warmup,
                input.instrument_id,
                AppConfig::five_minute_bar_type(input.instrument_id),
            )?,
            four_hour_warmup: split_warmup_bars(
                &input.four_hour_warmup,
                &input.four_hour_bars,
                split,
                config.four_hour_warmup,
                input.instrument_id,
                AppConfig::four_hour_bar_type(input.instrument_id),
            )?,
            five_minute_bars: input
                .five_minute_bars
                .iter()
                .copied()
                .filter(|bar| bar.ts_event >= split)
                .collect(),
            four_hour_bars: input
                .four_hour_bars
                .iter()
                .copied()
                .filter(|bar| bar.ts_event >= split)
                .collect(),
        };
        anyhow::ensure!(
            !train.five_minute_bars.is_empty() && !test.five_minute_bars.is_empty(),
            "walk-forward split leaves an empty 5m sample for {}",
            input.instrument_id,
        );
        in_sample.push(train);
        out_of_sample.push(test);
    }
    Ok((in_sample, out_of_sample))
}

/// Loads warmup and replay bars for every symbol through the rate-limited Longbridge context.
async fn prepare_backtest_inputs(
    config: &SlcBacktestConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<PreparedBacktestInputs>> {
    let start_date = us_market_date(config.start)?;
    let end_date = us_market_date(config.end)?;
    let mut half_days = BTreeSet::new();
    let mut cursor = start_date;
    while cursor <= end_date {
        // Longbridge requires each trading-days query interval to be shorter than one month.
        let chunk_end = cursor
            .checked_add(time::Duration::days(TRADING_DAYS_CHUNK_DAYS - 1))
            .unwrap_or(end_date)
            .min(end_date);
        half_days.extend(
            quote_api_call_with_retry(|| context.trading_days(Market::US, cursor, chunk_end))
                .await
                .with_context(|| {
                    format!(
                        "failed to query US half trading days from {cursor} through {chunk_end}",
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
    let mut prepared = Vec::with_capacity(config.strategy.instruments.len());
    for (instrument_id, instrument) in load_instruments(&config.strategy, context).await? {
        let symbol = instrument_id.symbol.as_str();
        let five_minute_bar_type = AppConfig::five_minute_bar_type(instrument_id);
        let four_hour_bar_type = AppConfig::four_hour_bar_type(instrument_id);
        let five_minute_warmup = load_backtest_warmup_bars(
            context,
            symbol,
            Period::FiveMinute,
            five_minute_bar_type,
            config.strategy.five_minute_warmup,
            config.start,
            instrument.price_precision(),
        )
        .await?;
        let four_hour_warmup = load_backtest_warmup_bars(
            context,
            symbol,
            Period::FourHour,
            four_hour_bar_type,
            config.strategy.four_hour_warmup,
            config.start,
            instrument.price_precision(),
        )
        .await?;
        let downloaded_five_minute_bars = load_backtest_bars(
            context,
            symbol,
            Period::FiveMinute,
            five_minute_bar_type,
            config.start,
            config.end,
            instrument.price_precision(),
        )
        .await?;
        let mut five_minute_bars = Vec::with_capacity(downloaded_five_minute_bars.len());
        for bar in downloaded_five_minute_bars {
            let timestamp = Timestamp::from_nanosecond(i128::from(bar.ts_event.as_u64()))?;
            if !half_days.contains(&us_market_date(timestamp)?) {
                five_minute_bars.push(bar);
            }
        }
        anyhow::ensure!(
            !five_minute_bars.is_empty(),
            "Longbridge returned no complete non-half-day 5m bars for {symbol} in {}..={}",
            config.start,
            config.end,
        );
        let four_hour_bars = load_backtest_bars(
            context,
            symbol,
            Period::FourHour,
            four_hour_bar_type,
            config.start,
            config.end,
            instrument.price_precision(),
        )
        .await?;
        log::info!(
            "[{instrument_id}] SLC backtest data ready: 5m_warmup={}, 4h_warmup={}, 5m_bars={}, 4h_bars={}, skipped_half_days={}",
            five_minute_warmup.len(),
            four_hour_warmup.len(),
            five_minute_bars.len(),
            four_hour_bars.len(),
            half_days.len(),
        );
        prepared.push(PreparedBacktestInputs {
            instrument_id,
            instrument,
            five_minute_warmup,
            four_hour_warmup,
            five_minute_bars,
            four_hour_bars,
        });
    }
    Ok(prepared)
}

/// Loads bars ending no later than the backtest start for indicator and structure warmup.
async fn load_backtest_warmup_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    count: usize,
    start: Timestamp,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let request_count = count
        .checked_add(1)
        .filter(|request_count| *request_count <= MAX_WARMUP_BARS)
        .context("warmup count must leave room for the first non-warmup bar")?;
    let start_datetime = us_market_datetime(start)?;
    let candlesticks = quote_api_call_with_retry(|| {
        context.history_candlesticks_by_offset(
            symbol,
            period,
            AdjustType::NoAdjust,
            false,
            Some(start_datetime),
            request_count,
            TradeSessions::Intraday,
        )
    })
    .await
    .with_context(|| format!("failed to request historical {period:?} warmup for {symbol}"))?;
    parse_warmup_bars(
        symbol,
        period,
        bar_type,
        candlesticks,
        count,
        OffsetDateTime::from_unix_timestamp_nanos(start.as_nanosecond())?,
        price_precision,
    )
}

/// Downloads a bounded date range in small chunks so Longbridge cannot silently truncate 5m data.
async fn load_backtest_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    start: Timestamp,
    end: Timestamp,
    price_precision: u8,
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
                    period,
                    AdjustType::NoAdjust,
                    Some(cursor),
                    Some(chunk_end),
                    TradeSessions::Intraday,
                )
            })
            .await
            .with_context(|| {
                format!(
                    "failed to request historical {period:?} bars for {symbol} from {cursor} through {chunk_end}",
                )
            })?,
        );
        if chunk_end == end_date {
            break;
        }
        cursor = chunk_end
            .next_day()
            .context("historical date range exceeded the supported calendar")?;
    }
    parse_backtest_bars(
        symbol,
        period,
        bar_type,
        candlesticks,
        start,
        end,
        price_precision,
    )
}

/// Parses complete historical candles and assigns their close as the replay arrival timestamp.
fn parse_backtest_bars(
    symbol: &str,
    period: Period,
    bar_type: BarType,
    candlesticks: Vec<Candlestick>,
    start: Timestamp,
    end: Timestamp,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let duration = match period {
        Period::FiveMinute => FIVE_MINUTE_NANOS,
        Period::FourHour => FOUR_HOUR_NANOS,
        _ => anyhow::bail!("unsupported SLC backtest period: {period:?}"),
    };
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
    bars.retain_mut(|bar| {
        let Some(close) = bar.ts_event.checked_add(duration) else {
            return false;
        };
        if bar.ts_event < start || close > end {
            return false;
        }
        bar.ts_init = close;
        true
    });
    anyhow::ensure!(
        !bars.is_empty(),
        "Longbridge returned no complete {period:?} bars for {symbol}: bar_type={bar_type}, start={start}, end={end}",
    );
    Ok(bars)
}

/// Converts a UTC timestamp to the market-local datetime expected by Longbridge offset history.
fn us_market_datetime(timestamp: Timestamp) -> anyhow::Result<PrimitiveDateTime> {
    let local = timestamp.to_zoned(get_timezone(US_TIMEZONE)?);
    let date = local.date();
    Ok(PrimitiveDateTime::new(
        Date::from_calendar_date(
            i32::from(date.year()),
            Month::try_from(u8::try_from(date.month())?)?,
            u8::try_from(date.day())?,
        )?,
        Time::from_hms(
            u8::try_from(local.hour())?,
            u8::try_from(local.minute())?,
            0,
        )?,
    ))
}

/// Returns today's authoritative Longbridge US regular-session close and local minute.
async fn current_us_market_close(context: &QuoteContext) -> anyhow::Result<(Timestamp, u16)> {
    let now = Timestamp::now();
    let market_date = us_market_date(now)?;
    let trading_days =
        quote_api_call_with_retry(|| context.trading_days(Market::US, market_date, market_date))
            .await
            .context("failed to query the current US trading day from Longbridge")?;
    if !trading_days.trading_days.contains(&market_date) {
        anyhow::bail!("{market_date} is not a US trading day");
    }

    let close_time = if trading_days.half_trading_days.contains(&market_date) {
        time::macros::time!(13:00)
    } else {
        quote_api_call_with_retry(|| context.trading_session())
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
                    config.per_position_notional_limit().to_string(),
                )
            })
            .collect(),
        ..Default::default()
    }
}

struct TemporaryRiskState(PathBuf);

impl Drop for TemporaryRiskState {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "Failed to remove temporary SLC backtest risk state {}: {error}",
                self.0.display(),
            );
        }
    }
}

/// Takes a consistent snapshot of shared per-symbol diagnostics after a runner stops.
fn run_statistics_lines(run_statistics: &Mutex<RunStatistics>) -> anyhow::Result<Vec<String>> {
    Ok(run_statistics
        .lock()
        .map_err(|_| anyhow::anyhow!("SLC run statistics mutex was poisoned"))?
        .lines())
}

/// Replays only five-minute bars through matching while four-hour bars advance strategy structure.
fn run_backtest_engine(
    config: &SlcBacktestConfig,
    prepared: Vec<PreparedBacktestInputs>,
    sample: &str,
) -> anyhow::Result<BacktestEvaluation> {
    let _cleanup = TemporaryRiskState(config.strategy.risk_state_path.clone());
    let account_risk = Arc::new(AccountRisk::load(config.strategy.risk_state_path.clone())?);
    let run_statistics = Arc::new(Mutex::new(RunStatistics::default()));
    let trading_days = prepared
        .iter()
        .flat_map(|input| input.five_minute_bars.iter())
        .map(|bar| {
            bar.ts_event
                .to_datetime_utc()
                .to_zoned(config.strategy.timezone.clone())
                .date()
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    println!(
        "SLC backtest run: sample={sample}, risk_reward={}, trading_days={}",
        config.strategy.risk_reward,
        trading_days.len(),
    );
    let flatten_minute = RTH_CLOSE_MINUTE
        .checked_sub(config.strategy.session.flatten_before_close_minutes)
        .context("flatten buffer exceeds the regular US trading session")?;
    anyhow::ensure!(
        config.strategy.session.entry_start_minute < flatten_minute,
        "entry window starts after the SLC backtest pre-close flatten time",
    );

    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(
                config
                    .strategy
                    .instruments
                    .first()
                    .context("SLC backtest requires at least one instrument")?
                    .instrument_id
                    .venue,
            )
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![config.starting_balance])
            .reject_stop_orders(false)
            .bar_adaptive_high_low_ordering(true)
            .build()?,
    )?;

    let symbol_count = prepared.len();
    let mut bar_count = 0;
    let mut data = Vec::new();
    for prepared in prepared {
        engine.add_instrument(&prepared.instrument)?;
        bar_count += prepared.five_minute_bars.len();
        data.extend(prepared.five_minute_bars.into_iter().map(Data::Bar));
        engine.add_strategy(SlcStrategy::new(
            &config.strategy,
            prepared.instrument_id,
            prepared.instrument,
            prepared.five_minute_warmup,
            prepared.four_hour_warmup,
            Arc::clone(&account_risk),
            SlcRunConfig {
                flatten_minute,
                backtest_four_hour_bars: Some(prepared.four_hour_bars),
                round_trip_cost_per_share: config.round_trip_cost_per_share,
                log_bars: config.log_bars,
                run_statistics: Arc::clone(&run_statistics),
            },
        )?)?;
    }
    engine.add_data(data, None, true, true)?;
    engine.run(None, None, None, false)?;

    for line in run_statistics_lines(&run_statistics)? {
        println!("{line}");
    }

    let (aggregate, conservative_cost_adjusted_sharpe) = {
        let statistics = run_statistics
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC run statistics mutex was poisoned"))?;
        (
            TradeAggregate::from_trades(
                statistics
                    .symbols
                    .values()
                    .flat_map(|symbol| symbol.trades.iter()),
            ),
            conservative_cost_adjusted_sharpe(
                &statistics,
                &trading_days,
                config.starting_balance,
                &config.strategy.timezone,
            ),
        )
    };

    let result = engine.get_result();
    println!(
        "SLC backtest complete: symbols={symbol_count}, 5m_bars={bar_count}, orders={}, positions={}",
        result.total_orders, result.total_positions,
    );
    println!("Summary: {:?}", result.summary);
    println!("PnL statistics: {:?}", result.stats_pnls);
    println!("Return statistics: {:?}", result.stats_returns);
    println!("General statistics: {:?}", result.stats_general);
    let evaluation = BacktestEvaluation {
        risk_reward: config.strategy.risk_reward,
        trades: aggregate.trades,
        conservative_pnl: aggregate.conservative_pnl,
        conservative_cost_adjusted_sharpe,
        engine_sharpe: result.stats_returns.get("Sharpe Ratio (252 days)").copied(),
    };
    println!("{}", evaluation.summary(sample));
    Ok(evaluation)
}

/// Selects the live or historical runner while keeping the SLC implementation shared.
pub(super) async fn run(backtest: bool) -> anyhow::Result<()> {
    if backtest {
        run_backtest().await
    } else {
        run_live().await
    }
}

/// Downloads Longbridge history and runs the parameterized multi-symbol SLC backtest.
async fn run_backtest() -> anyhow::Result<()> {
    let config = SlcBacktestConfig::from_env()?;
    let prepared = tokio::time::timeout(Duration::from_secs(config.timeout_secs), async {
        let sdk_config = config.strategy.data_config().sdk_config().await?;
        let (context, _receiver) = QuoteContext::new(sdk_config);
        prepare_backtest_inputs(&config, &context).await
    })
    .await
    .with_context(|| {
        format!(
            "SLC historical download exceeded {} seconds",
            config.timeout_secs,
        )
    })??;
    if let Some(split) = config.walk_forward_split {
        let (in_sample, out_of_sample) =
            split_walk_forward_inputs(&prepared, &config.strategy, split)?;
        let mut in_sample_evaluations = Vec::with_capacity(config.risk_rewards.len());
        for risk_reward in &config.risk_rewards {
            let mut candidate = config.clone();
            candidate.strategy.risk_reward = *risk_reward;
            in_sample_evaluations.push(run_backtest_engine(&candidate, in_sample.clone(), "IS")?);
        }
        let winner = select_in_sample_winner(&in_sample_evaluations)?;
        let mut candidate = config.clone();
        candidate.strategy.risk_reward = winner.risk_reward;
        let out_of_sample_evaluation = run_backtest_engine(&candidate, out_of_sample, "OOS")?;
        let in_sample_sharpe = winner
            .conservative_cost_adjusted_sharpe
            .expect("winner requires a defined Sharpe");
        let out_of_sample_sharpe = out_of_sample_evaluation.conservative_cost_adjusted_sharpe;
        let verdict = out_of_sample_sharpe.map_or("reject_undefined_oos", |sharpe| {
            walk_forward_verdict(in_sample_sharpe, sharpe)
        });
        let degradation = out_of_sample_sharpe
            .filter(|_| in_sample_sharpe != 0.0)
            .map(|sharpe| sharpe / in_sample_sharpe);
        println!(
            "SLC walk-forward result: split={split}, selected_risk_reward={}, is_conservative_cost_adjusted_sharpe={}, oos_conservative_cost_adjusted_sharpe={}, degradation_ratio={}, verdict={verdict}",
            winner.risk_reward,
            format_optional_metric(Some(in_sample_sharpe)),
            format_optional_metric(out_of_sample_sharpe),
            format_optional_metric(degradation),
        );
        Ok(())
    } else {
        for risk_reward in &config.risk_rewards {
            let mut candidate = config.clone();
            candidate.strategy.risk_reward = *risk_reward;
            run_backtest_engine(&candidate, prepared.clone(), "FULL")?;
        }
        Ok(())
    }
}

/// Prepares all symbols before constructing the reconciled live trading node.
async fn run_live() -> anyhow::Result<()> {
    let config = AppConfig::from_env(true)?;
    let account_risk = Arc::new(AccountRisk::load(config.risk_state_path.clone())?);
    let run_statistics = Arc::new(Mutex::new(RunStatistics::default()));
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
            Arc::clone(&account_risk),
            SlcRunConfig {
                flatten_minute,
                backtest_four_hour_bars: None,
                round_trip_cost_per_share: Decimal::ZERO,
                log_bars: true,
                run_statistics: Arc::clone(&run_statistics),
            },
        )?)?;
    }
    schedule_market_close_stop(&node, market_close)?;
    let result = node.run().await;
    match run_statistics_lines(&run_statistics) {
        Ok(lines) => {
            for line in lines {
                log::info!("{line}");
            }
        }
        Err(e) => log::error!("Failed to report SLC run statistics: {e:#}"),
    }
    result
}

#[cfg(test)]
mod tests {
    use nautilus_model::{
        enums::LiquiditySide,
        identifiers::{TradeId, VenueOrderId},
    };

    use super::*;

    fn sdk_candlestick(
        open: &str,
        high: &str,
        low: &str,
        close: &str,
        timestamp: &str,
    ) -> Candlestick {
        toml::from_str(&format!(
            r#"
open = "{open}"
high = "{high}"
low = "{low}"
close = "{close}"
volume = 100
turnover = "10000"
timestamp = "{timestamp}"
trade_session = "Intraday"
open_updated = true
"#,
        ))
        .unwrap()
    }

    fn bar(
        bar_type: BarType,
        open: &str,
        high: &str,
        low: &str,
        close: &str,
        timestamp: u64,
    ) -> Bar {
        let price =
            |value: &str| Price::from_decimal_dp(value.parse::<Decimal>().unwrap(), 2).unwrap();
        Bar::new(
            bar_type,
            price(open),
            price(high),
            price(low),
            price(close),
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

    #[test]
    fn warmup_ignores_invalid_in_progress_bar() {
        let bars = parse_warmup_bars(
            "QQQ.US",
            Period::FiveMinute,
            BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            vec![
                sdk_candlestick(
                    "100.00",
                    "101.00",
                    "99.00",
                    "100.50",
                    "2026-09-04T13:25:00Z",
                ),
                sdk_candlestick("0.00", "101.00", "100.00", "100.50", "2026-09-04T13:30:00Z"),
            ],
            1,
            time::macros::datetime!(2026-09-04 13:31 UTC),
            2,
        )
        .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(
            bars[0].ts_event,
            UnixNanos::from(
                u64::try_from(
                    time::macros::datetime!(2026-09-04 13:25 UTC).unix_timestamp_nanos(),
                )
                .unwrap(),
            ),
        );
    }

    #[test]
    fn completed_warmup_bar_normalizes_provider_ohlc_envelope() {
        let bar_type = BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL");
        let bars = parse_warmup_bars(
            "QQQ.US",
            Period::FiveMinute,
            bar_type,
            vec![sdk_candlestick(
                "99.00",
                "101.00",
                "100.00",
                "100.50",
                "2026-09-04T13:25:00Z",
            )],
            1,
            time::macros::datetime!(2026-09-04 13:31 UTC),
            2,
        )
        .unwrap();

        assert_eq!(bars[0].open, Price::from("99.00"));
        assert_eq!(bars[0].high, Price::from("101.00"));
        assert_eq!(bars[0].low, Price::from("99.00"));
        assert_eq!(bars[0].close, Price::from("100.50"));
    }

    #[rstest::rstest]
    fn backtest_bars_are_deduplicated_and_arrive_at_close() {
        let bar_type = BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL");
        let start = "2026-09-04T13:25:00Z".parse::<Timestamp>().unwrap();
        let end = "2026-09-04T13:35:00Z".parse::<Timestamp>().unwrap();
        let bars = parse_backtest_bars(
            "QQQ.US",
            Period::FiveMinute,
            bar_type,
            vec![
                sdk_candlestick(
                    "100.00",
                    "101.00",
                    "99.00",
                    "100.50",
                    "2026-09-04T13:20:00Z",
                ),
                sdk_candlestick(
                    "100.123",
                    "101.234",
                    "99.012",
                    "100.567",
                    "2026-09-04T13:25:00Z",
                ),
                sdk_candlestick(
                    "100.123",
                    "102.345",
                    "99.012",
                    "101.567",
                    "2026-09-04T13:25:00Z",
                ),
                sdk_candlestick(
                    "101.567",
                    "102.345",
                    "101.123",
                    "101.789",
                    "2026-09-04T13:30:00Z",
                ),
                sdk_candlestick(
                    "101.75",
                    "103.00",
                    "101.00",
                    "102.50",
                    "2026-09-04T13:35:00Z",
                ),
            ],
            start,
            end,
            2,
        )
        .unwrap();

        assert_eq!(bars.len(), 2);
        assert!(bars.iter().all(|bar| {
            [bar.open, bar.high, bar.low, bar.close]
                .iter()
                .all(|price| price.precision == 2)
        }));
        assert_eq!(bars[0].ts_event, UnixNanos::from(start));
        assert_eq!(
            bars[0].ts_init,
            UnixNanos::from(start)
                .checked_add(FIVE_MINUTE_NANOS)
                .unwrap(),
        );
        assert_eq!(bars[1].ts_init, UnixNanos::from(end));
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
            zone_ttl_bars: 234,
            max_zones_per_side: 8,
            confirmation_window_bars: 6,
            confirmation_max_distance_atr: 0.35,
            displacement_atr_multiple: 1.0,
            displacement_close_fraction: 0.35,
            displacement_max_bars: 3,
            oversold: 20.0,
            overbought: 80.0,
        }
    }

    #[test]
    fn five_minute_indicators_initialize_without_four_hour_structure() {
        let mut signals = SlcSignalState {
            five_minute_bars: FinalBarBuffer::default(),
            four_hour_bars: FinalBarBuffer::default(),
            structure: PivotStructure::new(2),
            atr: AverageTrueRange::new(1, Some(MovingAverageType::Wilder), Some(true), None),
            stochastics: Stochastics::new_with_params(
                1,
                1,
                1,
                MovingAverageType::Simple,
                StochasticsDMethod::MovingAverage,
            ),
            recent_five_minute_bars: VecDeque::new(),
            last_demand_source: None,
            last_supply_source: None,
            previous_k: None,
            demand: VecDeque::new(),
            supply: VecDeque::new(),
            funnel: SignalFunnel::default(),
            rules: signal_rules(),
        };

        let _ = signals.process_five_minute(five_minute_bar("100", "101", "99", "100", 1), false);

        assert!(signals.atr.initialized());
        assert!(signals.stochastics.initialized());
        assert!(!signals.structure.initialized());
        assert_eq!(signals.structure.trend(), Trend::Neutral);
        assert!(signals.indicators_initialized());
    }

    #[test]
    fn backtest_uses_standard_stop_market_semantics() {
        assert_eq!(protective_stop_order_type(true), OrderType::StopMarket);
        assert_eq!(
            protective_stop_order_type(false),
            OrderType::MarketIfTouched,
        );
    }

    #[test]
    fn backtest_protection_links_resting_target_and_stop() {
        let event = OrderFilled::new(
            TraderId::from("TRADER-001"),
            StrategyId::from("SLC-001"),
            InstrumentId::from("QQQ.US.LONGBRIDGE"),
            ClientOrderId::from("ENTRY-001"),
            VenueOrderId::from("VENUE-001"),
            AccountId::from("ACCOUNT-001"),
            TradeId::from("TRADE-001"),
            OrderSide::Buy,
            OrderType::Limit,
            Quantity::from(10),
            Price::from("100.00"),
            Currency::USD(),
            LiquiditySide::Taker,
            UUID4::new(),
            UnixNanos::from(1),
            UnixNanos::from(1),
            false,
            None,
            None,
            None,
        );
        let stop_order_id = ClientOrderId::from("STOP-001");
        let target_order_id = ClientOrderId::from("TARGET-001");
        let orders = backtest_protective_orders(
            &event,
            OrderSide::Sell,
            Price::from("99.00"),
            Price::from("102.00"),
            OrderListId::from("OL-001"),
            stop_order_id,
            target_order_id,
        )
        .unwrap();

        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].order_type(), OrderType::StopMarket);
        assert_eq!(orders[0].trigger_price(), Some(Price::from("99.00")));
        assert_eq!(orders[0].linked_order_ids(), Some(&[target_order_id][..]));
        assert_eq!(orders[1].order_type(), OrderType::Limit);
        assert_eq!(orders[1].price(), Some(Price::from("102.00")));
        assert_eq!(orders[1].linked_order_ids(), Some(&[stop_order_id][..]));
        assert!(orders.iter().all(|order| {
            order.contingency_type() == Some(ContingencyType::Ouo)
                && order.order_list_id() == Some(OrderListId::from("OL-001"))
                && order.is_reduce_only()
                && order.time_in_force() == TimeInForce::Day
                && order.parent_order_id().is_none()
        }));
    }

    #[test]
    fn configured_symbols_have_unique_order_id_tags() {
        let qqq = StrategyCore::new(slc_strategy_config(InstrumentId::from("QQQ.US.LONGBRIDGE")));
        let aapl = StrategyCore::new(slc_strategy_config(InstrumentId::from(
            "AAPL.US.LONGBRIDGE",
        )));

        assert_eq!(qqq.order_id_tag(), Some("QQQ.US"));
        assert_eq!(aapl.order_id_tag(), Some("AAPL.US"));
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
    #[case(949, true, false, false)]
    #[case(950, false, false, false)]
    #[case(950, true, true, false)]
    #[case(950, true, false, true)]
    fn preclose_exit_requires_unmanaged_exposure(
        #[case] close_minute: u16,
        #[case] has_exposure: bool,
        #[case] exit_pending: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(
            should_request_preclose_exit(close_minute, 950, has_exposure, exit_pending),
            expected,
        );
    }

    #[rstest::rstest]
    fn test_supply_and_demand_displacement_rules() {
        let rules = signal_rules();
        let bearish_base = five_minute_bar("100.0", "101.0", "98.0", "99.0", 1);
        let bullish_displacement = five_minute_bar("99.0", "104.0", "99.0", "103.5", 2);
        let bullish_base = five_minute_bar("100.0", "102.0", "99.0", "101.0", 3);
        let bearish_displacement = five_minute_bar("101.0", "101.0", "96.0", "96.5", 4);

        assert_eq!(
            displacement_zone(
                &VecDeque::from([bearish_base, bullish_displacement]),
                2.0,
                rules,
            )
            .map(|(kind, source, _)| (kind, source)),
            Some((ZoneKind::Demand, bearish_base)),
        );
        assert_eq!(
            displacement_zone(
                &VecDeque::from([bullish_base, bearish_displacement]),
                2.0,
                rules,
            )
            .map(|(kind, source, _)| (kind, source)),
            Some((ZoneKind::Supply, bullish_base)),
        );
    }

    #[rstest::rstest]
    fn test_displacement_can_complete_across_three_bars() {
        let rules = signal_rules();
        let source = five_minute_bar("100.0", "101.0", "98.0", "99.0", 1);
        let first = five_minute_bar("99.0", "100.0", "98.8", "99.8", 2);
        let second = five_minute_bar("99.8", "100.8", "99.6", "100.6", 3);
        let third = five_minute_bar("100.6", "102.0", "100.5", "101.8", 4);

        assert_eq!(
            displacement_zone(&VecDeque::from([source, first, second]), 2.0, rules)
                .map(|(kind, source, _)| (kind, source)),
            None,
        );
        assert_eq!(
            displacement_zone(&VecDeque::from([source, first, second, third]), 2.0, rules,)
                .map(|(kind, source, _)| (kind, source)),
            Some((ZoneKind::Demand, source)),
        );
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
    fn test_zone_accepts_reentry_on_the_touch_bar() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch_and_reentry = five_minute_bar("102", "102", "100", "101", 2);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Demand, source)]);

        assert!(
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
            )
            .is_some(),
        );
        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_zone_rejects_confirmation_after_price_leaves_the_level() {
        let rules = SignalRules {
            confirmation_max_distance_atr: 0.25,
            ..signal_rules()
        };
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch = five_minute_bar("101", "101", "100", "100.5", 2);
        let far_confirmation = five_minute_bar("101.5", "102.5", "101.5", "102", 3);
        let near_confirmation = five_minute_bar("101.2", "101.5", "101", "101.4", 4);
        let mut zones =
            VecDeque::from([Zone::from_displacement(ZoneKind::Demand, source, 2.0, 1.5)]);

        let _ = observe_zones(
            &mut zones,
            touch,
            Confirmation {
                extreme: true,
                reentry: false,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        let far = observe_zones(
            &mut zones,
            far_confirmation,
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        let near = observe_zones(
            &mut zones,
            near_confirmation,
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Buy,
            rules,
        );

        assert!(far.is_none());
        assert!(near.is_some_and(|signal| (signal.distance_atr - 0.2).abs() < 1e-9),);
    }

    #[rstest::rstest]
    fn test_broken_supply_reclaims_and_confirms_once() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let broken = five_minute_bar("100", "103", "100", "102", 2);
        let still_above = five_minute_bar("102", "104", "101", "103", 3);
        let reclaimed = five_minute_bar("102", "102", "98", "98.5", 4);
        let retest = five_minute_bar("98", "100", "97", "99.5", 5);
        let confirmed = five_minute_bar("99", "100", "97", "98.5", 6);
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
            level: SignalLevel::Fresh,
            entry: Price::from("100.00"),
            zone_low: Price::from("98.00"),
            zone_high: Price::from("99.00"),
            level_age_bars: 1,
            confirmation_bars: 0,
            distance_atr: 0.0,
            zone_width_atr: 0.5,
            displacement_strength_atr: 1.0,
            ts_event: UnixNanos::from(1),
        };
        let (stop, entry_limit) = entry_prices(signal, Price::from("0.01"), 2, 1, 5).unwrap();
        let quantity = risk_sized_quantity(
            entry_limit,
            stop,
            Decimal::from(25),
            Quantity::from(20),
            Decimal::from(5_000),
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
    fn test_risk_sizing_reduces_quantity_to_the_notional_cap() {
        let quantity = risk_sized_quantity(
            Price::from("500.00"),
            Price::from("499.50"),
            Decimal::from(25),
            Quantity::from(50),
            Decimal::from(5_000),
            Quantity::from(1),
        )
        .unwrap();

        assert_eq!(quantity, Some(Quantity::from(10)));
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
    fn test_bar_target_fallback_ignores_prices_before_first_fill() {
        let target = Price::from("100.00");
        let first_fill_ts = UnixNanos::from(100);
        let fill_bar = five_minute_bar("99.00", "101.00", "98.00", "100.00", 90);
        let later_bar = five_minute_bar("99.00", "100.00", "99.00", "100.00", 101);

        assert!(!bar_reaches_target(
            OrderSide::Buy,
            target,
            first_fill_ts,
            fill_bar,
        ));
        assert!(bar_reaches_target(
            OrderSide::Buy,
            target,
            first_fill_ts,
            later_bar,
        ));
        assert!(!bar_reaches_target(
            OrderSide::Sell,
            target,
            first_fill_ts,
            fill_bar,
        ));
        assert!(bar_reaches_target(
            OrderSide::Sell,
            target,
            first_fill_ts,
            later_bar,
        ));
    }

    #[rstest::rstest]
    fn test_bar_reports_ambiguous_exit_when_stop_and_target_are_both_reached() {
        let bar = five_minute_bar("100.00", "102.00", "98.00", "101.00", 1);

        assert!(bar_reaches_stop_and_target(
            Price::from("99.00"),
            Price::from("101.50"),
            bar,
        ));
        assert!(!bar_reaches_stop_and_target(
            Price::from("97.00"),
            Price::from("101.50"),
            bar,
        ));
    }

    #[rstest::rstest]
    fn test_confirmation_distance_is_normalized_by_atr() {
        let zone = Zone::from_bar(
            ZoneKind::Demand,
            five_minute_bar("99.00", "101.00", "98.00", "100.00", 1),
        );

        assert_eq!(zone_distance_atr(zone, Price::from("101.50"), 2.0), 0.25,);
        assert_eq!(zone_distance_atr(zone, Price::from("100.00"), 2.0), 0.0);
    }

    #[rstest::rstest]
    fn test_time_stop_requires_stale_exposure_without_sufficient_mfe() {
        assert!(should_request_time_stop(
            9,
            Decimal::new(4, 1),
            9,
            Decimal::new(5, 1),
            true,
            false,
        ));
        assert!(!should_request_time_stop(
            9,
            Decimal::new(5, 1),
            9,
            Decimal::new(5, 1),
            true,
            false,
        ));
        assert!(!should_request_time_stop(
            8,
            Decimal::new(4, 1),
            9,
            Decimal::new(5, 1),
            true,
            false,
        ));
    }

    #[rstest::rstest]
    fn test_order_notional_reserves_capacity_for_each_position_slot() {
        assert_eq!(
            per_position_notional_limit(Decimal::from(5_000), 2, Decimal::from(5_000)),
            Decimal::from(2_500),
        );
        assert_eq!(
            per_position_notional_limit(Decimal::from(10_000), 2, Decimal::from(2_000)),
            Decimal::from(2_000),
        );
    }

    #[rstest::rstest]
    fn test_risk_reward_grid_is_sorted_and_deduplicated() {
        assert_eq!(
            parse_decimal_grid("2,1.5,1.75,2").unwrap(),
            vec![Decimal::new(15, 1), Decimal::new(175, 2), Decimal::from(2),],
        );
    }

    #[rstest::rstest]
    fn test_annualized_sharpe_requires_variance_and_preserves_sign() {
        assert_eq!(annualized_sharpe(&[0.0, 0.0]), None);
        assert!(annualized_sharpe(&[0.01, 0.02, 0.0]).is_some_and(|value| value > 0.0));
        assert!(annualized_sharpe(&[0.01, -0.02, -0.01]).is_some_and(|value| value < 0.0),);
    }

    #[rstest::rstest]
    fn test_walk_forward_verdict_rejects_loss_and_large_degradation() {
        assert_eq!(walk_forward_verdict(1.0, 0.6), "acceptable");
        assert_eq!(walk_forward_verdict(1.0, 0.4), "possible_overfit");
        assert_eq!(walk_forward_verdict(1.0, -0.1), "reject_non_positive_oos");
        assert_eq!(walk_forward_verdict(-0.1, 0.2), "reject_non_positive_is");
    }

    #[rstest::rstest]
    fn test_walk_forward_selects_only_the_best_is_sharpe() {
        let evaluations = [
            BacktestEvaluation {
                risk_reward: Decimal::new(15, 1),
                trades: 20,
                conservative_pnl: Decimal::from(10),
                conservative_cost_adjusted_sharpe: Some(0.8),
                engine_sharpe: Some(1.0),
            },
            BacktestEvaluation {
                risk_reward: Decimal::from(2),
                trades: 20,
                conservative_pnl: Decimal::from(50),
                conservative_cost_adjusted_sharpe: Some(0.6),
                engine_sharpe: Some(2.0),
            },
        ];

        assert_eq!(
            select_in_sample_winner(&evaluations).unwrap().risk_reward,
            Decimal::new(15, 1),
        );
    }

    #[rstest::rstest]
    fn test_walk_forward_warmup_excludes_split_and_later_bars() {
        let bar_type = AppConfig::five_minute_bar_type(InstrumentId::from("QQQ.US.LONGBRIDGE"));
        let initial = vec![
            five_minute_bar("99", "101", "98", "100", 1),
            five_minute_bar("100", "102", "99", "101", 2),
        ];
        let replay = vec![
            five_minute_bar("101", "103", "100", "102", 3),
            five_minute_bar("102", "104", "101", "103", 4),
            five_minute_bar("103", "105", "102", "104", 5),
        ];

        let warmup = split_warmup_bars(
            &initial,
            &replay,
            UnixNanos::from(5),
            3,
            InstrumentId::from("QQQ.US.LONGBRIDGE"),
            bar_type,
        )
        .unwrap();

        assert_eq!(
            warmup
                .iter()
                .map(|bar| bar.ts_event.as_u64())
                .collect::<Vec<_>>(),
            vec![2, 3, 4],
        );
    }

    #[rstest::rstest]
    fn test_run_statistics_report_exit_and_signal_cohorts() {
        let instrument_id = InstrumentId::from("QQQ.US.LONGBRIDGE");
        let trade = ClosedTradeStatistics {
            side: OrderSide::Buy,
            level: SignalLevel::Reclaimed,
            level_age_bars: 12,
            confirmation_bars: 2,
            distance_atr: 0.2,
            zone_width_atr: 0.5,
            displacement_strength_atr: 1.5,
            entry_minute: 600,
            holding_bars: 8,
            mfe_r: Decimal::from(2),
            mae_r: Decimal::new(5, 1),
            exit_reason: TradeExitReason::Target,
            realized_pnl: Some(Decimal::from(40)),
            estimated_cost: Decimal::from(1),
            initial_risk: Decimal::from(20),
            risk_utilization: Decimal::new(8, 1),
            r_multiple: Some(Decimal::from(2)),
            close_ts: UnixNanos::from(1),
            ambiguous_exit_bar: false,
        };
        let mut ambiguous = trade;
        ambiguous.ambiguous_exit_bar = true;
        assert_eq!(ambiguous.conservative_pnl(), Some(Decimal::from(-21)));
        let statistics = RunStatistics {
            symbols: HashMap::from([(
                instrument_id,
                SymbolRunStatistics {
                    funnel: SignalFunnel {
                        signals: 1,
                        ..Default::default()
                    },
                    entries_submitted: 1,
                    risk_rejections: BTreeMap::new(),
                    trades: vec![trade],
                },
            )]),
        };
        let output = statistics.lines().join("\n");

        assert!(output.contains("exit_reason=target"));
        assert!(output.contains("side=BUY, level=reclaimed"));
        assert!(output.contains("realized_pnl=40"));
        assert!(output.contains("cost_adjusted_pnl=39"));
        assert!(output.contains("average_r=2"));
        assert!(output.contains("average_mfe_r=2"));
        assert!(output.contains("bucket=10:00"));
    }

    #[rstest::rstest]
    fn test_account_risk_rejection_reports_the_binding_limit() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-rejection-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()).as_u64(),
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

        risk.reserve_entry("QQQ.US", date, reservation, 1, limits)
            .unwrap();
        let rejected = risk
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();

        assert_eq!(
            rejected.0,
            ReservationOutcome::Rejected(RiskRejectionReason::OpenPositions),
        );
        let _ = fs::remove_file(path);
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
            ReservationOutcome::Rejected(RiskRejectionReason::OpenPositions),
        );
        risk.record_close("QQQ.US", date, Some(Decimal::from(-50)), true)
            .unwrap();
        drop(risk);
        let restored = AccountRisk::load(path.clone()).unwrap();
        let (outcome, snapshot) = restored
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();

        assert_eq!(
            outcome,
            ReservationOutcome::Rejected(RiskRejectionReason::DailyLoss),
        );
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
        assert_eq!(
            same_day.0,
            ReservationOutcome::Rejected(RiskRejectionReason::AccountHalted),
        );
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
