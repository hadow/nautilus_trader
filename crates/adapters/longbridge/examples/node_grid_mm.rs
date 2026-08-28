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

//! Example demonstrating the built-in GridMarketMaker strategy with Longbridge stocks.
//!
//! WARNING: This example submits orders. The defaults route them to Longbridge paper trading,
//! but setting `PAPERTRADING = false` routes orders to the configured live margin account.
//! A symmetric grid can open both long and short positions and has no guaranteed profitability.
//!
//! Longbridge does not support post-only or reduce-only instructions. This example disables
//! post-only grid orders and reduce-only exit orders explicitly. Limit orders can therefore execute
//! immediately, and the asynchronous cancel-and-close sequence cannot guarantee a flat account.
//! Inspect the broker account after shutdown.
//!
//! The example starts one strategy per entry in `INSTRUMENTS`, so position limits and order routing
//! remain independent for each symbol. Edit each instrument definition and verify it against
//! Longbridge before running. There is no aggregate account-level position or notional cap.
//! The node queries Longbridge's trading-day and trading-session APIs and runs until the current US
//! regular session closes. It refuses to start on a non-trading day or after the regular close.
//!
//! Run with:
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-grid-mm`
//!
//! Required environment variable:
//! - `LONGBRIDGE_OAUTH_CLIENT_ID`: OAuth 2.0 public client ID.

use std::time::Duration;

use anyhow::Context;
use jiff::{Timestamp, civil::Time as CivilTime};
use longbridge::{
    Market,
    quote::{QuoteContext, TradeSession},
};
use nautilus_common::{enums::Environment, live::get_runtime};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_live::{config::LiveExecEngineConfig, node::LiveNode};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
};
use nautilus_model::{
    enums::{AccountType, TimeInForce},
    identifiers::{AccountId, InstrumentId, StrategyId, Symbol, TraderId},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Price, Quantity},
};
use nautilus_trading::{
    examples::strategies::{GridMarketMaker, GridMarketMakerConfig},
    strategy::StrategyConfig,
};
use time::{Date, Month, Time};

const TRADER_ID: &str = "TRADER-001";
const ACCOUNT_ID: &str = "LONGBRIDGE-001";
const NODE_NAME: &str = "LONGBRIDGE-GRID-MM-001";
const PAPERTRADING: bool = true;
const US_TIMEZONE: &str = "America/New_York";

#[derive(Clone, Copy)]
struct InstrumentSpec {
    strategy_id: &'static str,
    order_id_tag: &'static str,
    instrument_id: &'static str,
    raw_symbol: &'static str,
    currency: &'static str,
    price_increment: &'static str,
    lot_size: &'static str,
    trade_size: &'static str,
    max_position: &'static str,
    num_levels: usize,
    grid_step_bps: u32,
    skew_factor: f64,
    requote_threshold_bps: u32,
}

const INSTRUMENTS: &[InstrumentSpec] = &[
    InstrumentSpec {
        strategy_id: "GRID_AAPL-001",
        order_id_tag: "101",
        instrument_id: "AAPL.US.LONGBRIDGE",
        raw_symbol: "AAPL.US",
        currency: "USD",
        price_increment: "0.01",
        lot_size: "1",
        trade_size: "10",
        max_position: "60",
        num_levels: 3,
        grid_step_bps: 25,
        skew_factor: 0.02,
        requote_threshold_bps: 10,
    },
    InstrumentSpec {
        strategy_id: "GRID_MSFT-001",
        order_id_tag: "102",
        instrument_id: "MSFT.US.LONGBRIDGE",
        raw_symbol: "MSFT.US",
        currency: "USD",
        price_increment: "0.01",
        lot_size: "1",
        trade_size: "10",
        max_position: "60",
        num_levels: 3,
        grid_step_bps: 25,
        skew_factor: 0.02,
        requote_threshold_bps: 10,
    },
    InstrumentSpec {
        strategy_id: "GRID_NVDA-001",
        order_id_tag: "103",
        instrument_id: "NVDA.US.LONGBRIDGE",
        raw_symbol: "NVDA.US",
        currency: "USD",
        price_increment: "0.01",
        lot_size: "1",
        trade_size: "10",
        max_position: "60",
        num_levels: 3,
        grid_step_bps: 25,
        skew_factor: 0.02,
        requote_threshold_bps: 10,
    },
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let environment = if PAPERTRADING {
        Environment::Sandbox
    } else {
        Environment::Live
    };
    let trader_id = TraderId::from(TRADER_ID);
    let account_id = AccountId::from(ACCOUNT_ID);
    let data_config = LongbridgeDataClientConfig::default();
    let market_close = current_us_market_close(&data_config).await?;
    let instruments = INSTRUMENTS
        .iter()
        .map(sample_equity)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let instrument_ids = instruments
        .iter()
        .map(|instrument| instrument.id)
        .collect::<Vec<_>>();

    let exec_engine_config = LiveExecEngineConfig {
        reconciliation_lookback_mins: Some(60),
        reconciliation_instrument_ids: Some(
            INSTRUMENTS
                .iter()
                .map(|spec| spec.instrument_id.to_string())
                .collect(),
        ),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(30.0),
        ..Default::default()
    };

    let mut node = LiveNode::builder(trader_id, environment)?
        .with_name(NODE_NAME.to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_exec_engine_config(exec_engine_config)
        .with_reconciliation(true)
        .with_delay_post_stop_secs(5)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(data_config),
        )?
        .add_exec_client(
            None,
            Box::new(LongbridgeExecutionClientFactory::new(trader_id, account_id)),
            Box::new(execution_config()),
        )?
        .build()?;

    let cache = node.kernel().cache();
    for instrument in instruments {
        cache
            .borrow_mut()
            .add_instrument(InstrumentAny::Equity(instrument))?;
    }

    for (spec, instrument_id) in INSTRUMENTS.iter().zip(instrument_ids) {
        node.add_strategy(GridMarketMaker::new(grid_config(spec, instrument_id)))?;
    }

    schedule_market_close(&node, market_close)?;
    node.run().await?;

    Ok(())
}

fn execution_config() -> LongbridgeExecClientConfig {
    LongbridgeExecClientConfig {
        account_type: AccountType::Margin,
        papertrading: PAPERTRADING,
        outside_rth: false,
        ..Default::default()
    }
}

fn grid_config(spec: &InstrumentSpec, instrument_id: InstrumentId) -> GridMarketMakerConfig {
    GridMarketMakerConfig::builder()
        .base(StrategyConfig {
            strategy_id: Some(StrategyId::from(spec.strategy_id)),
            order_id_tag: Some(spec.order_id_tag.to_string()),
            external_order_claims: Some(vec![instrument_id]),
            market_exit_time_in_force: TimeInForce::Day,
            market_exit_reduce_only: false,
            ..Default::default()
        })
        .instrument_id(instrument_id)
        .trade_size(Quantity::from(spec.trade_size))
        .max_position(Quantity::from(spec.max_position))
        .post_only(false)
        .num_levels(spec.num_levels)
        .grid_step_bps(spec.grid_step_bps)
        .skew_factor(spec.skew_factor)
        .requote_threshold_bps(spec.requote_threshold_bps)
        .build()
}

async fn current_us_market_close(
    data_config: &LongbridgeDataClientConfig,
) -> anyhow::Result<Timestamp> {
    let now = Timestamp::now();
    let market_date = us_market_date(now)?;
    let sdk_config = data_config.sdk_config().await?;
    let (context, _receiver) = QuoteContext::new(sdk_config);
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

    us_market_close_at(now, close_time)
}

fn us_market_date(now: Timestamp) -> anyhow::Result<Date> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let local_date = now.to_zoned(timezone).date();
    let month = Month::try_from(u8::try_from(local_date.month())?)?;

    Ok(Date::from_calendar_date(
        i32::from(local_date.year()),
        month,
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
    let market_close = timezone
        .to_ambiguous_timestamp(local_date.to_datetime(civil_close_time))
        .unambiguous()
        .context("US market close must resolve to one timestamp")?;

    if market_close <= now {
        anyhow::bail!("US regular trading session has already closed at {market_close}");
    }

    Ok(market_close)
}

fn schedule_market_close(node: &LiveNode, market_close: Timestamp) -> anyhow::Result<()> {
    let delay = Duration::try_from(Timestamp::now().duration_until(market_close))
        .context("US market close must be in the future")?;

    let handle = node.handle();
    log::info!("Longbridge grid example will stop at US market close: {market_close}");

    get_runtime().spawn(async move {
        tokio::time::sleep(delay).await;
        handle.stop();
    });

    Ok(())
}

fn sample_equity(spec: &InstrumentSpec) -> anyhow::Result<Equity> {
    let price_increment = Price::from(spec.price_increment);

    Ok(Equity::new_checked(
        InstrumentId::from(spec.instrument_id),
        Symbol::from(spec.raw_symbol),
        None,
        Currency::from(spec.currency),
        price_increment.precision,
        price_increment,
        Some(Quantity::from(spec.lot_size)),
        None,
        Some(Quantity::from(spec.lot_size)),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        UnixNanos::default(),
        UnixNanos::default(),
    )?)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[rstest::rstest]
    fn test_sample_equities_match_longbridge_symbol_contract() {
        for spec in INSTRUMENTS {
            let instrument = sample_equity(spec).unwrap();

            assert_eq!(instrument.id, InstrumentId::from(spec.instrument_id));
            assert_eq!(instrument.raw_symbol, Symbol::from(spec.raw_symbol));
            assert_eq!(
                instrument.price_increment,
                Price::from(spec.price_increment)
            );
            assert_eq!(instrument.lot_size, Some(Quantity::from(spec.lot_size)));
        }
    }

    #[rstest::rstest]
    fn test_defaults_match_longbridge_order_capabilities() {
        assert!(execution_config().papertrading);
        assert_eq!(execution_config().account_type, AccountType::Margin);

        for spec in INSTRUMENTS {
            let config = grid_config(spec, InstrumentId::from(spec.instrument_id));

            assert!(!config.post_only);
            assert_eq!(config.base.market_exit_time_in_force, TimeInForce::Day);
            assert!(!config.base.market_exit_reduce_only);
            assert_eq!(config.trade_size, Some(Quantity::from(spec.trade_size)));
            assert_eq!(config.max_position, Quantity::from(spec.max_position));
            assert_eq!(config.num_levels, spec.num_levels);
            assert_eq!(config.grid_step_bps, spec.grid_step_bps);
            assert_eq!(config.requote_threshold_bps, spec.requote_threshold_bps);
        }
    }

    #[rstest::rstest]
    fn test_example_uses_multi_instrument_active_grid_defaults() {
        assert_eq!(INSTRUMENTS.len(), 3);

        for spec in INSTRUMENTS {
            assert_eq!(spec.trade_size, "10");
            assert_eq!(spec.max_position, "60");
            assert_eq!(spec.num_levels, 3);
            assert_eq!(spec.grid_step_bps, 25);
            assert_eq!(spec.requote_threshold_bps, 10);
        }
    }

    #[rstest::rstest]
    fn test_instrument_specs_have_unique_routing_identity() {
        let strategy_ids = INSTRUMENTS
            .iter()
            .map(|spec| spec.strategy_id)
            .collect::<HashSet<_>>();
        let order_id_tags = INSTRUMENTS
            .iter()
            .map(|spec| spec.order_id_tag)
            .collect::<HashSet<_>>();
        let instrument_ids = INSTRUMENTS
            .iter()
            .map(|spec| spec.instrument_id)
            .collect::<HashSet<_>>();

        assert_eq!(strategy_ids.len(), INSTRUMENTS.len());
        assert_eq!(order_id_tags.len(), INSTRUMENTS.len());
        assert_eq!(instrument_ids.len(), INSTRUMENTS.len());
    }

    #[rstest::rstest]
    #[case("2026-06-29T14:00:00Z", 16, "2026-06-29T20:00:00Z")]
    #[case("2026-01-16T15:00:00Z", 16, "2026-01-16T21:00:00Z")]
    #[case("2026-11-27T15:00:00Z", 13, "2026-11-27T18:00:00Z")]
    fn test_us_market_close_handles_dst_and_half_days(
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

    #[rstest::rstest]
    fn test_us_market_close_rejects_start_after_close() {
        let now = "2026-06-29T20:00:01Z".parse::<Timestamp>().unwrap();

        assert!(us_market_close_at(now, Time::from_hms(16, 0, 0).unwrap()).is_err());
    }

    #[rstest::rstest]
    fn test_us_market_date_uses_new_york_calendar_day() {
        let now = "2026-06-30T01:00:00Z".parse::<Timestamp>().unwrap();

        assert_eq!(
            us_market_date(now).unwrap(),
            time::macros::date!(2026 - 06 - 29)
        );
    }
}
