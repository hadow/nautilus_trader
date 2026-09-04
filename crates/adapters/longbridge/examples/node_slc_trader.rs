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

//! Runs a bar-only Structure-Level-Confirmation strategy with Longbridge stocks.
//!
//! WARNING: This example submits orders. It defaults to Longbridge paper trading. Setting
//! `LONGBRIDGE_SLC_PAPERTRADING=false` routes orders to a live margin account and additionally
//! requires `LONGBRIDGE_SLC_LIVE_ACK=I_UNDERSTAND_LIVE_ORDERS`.
//!
//! The strategy trades completed five-minute bars during the US regular session. It combines
//! confirmed four-hour higher-high/higher-low or lower-high/lower-low structure, fresh supply or
//! demand zones formed before ATR-sized displacement candles, and Stochastics(5,3,3) re-entry from
//! the 20/80 bands. Each market entry has a signal-close-based 2R target and a
//! Longbridge-compatible market-if-touched stop. The node cancels orders and closes positions
//! before the regular session ends.
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
//! Configure one or more US equities with `LONGBRIDGE_SLC_INSTRUMENT_IDS`, separated by commas,
//! for example `QQQ.US.LONGBRIDGE,AAPL.US.LONGBRIDGE,MSFT.US.LONGBRIDGE`. The legacy
//! `LONGBRIDGE_SLC_INSTRUMENT_ID` remains supported for a single instrument.

use std::{
    collections::{HashMap, VecDeque},
    env,
    fmt::{Debug, Display},
    str::FromStr,
    time::Duration,
};

use anyhow::Context;
use jiff::{Timestamp, civil::Time as CivilTime, tz::TimeZone};
use longbridge::{
    Market,
    quote::{AdjustType, Period, QuoteContext, TradeSession, TradeSessions},
};
use nautilus_common::{actor::DataActor, enums::Environment, live::get_runtime};
use nautilus_core::{UnixNanos, datetime::get_timezone};
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
    common::parse::{parse_bar, parse_instrument},
};
use nautilus_model::{
    data::{Bar, BarType},
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
const MAX_WARMUP_BARS: usize = 1_000;
const LIVE_ACK: &str = "I_UNDERSTAND_LIVE_ORDERS";

#[derive(Clone, Copy, Debug)]
struct SessionRules {
    entry_start_minute: u16,
    entry_end_minute: u16,
    flatten_before_close_minutes: u16,
    max_trades_per_day: usize,
}

impl SessionRules {
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
    instrument_ids: Vec<InstrumentId>,
    price_increment: Price,
    papertrading: bool,
    risk_amount: Decimal,
    daily_loss_limit: Decimal,
    max_order_quantity: Quantity,
    max_order_notional: Decimal,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
    atr_period: usize,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    pivot_span: usize,
    zone_ttl_bars: usize,
    confirmation_window_bars: usize,
    oversold: f64,
    overbought: f64,
    five_minute_warmup: usize,
    four_hour_warmup: usize,
    timezone: TimeZone,
    session: SessionRules,
}

impl AppConfig {
    fn from_env() -> anyhow::Result<Self> {
        let papertrading = env_parse("LONGBRIDGE_SLC_PAPERTRADING", "true")?;
        validate_live_guard(
            papertrading,
            env::var("LONGBRIDGE_SLC_LIVE_ACK").ok().as_deref(),
        )?;
        validate_realtime_candlesticks()?;

        let instrument_ids = env_instrument_ids()?;

        let price_increment = env_parse("LONGBRIDGE_SLC_PRICE_INCREMENT", "0.01")?;
        anyhow::ensure!(
            Price::is_positive(&price_increment),
            "LONGBRIDGE_SLC_PRICE_INCREMENT must be positive",
        );
        let risk_amount = env_parse("LONGBRIDGE_SLC_RISK_AMOUNT", "25")?;
        let daily_loss_limit = env_parse("LONGBRIDGE_SLC_DAILY_LOSS_LIMIT", "50")?;
        let max_order_quantity = env_parse("LONGBRIDGE_SLC_MAX_ORDER_QUANTITY", "5")?;
        let max_order_notional = env_parse("LONGBRIDGE_SLC_MAX_ORDER_NOTIONAL", "5000")?;
        let risk_reward = env_parse("LONGBRIDGE_SLC_RISK_REWARD", "2")?;
        anyhow::ensure!(risk_amount > Decimal::ZERO, "risk amount must be positive");
        anyhow::ensure!(
            daily_loss_limit > Decimal::ZERO,
            "daily loss limit must be positive",
        );
        anyhow::ensure!(
            Quantity::is_positive(&max_order_quantity),
            "maximum order quantity must be positive",
        );
        anyhow::ensure!(
            max_order_notional > Decimal::ZERO,
            "maximum order notional must be positive",
        );
        anyhow::ensure!(risk_reward > Decimal::ZERO, "risk reward must be positive");

        let atr_period = env_parse("LONGBRIDGE_SLC_ATR_PERIOD", "14")?;
        let displacement_atr_multiple = env_parse("LONGBRIDGE_SLC_DISPLACEMENT_ATR", "1.5")?;
        let displacement_close_fraction =
            env_parse("LONGBRIDGE_SLC_DISPLACEMENT_CLOSE_FRACTION", "0.25")?;
        let pivot_span = env_parse("LONGBRIDGE_SLC_PIVOT_SPAN", "2")?;
        let zone_ttl_bars = env_parse("LONGBRIDGE_SLC_ZONE_TTL_BARS", "78")?;
        let confirmation_window_bars = env_parse("LONGBRIDGE_SLC_CONFIRMATION_WINDOW_BARS", "1")?;
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
        anyhow::ensure!(pivot_span > 0, "pivot span must be positive");
        anyhow::ensure!(zone_ttl_bars > 0, "zone TTL must be positive");
        anyhow::ensure!(
            0.0 < oversold && oversold < overbought && overbought < 100.0,
            "stochastic thresholds must satisfy 0 < oversold < overbought < 100",
        );

        let five_minute_warmup = env_parse("LONGBRIDGE_SLC_5M_WARMUP", "500")?;
        let four_hour_warmup = env_parse("LONGBRIDGE_SLC_4H_WARMUP", "100")?;
        anyhow::ensure!(
            five_minute_warmup > atr_period && five_minute_warmup <= MAX_WARMUP_BARS,
            "5-minute warmup must exceed ATR period and not exceed {MAX_WARMUP_BARS}",
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

        Ok(Self {
            instrument_ids,
            price_increment,
            papertrading,
            risk_amount,
            daily_loss_limit,
            max_order_quantity,
            max_order_notional,
            risk_reward,
            stop_buffer_ticks: env_parse("LONGBRIDGE_SLC_STOP_BUFFER_TICKS", "1")?,
            atr_period,
            displacement_atr_multiple,
            displacement_close_fraction,
            pivot_span,
            zone_ttl_bars,
            confirmation_window_bars,
            oversold,
            overbought,
            five_minute_warmup,
            four_hour_warmup,
            timezone: get_timezone(US_TIMEZONE)?,
            session,
        })
    }

    fn five_minute_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{}-5-MINUTE-LAST-EXTERNAL", instrument_id).as_str())
    }

    fn four_hour_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{}-4-HOUR-LAST-EXTERNAL", instrument_id).as_str())
    }

    fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            instrument_price_increments: self
                .instrument_ids
                .iter()
                .map(|instrument_id| {
                    (instrument_id.to_string(), self.price_increment.to_string())
                })
                .collect(),
            ..Default::default()
        }
    }
}

fn env_string(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

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

fn parse_instrument_ids(value: &str) -> anyhow::Result<Vec<InstrumentId>> {
    let mut instrument_ids = Vec::new();
    for raw in value.split(',').map(str::trim).filter(|value| !value.is_empty()) {
        let instrument_id: InstrumentId = raw
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid Longbridge instrument {raw:?}: {e}"))?;
        anyhow::ensure!(
            instrument_id.venue.as_str() == "LONGBRIDGE"
                && instrument_id.symbol.as_str().ends_with(".US"),
            "SLC live example currently requires US equities on venue LONGBRIDGE: {instrument_id}",
        );
        anyhow::ensure!(
            !instrument_ids.contains(&instrument_id),
            "duplicate Longbridge instrument configured: {instrument_id}",
        );
        instrument_ids.push(instrument_id);
    }

    anyhow::ensure!(
        !instrument_ids.is_empty(),
        "LONGBRIDGE_SLC_INSTRUMENT_IDS must contain at least one instrument",
    );
    anyhow::ensure!(
        instrument_ids.len() <= 500,
        "LONGBRIDGE_SLC_INSTRUMENT_IDS supports at most 500 instruments",
    );
    Ok(instrument_ids)
}

fn env_instrument_ids() -> anyhow::Result<Vec<InstrumentId>> {
    let value = env::var("LONGBRIDGE_SLC_INSTRUMENT_IDS")
        .or_else(|_| env::var("LONGBRIDGE_SLC_INSTRUMENT_ID"))
        .unwrap_or_else(|_| "QQQ.US.LONGBRIDGE".to_string());
    parse_instrument_ids(&value)
}

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
    fn new(span: usize) -> Self {
        Self {
            span,
            window: VecDeque::with_capacity(span * 2 + 1),
            highs: VecDeque::with_capacity(2),
            lows: VecDeque::with_capacity(2),
        }
    }

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

    fn trend(&self) -> Trend {
        if self.highs.len() < 2 || self.lows.len() < 2 {
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
struct Zone {
    kind: ZoneKind,
    low: Price,
    high: Price,
    age: usize,
    touched: bool,
    confirmation_bars_left: usize,
}

impl Zone {
    fn from_bar(kind: ZoneKind, bar: Bar) -> Self {
        Self {
            kind,
            low: bar.low,
            high: bar.high,
            age: 0,
            touched: false,
            confirmation_bars_left: 0,
        }
    }

    fn intersects(self, bar: Bar) -> bool {
        bar.low <= self.high && bar.high >= self.low
    }

    fn invalidated(self, bar: Bar) -> bool {
        match self.kind {
            ZoneKind::Demand => bar.close < self.low,
            ZoneKind::Supply => bar.close > self.high,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Signal {
    side: OrderSide,
    entry: Price,
    zone_low: Price,
    zone_high: Price,
}

#[derive(Debug, Default)]
struct FinalBarBuffer {
    pending: Option<Bar>,
}

impl FinalBarBuffer {
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
    demand: Option<Zone>,
    supply: Option<Zone>,
    rules: SignalRules,
}

impl SlcSignalState {
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
                5,
                3,
                3,
                MovingAverageType::Simple,
                StochasticsDMethod::MovingAverage,
            ),
            previous_five_minute_bar: None,
            previous_k: None,
            demand: None,
            supply: None,
            rules: SignalRules {
                zone_ttl_bars: config.zone_ttl_bars,
                confirmation_window_bars: config.confirmation_window_bars,
                displacement_atr_multiple: config.displacement_atr_multiple,
                displacement_close_fraction: config.displacement_close_fraction,
                oversold: config.oversold,
                overbought: config.overbought,
            },
        }
    }

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

    fn finalize_four_hour(&mut self, bar: Bar) -> Option<Bar> {
        self.four_hour_bars.update(bar)
    }

    fn process_four_hour(&mut self, bar: Bar) {
        self.structure.update(bar);
    }

    fn finalize_five_minute(&mut self, bar: Bar) -> Option<Bar> {
        self.five_minute_bars.update(bar)
    }

    fn process_five_minute(&mut self, bar: Bar, allow_signal: bool) -> Option<Signal> {
        let atr_before = self.atr.value;
        let atr_initialized = self.atr.initialized();
        self.stochastics.handle_bar(&bar);
        let current_k = self.stochastics.value_k;
        let long_cross = self.stochastics.initialized()
            && self
                .previous_k
                .is_some_and(|previous| previous <= self.rules.oversold)
            && current_k > self.rules.oversold;
        let short_cross = self.stochastics.initialized()
            && self
                .previous_k
                .is_some_and(|previous| previous >= self.rules.overbought)
            && current_k < self.rules.overbought;

        let trend = self.structure.trend();
        let long_signal = observe_zone(
            &mut self.demand,
            bar,
            trend == Trend::Up && long_cross && allow_signal,
            OrderSide::Buy,
            self.rules,
        );
        let short_signal = observe_zone(
            &mut self.supply,
            bar,
            trend == Trend::Down && short_cross && allow_signal,
            OrderSide::Sell,
            self.rules,
        );

        if atr_initialized && let Some(previous) = self.previous_five_minute_bar {
            if is_up_displacement(previous, bar, atr_before, self.rules) {
                self.demand = Some(Zone::from_bar(ZoneKind::Demand, previous));
            } else if is_down_displacement(previous, bar, atr_before, self.rules) {
                self.supply = Some(Zone::from_bar(ZoneKind::Supply, previous));
            }
        }

        self.atr.handle_bar(&bar);
        self.previous_five_minute_bar = Some(bar);
        self.previous_k = Some(current_k);
        long_signal.or(short_signal)
    }
}

fn observe_zone(
    zone: &mut Option<Zone>,
    bar: Bar,
    confirmed: bool,
    side: OrderSide,
    rules: SignalRules,
) -> Option<Signal> {
    let active = zone.as_mut()?;
    active.age += 1;
    if active.age > rules.zone_ttl_bars || active.invalidated(bar) {
        *zone = None;
        return None;
    }
    if !active.touched && active.intersects(bar) {
        active.touched = true;
        active.confirmation_bars_left = rules.confirmation_window_bars + 1;
    }
    if !active.touched {
        return None;
    }
    if confirmed {
        let signal = Signal {
            side,
            entry: bar.close,
            zone_low: active.low,
            zone_high: active.high,
        };
        *zone = None;
        return Some(signal);
    }

    active.confirmation_bars_left -= 1;
    if active.confirmation_bars_left == 0 {
        *zone = None;
    }
    None
}

fn is_up_displacement(previous: Bar, current: Bar, atr: f64, rules: SignalRules) -> bool {
    previous.close < previous.open
        && current.close > current.open
        && displacement_body(current) >= atr * rules.displacement_atr_multiple
        && close_fraction_from_high(current) <= rules.displacement_close_fraction
}

fn is_down_displacement(previous: Bar, current: Bar, atr: f64, rules: SignalRules) -> bool {
    previous.close > previous.open
        && current.close < current.open
        && displacement_body(current) >= atr * rules.displacement_atr_multiple
        && close_fraction_from_low(current) <= rules.displacement_close_fraction
}

fn displacement_body(bar: Bar) -> f64 {
    (bar.close.as_f64() - bar.open.as_f64()).abs()
}

fn close_fraction_from_high(bar: Bar) -> f64 {
    let range = bar.high.as_f64() - bar.low.as_f64();
    if range == 0.0 {
        return 1.0;
    }
    (bar.high.as_f64() - bar.close.as_f64()) / range
}

fn close_fraction_from_low(bar: Bar) -> f64 {
    let range = bar.high.as_f64() - bar.low.as_f64();
    if range == 0.0 {
        return 1.0;
    }
    (bar.close.as_f64() - bar.low.as_f64()) / range
}

#[derive(Debug, Default)]
struct DailyRiskState {
    date: Option<jiff::civil::Date>,
    trades: usize,
    realized_pnl: Decimal,
    disabled: bool,
}

impl DailyRiskState {
    fn roll_date(&mut self, date: jiff::civil::Date) -> bool {
        if self.date == Some(date) {
            return false;
        }
        self.date = Some(date);
        self.trades = 0;
        self.realized_pnl = Decimal::ZERO;
        self.disabled = false;
        true
    }

    fn record_trade(&mut self) {
        self.trades += 1;
    }

    fn record_pnl(&mut self, pnl: Decimal, daily_loss_limit: Decimal) {
        self.realized_pnl += pnl;
        if self.realized_pnl <= -daily_loss_limit {
            self.disabled = true;
        }
    }
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
    daily_loss_limit: Decimal,
    max_order_quantity: Quantity,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingEntry {
    client_order_id: ClientOrderId,
    side: OrderSide,
    stop: Price,
    target: Price,
}

#[derive(Debug)]
struct ActiveTrade {
    side: OrderSide,
    target: Price,
}

struct SlcStrategy {
    core: StrategyCore,
    config: SlcStrategyConfig,
    instrument: InstrumentAny,
    signals: SlcSignalState,
    daily: DailyRiskState,
    pending_entry: Option<PendingEntry>,
    active_trade: Option<ActiveTrade>,
    exit_pending: bool,
    faulted: bool,
}

impl SlcStrategy {
    fn new(
        app_config: &AppConfig,
        instrument_id: InstrumentId,
        instrument: InstrumentAny,
        five_minute_bars: Vec<Bar>,
        four_hour_bars: Vec<Bar>,
        flatten_minute: u16,
    ) -> Self {
        let mut signals = SlcSignalState::new(app_config);
        signals.warm_up(five_minute_bars, four_hour_bars);
        Self {
            core: StrategyCore::new(StrategyConfig {
                strategy_id: Some(StrategyId::from(
                    format!("{STRATEGY_ID}-{}", instrument_id.symbol).as_str(),
                )),
                order_id_tag: Some(ORDER_ID_TAG.to_string()),
                external_order_claims: Some(vec![instrument_id]),
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
                daily_loss_limit: app_config.daily_loss_limit,
                max_order_quantity: app_config.max_order_quantity,
                risk_reward: app_config.risk_reward,
                stop_buffer_ticks: app_config.stop_buffer_ticks,
            },
            instrument,
            signals,
            daily: DailyRiskState::default(),
            pending_entry: None,
            active_trade: None,
            exit_pending: false,
            faulted: false,
        }
    }

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

    fn flatten(&mut self) -> anyhow::Result<()> {
        if !self.has_exposure() {
            return Ok(());
        }
        self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        self.close_positions()
    }

    fn disable_after_order_failure(&mut self, reason: &str) {
        self.faulted = true;
        log::error!("SLC trading disabled after order failure: {reason}");
        if let Err(e) = self.market_exit() {
            log::error!("Failed to flatten SLC exposure after order failure: {e:#}");
        }
    }

    fn submit_signal(&mut self, signal: Signal) -> anyhow::Result<()> {
        let (stop, target) = bracket_prices(
            signal,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.stop_buffer_ticks,
            self.config.risk_reward,
        )?;
        let lot_size = self
            .instrument
            .lot_size()
            .unwrap_or_else(|| Quantity::from(1));
        let Some(quantity) = risk_sized_quantity(
            signal.entry,
            stop,
            self.config.risk_amount,
            self.config.max_order_quantity,
            lot_size,
        )?
        else {
            log::warn!("Skipping SLC signal because risk sizing produced zero quantity");
            return Ok(());
        };

        let order = self.order().market(
            self.config.instrument_id,
            signal.side,
            quantity,
            Some(TimeInForce::Day),
            Some(false),
            Some(false),
            None,
            None,
            Some(vec![Ustr::from("SLC_ENTRY")]),
            None,
        );
        let client_order_id = order.client_order_id();
        self.submit_order(order, None, None, None)?;
        self.pending_entry = Some(PendingEntry {
            client_order_id,
            side: signal.side,
            stop,
            target,
        });
        self.daily.record_trade();
        log::info!(
            "Submitted SLC {} entry: quantity={}, signal_close={}, stop={}, target={}",
            signal.side,
            quantity,
            signal.entry,
            stop,
            target,
        );
        Ok(())
    }

    fn protect_entry_fill(&mut self, event: &OrderFilled) -> anyhow::Result<()> {
        let Some(pending) = self.pending_entry else {
            return Ok(());
        };
        if event.client_order_id != pending.client_order_id {
            return Ok(());
        }
        let exit_side = match pending.side {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
            OrderSide::NoOrderSide => anyhow::bail!("entry side is unspecified"),
        };
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
        self.active_trade.get_or_insert(ActiveTrade {
            side: pending.side,
            target: pending.target,
        });

        let entry_closed = self
            .cache()
            .order(&pending.client_order_id)
            .is_some_and(|order| order.is_closed());
        if entry_closed {
            self.pending_entry = None;
        }
        log::info!(
            "Protected SLC entry fill: quantity={}, stop={}, target={}",
            event.last_qty,
            pending.stop,
            pending.target,
        );
        if self.exit_pending {
            self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        }
        Ok(())
    }

    fn target_reached(&self, bar: Bar) -> bool {
        !self.exit_pending
            && self
                .active_trade
                .as_ref()
                .is_some_and(|active| match active.side {
                    OrderSide::Buy => bar.close >= active.target,
                    OrderSide::Sell => bar.close <= active.target,
                    OrderSide::NoOrderSide => false,
                })
    }

    fn request_exit(&mut self, reason: &str) -> anyhow::Result<()> {
        self.exit_pending = true;
        log::info!("Requesting SLC position exit: {reason}");
        if self.has_open_orders() {
            self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        } else {
            self.finish_exit()?;
        }
        Ok(())
    }

    fn finish_exit(&mut self) -> anyhow::Result<()> {
        if !self.exit_pending || self.has_open_orders() {
            return Ok(());
        }
        if !self.has_open_position() {
            self.exit_pending = false;
            return Ok(());
        }
        log::info!("All SLC stop orders canceled; submitting market exit");
        self.close_positions()
    }
}

impl Debug for SlcStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(SlcStrategy))
            .field("config", &self.config)
            .field("daily", &self.daily)
            .field("faulted", &self.faulted)
            .finish_non_exhaustive()
    }
}

nautilus_strategy!(SlcStrategy, {
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

    fn on_order_rejected(&mut self, event: OrderRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.disable_after_order_failure(&event.reason);
        }
    }

    fn on_order_denied(&mut self, event: OrderDenied) {
        if event.instrument_id == self.config.instrument_id {
            self.disable_after_order_failure(&event.reason);
        }
    }

    fn on_order_expired(&mut self, event: OrderExpired) {
        if event.instrument_id == self.config.instrument_id && self.has_exposure() {
            self.disable_after_order_failure("a live SLC order expired while exposure remained");
        }
    }

    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        if self.exit_pending {
            if let Err(e) = self.finish_exit() {
                self.disable_after_order_failure(&format!("failed to submit SLC exit: {e:#}"));
            }
        } else if !self.is_exiting() && self.has_open_position() {
            self.disable_after_order_failure("a protective SLC stop was canceled unexpectedly");
        }
    }

    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.faulted = true;
            log::error!(
                "SLC stop cancellation was rejected; keeping the protective stop active and disabling new entries: {}",
                event.reason,
            );
        }
    }

    fn on_position_closed(&mut self, event: PositionClosed) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        self.pending_entry = None;
        self.active_trade = None;
        self.exit_pending = false;
        if let Err(e) = self.cancel_all_orders(self.config.instrument_id, None, None, None) {
            log::error!("Failed to cancel residual SLC orders after position close: {e:#}");
            self.faulted = true;
        }
        let local_date = event
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone())
            .date();
        self.daily.roll_date(local_date);
        if let Some(pnl) = event.realized_pnl {
            self.daily
                .record_pnl(pnl.as_decimal(), self.config.daily_loss_limit);
        }
    }
});

impl DataActor for SlcStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        validate_bar_type(self.config.five_minute_bar_type, 5, BarAggregation::Minute)?;
        validate_bar_type(self.config.four_hour_bar_type, 4, BarAggregation::Hour)?;
        anyhow::ensure!(
            self.config.entry_start_minute < self.config.entry_end_minute,
            "effective entry window ends before it starts on this trading day",
        );
        self.cache().try_instrument(&self.config.instrument_id)?;
        self.subscribe_bars(self.config.five_minute_bar_type, None, None);
        self.subscribe_bars(self.config.four_hour_bar_type, None, None);
        if self.has_exposure() {
            log::warn!("Flattening reconciled SLC exposure before accepting new signals");
            self.request_exit("reconciled startup exposure")?;
        }
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.flatten()?;
        self.unsubscribe_bars(self.config.five_minute_bar_type, None, None);
        self.unsubscribe_bars(self.config.four_hour_bar_type, None, None);
        Ok(())
    }

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
            }
            return Ok(());
        }
        if bar.bar_type != self.config.five_minute_bar_type {
            return Ok(());
        }
        let Some(finalized) = self.signals.finalize_five_minute(*bar) else {
            return Ok(());
        };
        let local = finalized
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
        let close_minute = minute.saturating_add(FIVE_MINUTES);
        if !(RTH_OPEN_MINUTE..RTH_CLOSE_MINUTE).contains(&minute) {
            return Ok(());
        }
        let signal = self.signals.process_five_minute(finalized, true);
        if self.daily.roll_date(local.date()) && self.has_exposure() {
            self.request_exit("unexpected overnight exposure")?;
        }
        if close_minute >= self.config.flatten_minute {
            self.daily.disabled = true;
            self.request_exit("pre-close risk cutoff")?;
            return Ok(());
        }
        if self.target_reached(finalized) {
            self.request_exit("five-minute bar closed beyond the 2R target")?;
            return Ok(());
        }
        if self.faulted
            || self.exit_pending
            || self.daily.disabled
            || self.daily.trades >= self.config.max_trades_per_day
            || close_minute < self.config.entry_start_minute
            || close_minute > self.config.entry_end_minute
            || self.has_exposure()
        {
            return Ok(());
        }
        if let Some(signal) = signal
            && let Err(e) = self.submit_signal(signal)
        {
            self.faulted = true;
            return Err(e);
        }
        Ok(())
    }
}

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

fn bracket_prices(
    signal: Signal,
    increment: Price,
    precision: u8,
    stop_buffer_ticks: u64,
    risk_reward: Decimal,
) -> anyhow::Result<(Price, Price)> {
    let entry = signal.entry.as_decimal();
    let increment = increment.as_decimal();
    let buffer = increment * Decimal::from(stop_buffer_ticks);
    let (stop, target) = match signal.side {
        OrderSide::Buy => {
            let stop = signal.zone_low.as_decimal() - buffer;
            let risk = entry - stop;
            anyhow::ensure!(risk > Decimal::ZERO, "long stop must be below signal close");
            let target = ((entry + risk * risk_reward) / increment).floor() * increment;
            (stop, target)
        }
        OrderSide::Sell => {
            let stop = signal.zone_high.as_decimal() + buffer;
            let risk = stop - entry;
            anyhow::ensure!(
                risk > Decimal::ZERO,
                "short stop must be above signal close"
            );
            let target = ((entry - risk * risk_reward) / increment).ceil() * increment;
            (stop, target)
        }
        OrderSide::NoOrderSide => anyhow::bail!("signal side is unspecified"),
    };
    anyhow::ensure!(stop > Decimal::ZERO, "stop price must be positive");
    anyhow::ensure!(target > Decimal::ZERO, "target price must be positive");
    Ok((
        Price::from_decimal_dp(stop, precision)?,
        Price::from_decimal_dp(target, precision)?,
    ))
}

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

async fn prepare_inputs(
    config: &AppConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<PreparedInputs>> {
    let symbols = config
        .instrument_ids
        .iter()
        .map(|instrument_id| instrument_id.symbol.as_str())
        .collect::<Vec<_>>();
    let static_info = context
        .static_info(symbols.clone())
        .await
        .context("failed to request Longbridge static security info")?;
    let mut static_info_by_symbol = static_info
        .into_iter()
        .map(|info| (info.symbol.clone(), info))
        .collect::<HashMap<_, _>>();

    let (market_close, market_close_minute) = current_us_market_close(context).await?;
    let mut prepared = Vec::with_capacity(config.instrument_ids.len());

    for instrument_id in &config.instrument_ids {
        let symbol = instrument_id.symbol.as_str();
        let static_security_info = static_info_by_symbol
            .remove(symbol)
            .with_context(|| format!("Longbridge did not return exact static security info for {symbol}"))?;
        let instrument = parse_instrument(
            &static_security_info,
            config.price_increment,
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
            AppConfig::five_minute_bar_type(*instrument_id),
            config.five_minute_warmup,
        )
        .await?;
        let four_hour_bars = load_warmup_bars(
            context,
            symbol,
            Period::FourHour,
            AppConfig::four_hour_bar_type(*instrument_id),
            config.four_hour_warmup,
        )
        .await?;

        prepared.push(PreparedInputs {
            instrument_id: *instrument_id,
            instrument,
            five_minute_bars,
            four_hour_bars,
            market_close,
            market_close_minute,
        });
    }

    Ok(prepared)
}

async fn load_warmup_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    count: usize,
) -> anyhow::Result<Vec<Bar>> {
    let mut bars = context
        .candlesticks(
            symbol,
            period,
            count,
            AdjustType::NoAdjust,
            TradeSessions::Intraday,
        )
        .await
        .with_context(|| format!("failed to request {period:?} warmup bars for {symbol}"))?
        .into_iter()
        .map(|candlestick| parse_bar(bar_type, candlestick, UnixNanos::default()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    anyhow::ensure!(
        bars.len() >= 2,
        "Longbridge returned insufficient {period:?} warmup bars for {symbol}",
    );
    Ok(bars)
}

async fn current_us_market_close(context: &QuoteContext) -> anyhow::Result<(Timestamp, u16)> {
    let now = Timestamp::now();
    let market_date = us_market_date(now)?;
    let trading_days = context
        .trading_days(Market::US, market_date, market_date)
        .await
        .context("failed to query the current US trading day from Longbridge")?;
    if !trading_days.trading_days.contains(&market_date) {
        anyhow::bail!("{market_date} is not a US trading day");
    }

    let close_time = if trading_days.half_trading_days.contains(&market_date) {
        time::macros::time!(13:00)
    } else {
        context
            .trading_session()
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

fn us_market_date(now: Timestamp) -> anyhow::Result<Date> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let local_date = now.to_zoned(timezone).date();
    Ok(Date::from_calendar_date(
        i32::from(local_date.year()),
        Month::try_from(u8::try_from(local_date.month())?)?,
        u8::try_from(local_date.day())?,
    )?)
}

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

fn execution_config(config: &AppConfig) -> LongbridgeExecClientConfig {
    LongbridgeExecClientConfig {
        account_type: AccountType::Margin,
        papertrading: config.papertrading,
        outside_rth: false,
        ..Default::default()
    }
}

fn risk_engine_config(config: &AppConfig) -> LiveRiskEngineConfig {
    LiveRiskEngineConfig {
        bypass: false,
        max_order_submit_rate: "6/00:00:01".to_string(),
        max_order_modify_rate: "6/00:00:01".to_string(),
        max_notional_per_order: config
            .instrument_ids
            .iter()
            .map(|instrument_id| {
                (instrument_id.to_string(), config.max_order_notional.to_string())
            })
            .collect(),
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AppConfig::from_env()?;
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
                .instrument_ids
                .iter()
                .map(ToString::to_string)
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
        ))?;
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

    #[rstest::rstest]
    fn test_parse_multiple_instrument_ids() {
        let instruments = parse_instrument_ids(
            "QQQ.US.LONGBRIDGE, AAPL.US.LONGBRIDGE, MSFT.US.LONGBRIDGE",
        )
        .unwrap();

        assert_eq!(instruments.len(), 3);
        assert_eq!(instruments[0].symbol.as_str(), "QQQ.US");
        assert_eq!(instruments[1].symbol.as_str(), "AAPL.US");
        assert_eq!(instruments[2].symbol.as_str(), "MSFT.US");
    }

    #[rstest::rstest]
    fn test_parse_instrument_ids_rejects_duplicates() {
        assert!(parse_instrument_ids("QQQ.US.LONGBRIDGE,QQQ.US.LONGBRIDGE").is_err());
    }

    #[rstest::rstest]
    fn test_parse_instrument_ids_rejects_non_us_equities() {
        assert!(parse_instrument_ids("0700.HK.LONGBRIDGE").is_err());
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
    fn test_supply_and_demand_displacement_rules() {
        let rules = SignalRules {
            zone_ttl_bars: 78,
            confirmation_window_bars: 1,
            displacement_atr_multiple: 1.5,
            displacement_close_fraction: 0.25,
            oversold: 20.0,
            overbought: 80.0,
        };
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
            zone_ttl_bars: 78,
            confirmation_window_bars: 1,
            displacement_atr_multiple: 1.5,
            displacement_close_fraction: 0.25,
            oversold: 20.0,
            overbought: 80.0,
        };
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch = five_minute_bar("102", "102", "100", "101", 2);
        let later = five_minute_bar("102", "103", "102", "103", 3);
        let mut zone = Some(Zone::from_bar(ZoneKind::Demand, source));

        assert_eq!(
            observe_zone(&mut zone, touch, false, OrderSide::Buy, rules),
            None,
        );
        assert!(zone.is_some());
        assert_eq!(
            observe_zone(&mut zone, later, false, OrderSide::Buy, rules),
            None,
        );
        assert!(zone.is_none());
    }

    #[rstest::rstest]
    fn test_bracket_prices_and_risk_sizing_use_exact_decimals() {
        let signal = Signal {
            side: OrderSide::Buy,
            entry: Price::from("100.00"),
            zone_low: Price::from("98.00"),
            zone_high: Price::from("99.00"),
        };
        let (stop, target) =
            bracket_prices(signal, Price::from("0.01"), 2, 1, Decimal::from(2)).unwrap();
        let quantity = risk_sized_quantity(
            signal.entry,
            stop,
            Decimal::from(25),
            Quantity::from(20),
            Quantity::from(1),
        )
        .unwrap();

        assert_eq!(stop, Price::from("97.99"));
        assert_eq!(target, Price::from("104.02"));
        assert_eq!(quantity, Some(Quantity::from(12)));
    }

    #[rstest::rstest]
    fn test_daily_loss_limit_disables_new_trades() {
        let mut state = DailyRiskState::default();
        state.roll_date("2026-09-01".parse().unwrap());
        state.record_pnl(Decimal::from(-50), Decimal::from(50));

        assert!(state.disabled);
        assert_eq!(state.realized_pnl, Decimal::from(-50));
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
