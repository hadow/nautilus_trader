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
//! No arguments prints the plan. `--paper-buy` / `--paper-sell` sends one paper limit order.
//! This tester has no alpha advantage and is not intended for production trading.
//!
//! Edit the instrument constants below and verify them against the venue before running.
//!
//! Run with:
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-exec-tester`
//!
//! Required environment variable:
//! - `LONGBRIDGE_OAUTH_CLIENT_ID`: OAuth 2.0 public client ID.

mod intraday_calendar;

use std::time::Duration;

use longbridge::{TradeContext, quote::QuoteContext, trade::GetTodayOrdersOptions};
use nautilus_common::{enums::Environment, live::get_runtime};
use nautilus_core::UnixNanos;
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{
        consts::LONGBRIDGE_CLIENT_ID, parse::parse_order_status_report, rate_limit::trade_api_call,
    },
};
use nautilus_model::{
    enums::{AccountType, OrderSide, TimeInForce},
    events::OrderEventAny,
    identifiers::{AccountId, InstrumentId, StrategyId, Symbol, TraderId},
    instruments::{Equity, InstrumentAny},
    orders::{Order, OrderAny},
    types::{Currency, Price, Quantity},
};
use nautilus_testkit::testers::{ExecTester, ExecTesterConfig};
use nautilus_trading::strategy::StrategyConfig;
use rust_decimal::Decimal;

const TRADER_ID: &str = "TESTER-001";
const ACCOUNT_ID: &str = "LONGBRIDGE-001";
const NODE_NAME: &str = "LONGBRIDGE-EXEC-TESTER-001";
const STRATEGY_ID: &str = "EXEC_TESTER-001";
const INSTRUMENT_ID: &str = "F.US.LONGBRIDGE";
const RAW_SYMBOL: &str = "F.US";
const CURRENCY: &str = "USD";
const PRICE_INCREMENT: &str = "0.01";
const LOT_SIZE: &str = "1";
const ORDER_QTY: &str = "1";
const TOB_OFFSET_TICKS: u64 = 1;
const AUTO_STOP_SECS: u64 = 60;
const MAX_ORDER_VALUE: &str = "300";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() <= 1
            && args.first().is_none_or(|arg| matches!(
                arg.as_str(),
                "--check-paper" | "--paper-buy" | "--paper-sell"
            )),
        "Usage: longbridge-exec-tester [--check-paper | --paper-buy | --paper-sell]"
    );
    println!(
        "Paper only: {RAW_SYMBOL}, one share, limit order, max notional ${MAX_ORDER_VALUE}, no automatic market close"
    );
    if args.is_empty() {
        return Ok(());
    }
    let buy = args[0] == "--paper-buy";
    let execute = args[0] != "--check-paper";
    let exec_config = execution_config();
    let (broker, _pushes) = TradeContext::new(exec_config.sdk_config().await?);
    let quantity = paper_quantity(&broker).await?;
    let active = paper_active_orders(&broker).await?;
    let session_open = {
        let (quotes, _pushes) =
            QuoteContext::new(LongbridgeDataClientConfig::default().sdk_config().await?);
        let now = jiff::Timestamp::now();
        let sessions = intraday_calendar::calendar(&quotes, now, 0).await?;
        let timestamp: UnixNanos = now.into();
        sessions.iter().any(|session| {
            session.open <= timestamp
                && timestamp.as_u64() + (AUTO_STOP_SECS + 30) * 1_000_000_000
                    < session.close.as_u64()
        })
    };
    println!(
        "PAPER_PREFLIGHT {}",
        serde_json::json!({"symbol":RAW_SYMBOL,"position":quantity.to_string(),"active_orders":active,"regular_session_open":session_open})
    );
    if !execute {
        return Ok(());
    }
    anyhow::ensure!(
        session_open,
        "Paper acceptance requires the regular session with time to stop safely"
    );
    anyhow::ensure!(
        active == 0,
        "Existing orders for acceptance symbol; do not interfere"
    );
    anyhow::ensure!(
        quantity == if buy { Decimal::ZERO } else { Decimal::ONE },
        "Unexpected paper inventory; no order submitted"
    );
    let trader_id = TraderId::from(TRADER_ID);
    let account_id = AccountId::from(ACCOUNT_ID);
    let instrument = sample_equity()?;
    let instrument_id = instrument.id;

    let data_config = LongbridgeDataClientConfig::default();
    let exec_engine_config = LiveExecEngineConfig {
        reconciliation_lookback_mins: Some(60),
        reconciliation_instrument_ids: Some(vec![INSTRUMENT_ID.to_string()]),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(30.0),
        ..Default::default()
    };
    let risk_engine_config = LiveRiskEngineConfig {
        bypass: false,
        max_order_submit_rate: "2/00:01:00".into(),
        max_notional_per_order: [(INSTRUMENT_ID.to_string(), MAX_ORDER_VALUE.to_string())].into(),
        ..Default::default()
    };

    let mut node = LiveNode::builder(trader_id, Environment::Sandbox)?
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

    let started: UnixNanos = jiff::Timestamp::now().into();
    node.add_strategy(ExecTester::new(tester_config(buy)?))?;
    schedule_auto_stop(&node, AUTO_STOP_SECS);
    let run_result = node.run().await;

    let final_quantity = paper_quantity(&broker).await?;
    let remaining_orders = paper_active_orders(&broker).await?;
    let native: Vec<_> = cache
        .borrow()
        .orders_refs(
            None,
            Some(&instrument_id),
            Some(&StrategyId::from(STRATEGY_ID)),
            None,
            None,
        )
        .iter()
        .map(|order| order.cloned())
        .collect();
    let orders: Vec<_> = native.iter().map(|order| serde_json::json!({
        "client_order_id":order.client_order_id().to_string(),
        "venue_order_id":order.venue_order_id().map(|id| id.to_string()),
        "status":order.status().to_string(),"filled_quantity":order.filled_qty().to_string(),
        "submitted_this_run":submitted_this_run(order, started),
        "trade_ids":order.trade_ids().iter().map(ToString::to_string).collect::<Vec<_>>()
    })).collect();
    let expected = if buy { Decimal::ONE } else { Decimal::ZERO };
    let side = if buy { OrderSide::Buy } else { OrderSide::Sell };
    let current: Vec<_> = native
        .iter()
        .filter(|order| submitted_this_run(order, started))
        .collect();
    println!(
        "PAPER_ACCEPTANCE {}",
        serde_json::json!({"symbol":RAW_SYMBOL,"side":side.to_string(),"broker_quantity":final_quantity.to_string(),"remaining_orders":remaining_orders,"native_orders":orders})
    );
    run_result?;
    anyhow::ensure!(
        final_quantity == expected
            && remaining_orders == 0
            && current.len() == 1
            && current[0].order_side() == side
            && current[0].filled_qty().as_decimal() == Decimal::ONE
            && !current[0].trade_ids().is_empty(),
        "Paper acceptance incomplete; inspect broker state before another run"
    );

    Ok(())
}

fn submitted_this_run(order: &OrderAny, started: UnixNanos) -> bool {
    // 恢复订单的本地初始化时间也可能是当前时间；只有真实的 Submitted 事件才证明本轮下单。
    order
        .events()
        .iter()
        .any(|event| matches!(event, OrderEventAny::Submitted(event) if event.ts_event >= started))
}

fn tester_config(buy: bool) -> anyhow::Result<ExecTesterConfig> {
    let instrument_id = InstrumentId::from(INSTRUMENT_ID);
    Ok(ExecTesterConfig::builder()
        .base(StrategyConfig {
            strategy_id: Some(StrategyId::from(STRATEGY_ID)),
            external_order_claims: Some(vec![instrument_id]),
            ..Default::default()
        })
        .instrument_id(instrument_id)
        .client_id(*LONGBRIDGE_CLIENT_ID)
        .order_qty(Quantity::from(ORDER_QTY))
        .subscribe_quotes(true)
        .subscribe_trades(true)
        .enable_limit_buys(buy)
        .enable_limit_sells(!buy)
        .limit_aggressive(true)
        .tob_offset_ticks(TOB_OFFSET_TICKS)
        .limit_time_in_force(TimeInForce::Day)
        .use_post_only(false)
        .cancel_orders_on_stop(true)
        .use_individual_cancels_on_stop(true)
        .close_positions_on_stop(false)
        .close_positions_time_in_force(TimeInForce::Day)
        .reduce_only_on_stop(false)
        .dry_run(false)
        .log_data(false)
        .build()?)
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
        account_type: AccountType::Cash,
        papertrading: true,
        outside_rth: false,
        ..Default::default()
    }
}

async fn paper_quantity(broker: &TradeContext) -> anyhow::Result<Decimal> {
    Ok(trade_api_call(broker.stock_positions(None))
        .await?
        .channels
        .iter()
        .flat_map(|channel| &channel.positions)
        .filter(|p| p.symbol == RAW_SYMBOL)
        .map(|p| p.quantity)
        .sum())
}

async fn paper_active_orders(broker: &TradeContext) -> anyhow::Result<usize> {
    let orders =
        trade_api_call(broker.today_orders(GetTodayOrdersOptions::new().symbol(RAW_SYMBOL)))
            .await?;
    let reports = orders
        .iter()
        .map(|order| parse_order_status_report(order, AccountId::from(ACCOUNT_ID), None, 0.into()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(reports
        .iter()
        .filter(|report| !report.order_status.is_closed())
        .count())
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
    use nautilus_core::UUID4;
    use nautilus_model::{
        enums::OrderType, events::OrderSubmitted, orders::builder::OrderTestBuilder,
    };
    use rust_decimal::Decimal;

    use super::*;

    #[rstest::rstest]
    fn test_restored_orders_are_not_new_submissions() {
        let mut current = OrderTestBuilder::new(OrderType::Limit)
            .instrument_id(InstrumentId::from(INSTRUMENT_ID))
            .quantity(Quantity::from(ORDER_QTY))
            .price(Price::from("12.89"))
            .build();
        let started = UnixNanos::from(100);
        assert!(!submitted_this_run(&current, started));
        let submitted = |order: &OrderAny, timestamp| {
            OrderEventAny::Submitted(OrderSubmitted::new(
                order.trader_id(),
                order.strategy_id(),
                order.instrument_id(),
                order.client_order_id(),
                AccountId::from(ACCOUNT_ID),
                UUID4::new(),
                timestamp,
                timestamp,
            ))
        };
        let mut restored = current.clone();
        restored.apply(submitted(&restored, 10.into())).unwrap();
        current.apply(submitted(&current, 101.into())).unwrap();
        assert!(!submitted_this_run(&restored, started));
        assert!(submitted_this_run(&current, started));
    }

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
        assert!(!execution_config().outside_rth);
        assert_eq!(execution_config().account_type, AccountType::Cash);
        assert_eq!(Quantity::from(ORDER_QTY).as_decimal(), Decimal::ONE);
        for buy in [true, false] {
            let config = tester_config(buy).unwrap();
            assert_eq!(config.enable_limit_buys, buy);
            assert_eq!(config.enable_limit_sells, !buy);
            assert!(config.limit_aggressive);
            assert!(config.open_position_on_start_qty.is_none());
            assert!(!config.close_positions_on_stop);
            assert!(config.use_individual_cancels_on_stop);
        }
    }
}
