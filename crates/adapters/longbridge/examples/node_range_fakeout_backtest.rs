// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
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

//! Downloads Longbridge historical 5-minute bars and runs a pure-bar range-fakeout backtest.
//!
//! The default session reproduces the video rules in New York time: build the 00:00-04:00 range,
//! require a 5-minute close outside it, enter on a later close back inside, stop one tick beyond
//! the breakout excursion, and target 2R. Signals are evaluated on completed bars and market
//! brackets execute no earlier than the next bar.
//!
//! Run with:
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-range-fakeout-backtest`

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    fmt::{Debug, Display},
    num::NonZeroUsize,
    rc::Rc,
    str::FromStr,
    time::Duration,
};

use anyhow::Context;
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::{
    actor::{DataActor, DataActorConfig, DataActorCore},
    enums::Environment,
    nautilus_actor,
};
use nautilus_core::datetime::get_timezone;
use nautilus_live::node::LiveNode;
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, common::consts::LONGBRIDGE_CLIENT_ID,
};
use nautilus_model::{
    data::{Bar, BarType, Data},
    enums::{
        AccountType, AggregationSource, BarAggregation, BookType, OmsType, OrderSide, OrderType,
        PriceType,
    },
    identifiers::{InstrumentId, StrategyId, TraderId},
    instruments::{Instrument, InstrumentAny},
    types::{Money, Price, Quantity},
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::Decimal;

const TRADER_ID: &str = "RANGE-FAKEOUT-001";
const NODE_NAME: &str = "LONGBRIDGE-HISTORICAL-5M-001";
const ACTOR_ID: &str = "LONGBRIDGE-HISTORY-001";
const BAR_SPEC: &str = "5-MINUTE-LAST-EXTERNAL";
const FIVE_MINUTES_NS: u64 = 300_000_000_000;
const MAX_HISTORICAL_BARS: usize = 1_000;

#[derive(Clone, Copy, Debug)]
struct SessionRules {
    range_start_minute: u16,
    range_end_minute: u16,
    entry_start_minute: u16,
    entry_end_minute: u16,
    max_trades_per_day: usize,
}

impl SessionRules {
    fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.range_start_minute < self.range_end_minute,
            "range start must be before range end",
        );
        anyhow::ensure!(
            self.range_end_minute <= self.entry_start_minute,
            "entry window must start at or after range end",
        );
        anyhow::ensure!(
            self.entry_start_minute < self.entry_end_minute && self.entry_end_minute <= 24 * 60,
            "entry window must be non-empty and end by 24:00",
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
    instrument_id: InstrumentId,
    price_increment: Price,
    history_start: Timestamp,
    history_end: Timestamp,
    history_limit: NonZeroUsize,
    download_timeout_secs: u64,
    starting_balance: Money,
    trade_size: Quantity,
    timezone: TimeZone,
    rules: SessionRules,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
}

impl AppConfig {
    fn from_env() -> anyhow::Result<Self> {
        let history_limit = env_parse("LONGBRIDGE_BACKTEST_LIMIT", "1000")?;
        anyhow::ensure!(
            history_limit <= MAX_HISTORICAL_BARS,
            "LONGBRIDGE_BACKTEST_LIMIT must not exceed {MAX_HISTORICAL_BARS}",
        );
        let history_limit = NonZeroUsize::new(history_limit)
            .context("LONGBRIDGE_BACKTEST_LIMIT must be positive")?;

        let timezone_name = env_string("LONGBRIDGE_BACKTEST_TIMEZONE", "America/New_York");
        let timezone = get_timezone(&timezone_name)
            .with_context(|| format!("invalid LONGBRIDGE_BACKTEST_TIMEZONE {timezone_name:?}"))?;
        let rules = SessionRules {
            range_start_minute: env_time("LONGBRIDGE_BACKTEST_RANGE_START", "00:00")?,
            range_end_minute: env_time("LONGBRIDGE_BACKTEST_RANGE_END", "04:00")?,
            entry_start_minute: env_time("LONGBRIDGE_BACKTEST_ENTRY_START", "04:00")?,
            entry_end_minute: env_time("LONGBRIDGE_BACKTEST_ENTRY_END", "16:00")?,
            max_trades_per_day: env_parse("LONGBRIDGE_BACKTEST_MAX_TRADES_PER_DAY", "1")?,
        };
        rules.validate()?;

        let history_start = env_parse("LONGBRIDGE_BACKTEST_START", "2026-08-24T00:00:00Z")?;
        let history_end = env_parse("LONGBRIDGE_BACKTEST_END", "2026-08-31T23:59:59Z")?;
        anyhow::ensure!(
            history_start < history_end,
            "LONGBRIDGE_BACKTEST_START must be before LONGBRIDGE_BACKTEST_END",
        );

        let risk_reward = env_parse("LONGBRIDGE_BACKTEST_RISK_REWARD", "2")?;
        anyhow::ensure!(
            risk_reward > Decimal::ZERO,
            "LONGBRIDGE_BACKTEST_RISK_REWARD must be positive",
        );

        let price_increment = env_parse("LONGBRIDGE_BACKTEST_PRICE_INCREMENT", "0.01")?;
        anyhow::ensure!(
            Price::is_positive(&price_increment),
            "LONGBRIDGE_BACKTEST_PRICE_INCREMENT must be positive",
        );
        let download_timeout_secs = env_parse("LONGBRIDGE_BACKTEST_TIMEOUT_SECS", "60")?;
        anyhow::ensure!(
            download_timeout_secs > 0,
            "LONGBRIDGE_BACKTEST_TIMEOUT_SECS must be positive",
        );
        let starting_balance = env_parse("LONGBRIDGE_BACKTEST_STARTING_BALANCE", "100_000 USD")?;
        anyhow::ensure!(
            Money::is_positive(&starting_balance),
            "LONGBRIDGE_BACKTEST_STARTING_BALANCE must be positive",
        );
        let trade_size = env_parse("LONGBRIDGE_BACKTEST_TRADE_SIZE", "10")?;
        anyhow::ensure!(
            Quantity::is_positive(&trade_size),
            "LONGBRIDGE_BACKTEST_TRADE_SIZE must be positive",
        );

        Ok(Self {
            instrument_id: env_parse("LONGBRIDGE_BACKTEST_INSTRUMENT_ID", "AAPL.US.LONGBRIDGE")?,
            price_increment,
            history_start,
            history_end,
            history_limit,
            download_timeout_secs,
            starting_balance,
            trade_size,
            timezone,
            rules,
            risk_reward,
            stop_buffer_ticks: env_parse("LONGBRIDGE_BACKTEST_STOP_BUFFER_TICKS", "1")?,
        })
    }

    fn bar_type(&self) -> BarType {
        BarType::from(format!("{}-{BAR_SPEC}", self.instrument_id).as_str())
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

#[derive(Debug)]
struct HistoricalBarsActor {
    core: DataActorCore,
    bar_type: BarType,
    start: Timestamp,
    end: Timestamp,
    limit: NonZeroUsize,
    bars: Rc<RefCell<Vec<Bar>>>,
}

nautilus_actor!(HistoricalBarsActor);

impl HistoricalBarsActor {
    fn new(config: &AppConfig, bars: Rc<RefCell<Vec<Bar>>>) -> Self {
        Self {
            core: DataActorCore::new(DataActorConfig {
                actor_id: Some(ACTOR_ID.into()),
                ..Default::default()
            }),
            bar_type: config.bar_type(),
            start: config.history_start,
            end: config.history_end,
            limit: config.history_limit,
            bars,
        }
    }
}

impl DataActor for HistoricalBarsActor {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.request_bars(
            self.bar_type,
            Some(self.start),
            Some(self.end),
            Some(self.limit),
            Some(*LONGBRIDGE_CLIENT_ID),
            None,
        )?;
        Ok(())
    }

    fn on_historical_bars(&mut self, bars: &[Bar]) -> anyhow::Result<()> {
        self.bars.borrow_mut().extend_from_slice(bars);
        self.shutdown_system(Some(format!("received {} historical bars", bars.len())));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Candle {
    high: Price,
    low: Price,
    close: Price,
}

impl From<&Bar> for Candle {
    fn from(bar: &Bar) -> Self {
        Self {
            high: bar.high,
            low: bar.low,
            close: bar.close,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Breakout {
    #[default]
    None,
    Above {
        extreme: Price,
    },
    Below {
        extreme: Price,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RangeSignal {
    side: OrderSide,
    excursion: Price,
}

#[derive(Debug, Default)]
struct RangeState {
    date: Option<Date>,
    range_high: Option<Price>,
    range_low: Option<Price>,
    breakout: Breakout,
    trades_today: usize,
    flatten_requested: bool,
}

impl RangeState {
    fn roll_date(&mut self, date: Date) -> bool {
        let changed = self.date.is_some_and(|previous| previous != date);
        if self.date != Some(date) {
            self.date = Some(date);
            self.range_high = None;
            self.range_low = None;
            self.breakout = Breakout::None;
            self.trades_today = 0;
            self.flatten_requested = false;
        }
        changed
    }

    fn observe(&mut self, minute: u16, candle: Candle, rules: SessionRules) -> Option<RangeSignal> {
        if (rules.range_start_minute..rules.range_end_minute).contains(&minute) {
            self.range_high = Some(
                self.range_high
                    .map_or(candle.high, |value| value.max(candle.high)),
            );
            self.range_low = Some(
                self.range_low
                    .map_or(candle.low, |value| value.min(candle.low)),
            );
            self.breakout = Breakout::None;
            return None;
        }

        if !(rules.entry_start_minute..rules.entry_end_minute).contains(&minute)
            || self.trades_today >= rules.max_trades_per_day
        {
            return None;
        }

        let (range_high, range_low) = (self.range_high?, self.range_low?);
        if range_high <= range_low {
            return None;
        }

        match self.breakout {
            Breakout::None => {
                if candle.close > range_high {
                    self.breakout = Breakout::Above {
                        extreme: candle.high,
                    };
                } else if candle.close < range_low {
                    self.breakout = Breakout::Below {
                        extreme: candle.low,
                    };
                }
                None
            }
            Breakout::Above { extreme } => {
                let extreme = extreme.max(candle.high);
                if candle.close > range_low && candle.close < range_high {
                    self.breakout = Breakout::None;
                    Some(RangeSignal {
                        side: OrderSide::Sell,
                        excursion: extreme,
                    })
                } else {
                    self.breakout = if candle.close < range_low {
                        Breakout::Below {
                            extreme: candle.low,
                        }
                    } else {
                        Breakout::Above { extreme }
                    };
                    None
                }
            }
            Breakout::Below { extreme } => {
                let extreme = extreme.min(candle.low);
                if candle.close > range_low && candle.close < range_high {
                    self.breakout = Breakout::None;
                    Some(RangeSignal {
                        side: OrderSide::Buy,
                        excursion: extreme,
                    })
                } else {
                    self.breakout = if candle.close > range_high {
                        Breakout::Above {
                            extreme: candle.high,
                        }
                    } else {
                        Breakout::Below { extreme }
                    };
                    None
                }
            }
        }
    }

    fn record_trade(&mut self) {
        self.trades_today += 1;
    }
}

#[derive(Clone, Debug)]
struct RangeFakeoutConfig {
    instrument_id: InstrumentId,
    bar_type: BarType,
    trade_size: Quantity,
    timezone: TimeZone,
    rules: SessionRules,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
}

struct RangeFakeoutStrategy {
    core: StrategyCore,
    config: RangeFakeoutConfig,
    instrument: InstrumentAny,
    state: RangeState,
}

impl RangeFakeoutStrategy {
    fn new(config: RangeFakeoutConfig, instrument: InstrumentAny) -> Self {
        Self {
            core: StrategyCore::new(StrategyConfig {
                strategy_id: Some(StrategyId::from("RANGE-FAKEOUT-001")),
                order_id_tag: Some("001".to_string()),
                ..Default::default()
            }),
            config,
            instrument,
            state: RangeState::default(),
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

    fn flatten(&mut self) -> anyhow::Result<()> {
        if !self.has_exposure() {
            return Ok(());
        }
        self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        self.close_all_positions(
            self.config.instrument_id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn submit_signal(&mut self, signal: RangeSignal, entry: Price) -> anyhow::Result<()> {
        let prices = bracket_prices(
            entry,
            signal.excursion,
            signal.side,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.stop_buffer_ticks,
            self.config.risk_reward,
        );
        let (stop, target) = match prices {
            Ok(prices) => prices,
            Err(e) => {
                log::warn!("Skipping invalid range-fakeout signal: {e}");
                return Ok(());
            }
        };
        self.instrument.try_normalize_price(stop)?;
        self.instrument.try_normalize_price(target)?;

        let orders = self
            .order()
            .bracket()
            .instrument_id(self.config.instrument_id)
            .order_side(signal.side)
            .quantity(self.config.trade_size)
            .entry_order_type(OrderType::Market)
            .tp_price(target)
            .tp_post_only(false)
            .sl_trigger_price(stop)
            .call();
        self.submit_order_list(orders, None, None, None)?;
        self.state.record_trade();
        Ok(())
    }
}

impl Debug for RangeFakeoutStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeFakeoutStrategy")
            .field("config", &self.config)
            .field("state", &self.state)
            .finish()
    }
}

nautilus_strategy!(RangeFakeoutStrategy);

impl DataActor for RangeFakeoutStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        let spec = self.config.bar_type.spec();
        anyhow::ensure!(
            self.config.bar_type.instrument_id() == self.config.instrument_id,
            "bar type instrument does not match strategy instrument",
        );
        anyhow::ensure!(
            spec.step.get() == 5
                && spec.aggregation == BarAggregation::Minute
                && spec.price_type == PriceType::Last
                && self.config.bar_type.aggregation_source() == AggregationSource::External,
            "strategy requires 5-MINUTE-LAST-EXTERNAL bars",
        );
        self.config.rules.validate()?;
        self.cache().try_instrument(&self.config.instrument_id)?;
        self.subscribe_bars(self.config.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.flatten()?;
        self.unsubscribe_bars(self.config.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if bar.bar_type != self.config.bar_type {
            return Ok(());
        }

        let local = bar
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
        if self.state.roll_date(local.date()) {
            self.flatten()?;
        }

        if minute >= self.config.rules.entry_end_minute {
            if !self.state.flatten_requested {
                self.flatten()?;
                self.state.flatten_requested = true;
            }
            return Ok(());
        }

        let signal = self
            .state
            .observe(minute, Candle::from(bar), self.config.rules);
        if let Some(signal) = signal
            && !self.has_exposure()
        {
            self.submit_signal(signal, bar.close)?;
        }
        Ok(())
    }
}

fn bracket_prices(
    entry: Price,
    excursion: Price,
    side: OrderSide,
    increment: Price,
    precision: u8,
    stop_buffer_ticks: u64,
    risk_reward: Decimal,
) -> anyhow::Result<(Price, Price)> {
    let entry = entry.as_decimal();
    let increment = increment.as_decimal();
    let buffer = increment * Decimal::from(stop_buffer_ticks);
    let (stop, target) = match side {
        OrderSide::Buy => {
            let stop = excursion.as_decimal() - buffer;
            let risk = entry - stop;
            anyhow::ensure!(risk > Decimal::ZERO, "long stop must be below entry");
            let target = ((entry + risk * risk_reward) / increment).floor() * increment;
            (stop, target)
        }
        OrderSide::Sell => {
            let stop = excursion.as_decimal() + buffer;
            let risk = stop - entry;
            anyhow::ensure!(risk > Decimal::ZERO, "short stop must be above entry");
            let target = ((entry - risk * risk_reward) / increment).ceil() * increment;
            (stop, target)
        }
        OrderSide::NoOrderSide => anyhow::bail!("signal side is unspecified"),
    };
    anyhow::ensure!(stop > Decimal::ZERO, "stop price must be positive");
    anyhow::ensure!(target > Decimal::ZERO, "target price must be positive");
    anyhow::ensure!(
        (side == OrderSide::Buy && target > entry) || (side == OrderSide::Sell && target < entry),
        "target collapsed to entry after tick rounding",
    );
    Ok((
        Price::from_decimal_dp(stop, precision)?,
        Price::from_decimal_dp(target, precision)?,
    ))
}

async fn download_history(config: &AppConfig) -> anyhow::Result<(InstrumentAny, Vec<Bar>)> {
    let data_config = LongbridgeDataClientConfig {
        enable_overnight: true,
        instrument_price_increments: HashMap::from([(
            config.instrument_id.to_string(),
            config.price_increment.to_string(),
        )]),
        ..Default::default()
    };
    let received = Rc::new(RefCell::new(Vec::new()));
    let mut node = LiveNode::builder(TraderId::from(TRADER_ID), Environment::Live)?
        .with_name(NODE_NAME.to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_delay_post_stop_secs(0)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(data_config),
        )?
        .build()?;
    node.add_actor(HistoricalBarsActor::new(config, Rc::clone(&received)))?;

    let handle = node.handle();
    let timeout_secs = config.download_timeout_secs;
    let timeout = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
        handle.stop();
    });
    let run_result = node.run().await;
    timeout.abort();
    run_result?;

    let instrument = {
        let cache = node.kernel().cache();
        let cache = cache.borrow();
        cache.instrument(&config.instrument_id).cloned()
    }
    .with_context(|| {
        format!(
            "Longbridge did not load instrument {}",
            config.instrument_id
        )
    })?;
    let mut bars = received.borrow().clone();
    anyhow::ensure!(
        !bars.is_empty(),
        "Longbridge returned no bars before the {} second timeout",
        config.download_timeout_secs,
    );

    // Longbridge candle timestamps identify interval opens. Replay each completed bar at its close;
    // using the live-response creation time would collapse the whole history onto one timestamp.
    for bar in &mut bars {
        bar.ts_init = bar
            .ts_event
            .checked_add(FIVE_MINUTES_NS)
            .context("historical bar close timestamp overflowed")?;
    }
    Ok((instrument, bars))
}

fn run_backtest(
    config: &AppConfig,
    instrument: InstrumentAny,
    bars: Vec<Bar>,
) -> anyhow::Result<()> {
    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(config.instrument_id.venue)
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![config.starting_balance])
            .build()?,
    )?;
    engine.add_instrument(&instrument)?;
    engine.add_strategy(RangeFakeoutStrategy::new(
        RangeFakeoutConfig {
            instrument_id: config.instrument_id,
            bar_type: config.bar_type(),
            trade_size: config.trade_size,
            timezone: config.timezone.clone(),
            rules: config.rules,
            risk_reward: config.risk_reward,
            stop_buffer_ticks: config.stop_buffer_ticks,
        },
        instrument,
    ))?;
    let bar_count = bars.len();
    engine.add_data(bars.into_iter().map(Data::Bar).collect(), None, true, true)?;
    engine.run(None, None, None, false)?;

    let result = engine.get_result();
    println!(
        "Backtest complete: bars={bar_count}, orders={}, positions={}",
        result.total_orders, result.total_positions,
    );
    println!("PnL statistics: {:?}", result.stats_pnls);
    println!("General statistics: {:?}", result.stats_general);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AppConfig::from_env()?;
    let (instrument, bars) = download_history(&config).await?;
    run_backtest(&config, instrument, bars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(high: &str, low: &str, close: &str) -> Candle {
        Candle {
            high: Price::from(high),
            low: Price::from(low),
            close: Price::from(close),
        }
    }

    #[test]
    fn close_breakout_and_reentry_generate_both_directions() {
        let rules = SessionRules {
            range_start_minute: 0,
            range_end_minute: 240,
            entry_start_minute: 240,
            entry_end_minute: 960,
            max_trades_per_day: 1,
        };
        let mut state = RangeState::default();
        state.roll_date("2026-08-24".parse().unwrap());
        assert_eq!(state.observe(0, candle("105", "95", "100"), rules), None);

        // A wick above the range is not a breakout.
        assert_eq!(state.observe(240, candle("106", "99", "104"), rules), None);
        assert_eq!(state.observe(245, candle("107", "105", "106"), rules), None);
        assert_eq!(
            state.observe(250, candle("108", "100", "104"), rules),
            Some(RangeSignal {
                side: OrderSide::Sell,
                excursion: Price::from("108"),
            }),
        );
        state.record_trade();
        assert_eq!(state.observe(255, candle("107", "100", "106"), rules), None);

        state.roll_date("2026-08-25".parse().unwrap());
        state.observe(0, candle("105", "95", "100"), rules);
        assert_eq!(state.observe(240, candle("95", "93", "94"), rules), None);
        assert_eq!(
            state.observe(245, candle("100", "92", "96"), rules),
            Some(RangeSignal {
                side: OrderSide::Buy,
                excursion: Price::from("92"),
            }),
        );
    }

    #[test]
    fn bracket_prices_use_exact_ticks_and_conservative_target_rounding() {
        let (short_stop, short_target) = bracket_prices(
            Price::from("104.00"),
            Price::from("108.00"),
            OrderSide::Sell,
            Price::from("0.01"),
            2,
            1,
            Decimal::from(2),
        )
        .unwrap();
        assert_eq!(short_stop, Price::from("108.01"));
        assert_eq!(short_target, Price::from("95.98"));

        let (_, long_target) = bracket_prices(
            Price::from("100.00"),
            Price::from("98.00"),
            OrderSide::Buy,
            Price::from("0.05"),
            2,
            1,
            Decimal::new(15, 1),
        )
        .unwrap();
        assert_eq!(long_target, Price::from("103.05"));
    }
}
