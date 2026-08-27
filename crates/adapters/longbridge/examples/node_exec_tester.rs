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

//! Example demonstrating stock execution testing with the Longbridge adapter.
//!
//! WARNING: `DRY_RUN = false` submits orders. The defaults route them to Longbridge paper trading,
//! but setting `PAPERTRADING = false` routes orders to the configured live account.
//! This tester has no alpha advantage and is not intended for production trading.
//!
//! Edit the instrument constants below and verify them against the venue before running.
//!
//! Run with:
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-exec-tester`
//!
//! Required environment variable:
//! - `LONGBRIDGE_OAUTH_CLIENT_ID`: OAuth 2.0 public client ID.

use std::time::Duration;

use nautilus_common::{enums::Environment, live::get_runtime};
use nautilus_core::UnixNanos;
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory, common::consts::LONGBRIDGE_CLIENT_ID,
};
use nautilus_model::{
    enums::{AccountType, TimeInForce},
    identifiers::{AccountId, InstrumentId, StrategyId, Symbol, TraderId},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Price, Quantity},
};
use nautilus_testkit::testers::{ExecTester, ExecTesterConfig};
use nautilus_trading::strategy::StrategyConfig;

const TRADER_ID: &str = "TESTER-001";
const ACCOUNT_ID: &str = "LONGBRIDGE-001";
const NODE_NAME: &str = "LONGBRIDGE-EXEC-TESTER-001";
const STRATEGY_ID: &str = "EXEC_TESTER-001";
const INSTRUMENT_ID: &str = "AAPL.US.LONGBRIDGE";
const RAW_SYMBOL: &str = "AAPL.US";
const CURRENCY: &str = "USD";
const PRICE_INCREMENT: &str = "0.01";
const LOT_SIZE: &str = "1";
const ORDER_QTY: &str = "1";
const TOB_OFFSET_TICKS: u64 = 50;
const AUTO_STOP_SECS: u64 = 60;
const PAPERTRADING: bool = true;
const DRY_RUN: bool = false;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let environment = if PAPERTRADING {
        Environment::Sandbox
    } else {
        Environment::Live
    };
    let trader_id = TraderId::from(TRADER_ID);
    let account_id = AccountId::from(ACCOUNT_ID);
    let instrument = sample_equity()?;
    let instrument_id = instrument.id;

    let data_config = LongbridgeDataClientConfig::default();
    let exec_config = execution_config();
    let exec_engine_config = LiveExecEngineConfig {
        reconciliation_lookback_mins: Some(60),
        reconciliation_instrument_ids: Some(vec![INSTRUMENT_ID.to_string()]),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(30.0),
        ..Default::default()
    };
    let risk_engine_config = LiveRiskEngineConfig {
        bypass: true,
        ..Default::default()
    };

    let mut node = LiveNode::builder(trader_id, environment)?
        .with_name(NODE_NAME.to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_exec_engine_config(exec_engine_config)
        .with_risk_engine_config(risk_engine_config)
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
            Box::new(exec_config),
        )?
        .build()?;

    let cache = node.kernel().cache();
    cache
        .borrow_mut()
        .add_instrument(InstrumentAny::Equity(instrument))?;

    let order_qty = Quantity::from(ORDER_QTY);
    let tester_config = ExecTesterConfig::builder()
        .base(StrategyConfig {
            strategy_id: Some(StrategyId::from(STRATEGY_ID)),
            external_order_claims: Some(vec![instrument_id]),
            ..Default::default()
        })
        .instrument_id(instrument_id)
        .client_id(*LONGBRIDGE_CLIENT_ID)
        .order_qty(order_qty)
        .subscribe_quotes(true)
        .subscribe_trades(true)
        .open_position_on_start_qty(order_qty.as_decimal())
        .open_position_on_first_quote(true)
        .open_position_time_in_force(TimeInForce::Day)
        .enable_limit_buys(true)
        .enable_limit_sells(false)
        .tob_offset_ticks(TOB_OFFSET_TICKS)
        .limit_time_in_force(TimeInForce::Day)
        .use_post_only(false)
        .cancel_orders_on_stop(true)
        .close_positions_on_stop(true)
        .close_positions_time_in_force(TimeInForce::Day)
        .reduce_only_on_stop(false)
        .dry_run(DRY_RUN)
        .log_data(false)
        .build()?;

    node.add_strategy(ExecTester::new(tester_config))?;
    schedule_auto_stop(&node, AUTO_STOP_SECS);
    node.run().await?;

    Ok(())
}

fn schedule_auto_stop(node: &LiveNode, delay_secs: u64) {
    if delay_secs == 0 {
        return;
    }

    let handle = node.handle();

    get_runtime().spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        handle.stop();
    });
}

fn execution_config() -> LongbridgeExecClientConfig {
    LongbridgeExecClientConfig {
        account_type: AccountType::Margin,
        papertrading: PAPERTRADING,
        outside_rth: false,
        ..Default::default()
    }
}

fn sample_equity() -> anyhow::Result<Equity> {
    let price_increment = Price::from(PRICE_INCREMENT);

    Ok(Equity::new_checked(
        InstrumentId::from(INSTRUMENT_ID),
        Symbol::from(RAW_SYMBOL),
        None,
        Currency::from(CURRENCY),
        price_increment.precision,
        price_increment,
        Some(Quantity::from(LOT_SIZE)),
        None,
        Some(Quantity::from(LOT_SIZE)),
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
    use rust_decimal::Decimal;

    use super::*;

    #[rstest::rstest]
    fn test_sample_equity_matches_longbridge_symbol_contract() {
        let instrument = sample_equity().unwrap();

        assert_eq!(instrument.id, InstrumentId::from(INSTRUMENT_ID));
        assert_eq!(instrument.raw_symbol, Symbol::from(RAW_SYMBOL));
        assert_eq!(instrument.price_increment, Price::from(PRICE_INCREMENT));
        assert_eq!(instrument.lot_size, Some(Quantity::from(LOT_SIZE)));
    }

    #[rstest::rstest]
    fn test_safe_defaults_use_papertrading_with_one_share() {
        assert!(execution_config().papertrading);
        assert_eq!(Quantity::from(ORDER_QTY).as_decimal(), Decimal::ONE);
    }
}
