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

//! Validate by default; broker checks query only; --run requires preflight and explicit live routing.

mod dynamic_grid_config;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
    time::Duration,
};

use dynamic_grid_config::{AppConfig, Mode};
use longbridge::{
    TradeContext,
    trade::{
        AccountBalance, GetHistoryOrdersOptions, GetTodayExecutionsOptions, GetTodayOrdersOptions,
        Order, StockPositionsResponse,
    },
};
use nautilus_common::{actor::registry::try_get_actor_unchecked, enums::Environment};
use nautilus_execution::models::fee::{FeeModelAny, PerContractFeeModel};
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{
        parse::{parse_account_state, parse_order_status_report, parse_position_status_report},
        rate_limit::trade_api_call,
    },
};
use nautilus_model::{
    enums::{AccountType, OmsType},
    types::Money,
};
use nautilus_sandbox::{
    config::SandboxExecutionClientConfig, factory::SandboxExecutionClientFactory,
};
use nautilus_trading::examples::strategies::dynamic_grid::MultiAssetGridStrategy;
use rust_decimal::Decimal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        !args.is_empty(),
        "Usage: longbridge-dynamic-grid CONFIG.json [--check-paper | --check-live | --run [--live]]"
    );
    let app = AppConfig::load(Path::new(&args[0]))?;
    let action = action(app.mode, &args[1..])?;
    let strategy_config = app.strategy()?;
    let strategy_id = strategy_config
        .base
        .strategy_id
        .ok_or_else(|| anyhow::anyhow!("Missing strategy identity"))?;
    if action == Action::Check {
        let snapshot = query_snapshot(&app).await?;
        println!(
            "{}",
            serde_json::json!({
                "mode": app.mode, "read_only": true, "snapshot": snapshot,
                "checkpoint_exists": app.state_path.try_exists()?,
                "broker_fill_acceptance": "NOT_RUN",
                "timeout_reconciliation_acceptance": "NOT_RUN",
                "note": "Connectivity only; startup reconciliation and execution are not validated"
            })
        );
        return Ok(());
    }
    if action == Action::Validate {
        println!("{}", serde_json::to_string_pretty(&strategy_config)?);
        return Ok(());
    }
    // 先锁定并验证本地检查点，再检查整个券商账户，最后才创建可下单的节点。
    // 只读快照不是原子对账；启动后的 Nautilus 对账与策略库存核对仍不可省略。
    let strategy = MultiAssetGridStrategy::new(strategy_config)?;
    if app.mode != Mode::Sandbox {
        require_startup_ready(&query_snapshot(&app).await?)?;
    }
    let report = strategy.portfolio_report_handle();
    let environment = if app.mode == Mode::Live {
        Environment::Live
    } else {
        Environment::Sandbox
    };
    let instrument_names: Vec<_> = app.instruments.keys().map(ToString::to_string).collect();
    let data = LongbridgeDataClientConfig {
        instrument_price_increments: app
            .instruments
            .iter()
            .map(|(id, c)| (id.to_string(), c.price_increment.to_string()))
            .collect(),
        enable_overnight: false,
        ..Default::default()
    };
    data.validate()?;
    let mut builder = LiveNode::builder(app.trader_id, environment)?
        .with_name("DYNAMIC-GRID".to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_reconciliation(app.mode != Mode::Sandbox)
        .with_exec_engine_config(LiveExecEngineConfig {
            reconciliation: app.mode != Mode::Sandbox,
            reconciliation_instrument_ids: Some(instrument_names),
            reconciliation_lookback_mins: Some(60 * 24 * 30),
            open_check_interval_secs: Some(10.0),
            position_check_interval_secs: Some(30.0),
            ..Default::default()
        })
        .with_risk_engine_config(LiveRiskEngineConfig {
            bypass: false,
            max_order_submit_rate: format!("{}/00:01:00", app.portfolio.max_orders_per_minute),
            max_order_modify_rate: "5/00:00:01".to_string(),
            max_notional_per_order: app
                .instruments
                .iter()
                .map(|(id, c)| {
                    (
                        id.to_string(),
                        c.strategy
                            .grid
                            .max_notional
                            .min(app.portfolio.max_order_value)
                            .to_string(),
                    )
                })
                .collect(),
            ..Default::default()
        })
        .with_delay_post_stop_secs(10)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(data),
        )?;
    if app.mode == Mode::Sandbox {
        anyhow::ensure!(
            !app.state_path.exists(),
            "A fresh sandbox account cannot resume old broker state; use a new state path"
        );
        builder = builder.add_simulated_exec_client(
            None,
            Box::new(SandboxExecutionClientFactory::new()),
            Box::new(SandboxExecutionClientConfig {
                trader_id: app.trader_id,
                account_id: app.account_id,
                venue: app
                    .instruments
                    .keys()
                    .next()
                    .expect("Validated universe")
                    .venue,
                starting_balances: vec![Money::from_decimal(app.portfolio.capital, app.currency)?],
                base_currency: Some(app.currency),
                account_type: AccountType::Cash,
                oms_type: OmsType::Netting,
                use_reduce_only: false,
                fee_model: Some(FeeModelAny::PerContract(PerContractFeeModel::from_rate(
                    app.sandbox_fee_per_share
                        .ok_or_else(|| anyhow::anyhow!("Missing simulator fee"))?,
                    app.currency,
                )?)),
                bar_execution: false,
                trade_execution: true,
                ..Default::default()
            }),
        )?;
    } else {
        builder = builder.add_exec_client(
            None,
            Box::new(LongbridgeExecutionClientFactory::new(
                app.trader_id,
                app.account_id,
            )),
            Box::new(LongbridgeExecClientConfig {
                papertrading: app.mode == Mode::Paper,
                account_type: AccountType::Cash,
                outside_rth: false,
                ..Default::default()
            }),
        )?;
    }
    let mut node = builder.build()?;
    strategy.restore_cache(&mut node.kernel().cache().borrow_mut())?;
    node.add_strategy(strategy)?;
    node.run().await?;
    // We registered this exact concrete type above; the native registry retains it until dispose.
    let mut strategy = try_get_actor_unchecked::<MultiAssetGridStrategy>(&strategy_id.inner())
        .ok_or_else(|| anyhow::anyhow!("Stopped strategy unavailable for final reconciliation"))?;
    strategy.finalize_after_stop()?;
    serde_json::to_writer_pretty(File::create(app.report_path)?, &*report.borrow())?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Validate,
    Check,
    Run,
}

fn action(mode: Mode, flags: &[String]) -> anyhow::Result<Action> {
    let flags: Vec<_> = flags.iter().map(String::as_str).collect();
    match (mode, flags.as_slice()) {
        (_, []) => Ok(Action::Validate),
        (Mode::Paper, ["--check-paper"]) | (Mode::Live, ["--check-live"]) => Ok(Action::Check),
        (Mode::Paper | Mode::Sandbox, ["--run"])
        | (Mode::Live, ["--run", "--live"] | ["--live", "--run"]) => Ok(Action::Run),
        _ => anyhow::bail!(
            "Invalid mode/flags: checks must match Paper/Live and cannot trade; Live requires --run --live"
        ),
    }
}

async fn query_snapshot(app: &AppConfig) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(app.mode != Mode::Sandbox, "Sandbox has no broker account");
    let exec = LongbridgeExecClientConfig {
        papertrading: app.mode == Mode::Paper,
        account_type: AccountType::Cash,
        outside_rth: false,
        ..Default::default()
    };
    tokio::time::timeout(Duration::from_secs(45), async {
        let (context, _receiver) = TradeContext::new(exec.sdk_config().await?);
        let balances = trade_api_call(context.account_balance(None)).await?;
        let positions = trade_api_call(context.stock_positions(None)).await?;
        let orders = trade_api_call(context.today_orders(GetTodayOrdersOptions::new())).await?;
        // 今日无委托不代表没有跨日 GTC 委托；沿用 Adapter 的今日+历史去重方式。
        // 历史接口有查询窗口，本诊断不把“没查到”宣称为冻结额已归因。
        let history =
            trade_api_call(context.history_orders(GetHistoryOrdersOptions::new())).await?;
        let today_count = orders.len();
        let history_count = history.len();
        let orders: Vec<_> = history
            .into_iter()
            .chain(orders)
            .map(|order| (order.order_id.clone(), order))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        let executions =
            trade_api_call(context.today_executions(GetTodayExecutionsOptions::new())).await?;
        let mut snapshot = broker_snapshot(app, &balances, &positions, &orders, executions.len())?;
        snapshot["today_orders"] = today_count.into();
        snapshot["history_order_records"] = history_count.into();
        snapshot["order_query_scope"] = "TODAY_PLUS_HISTORY_API_WINDOW".into();
        Ok(snapshot)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Broker preflight timed out; no orders submitted"))?
}

fn require_startup_ready(snapshot: &serde_json::Value) -> anyhow::Result<()> {
    let blockers = snapshot["startup_blockers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing broker startup diagnostics"))?;
    anyhow::ensure!(blockers.is_empty(), "Broker startup blocked: {blockers:?}");

    if let Some(warnings) = snapshot["startup_warnings"].as_array()
        && !warnings.is_empty()
    {
        eprintln!("Broker startup warnings: {warnings:?}");
    }
    Ok(())
}

fn broker_snapshot(
    app: &AppConfig,
    balances: &[AccountBalance],
    positions: &StockPositionsResponse,
    orders: &[Order],
    executions: usize,
) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(!balances.is_empty(), "Broker account returned no balances");
    let resuming = app.state_path.try_exists()?;
    let (native_balances, _) = parse_account_state(balances)?;
    let mut active_orders = 0;
    let mut outside_universe_orders = 0;
    let mut isolated_orders = 0;
    let mut pending_buy_notional = Decimal::ZERO;
    let mut active_order_details = Vec::new();

    for order in orders {
        let report = parse_order_status_report(order, app.account_id, None, 0.into())?;

        if !report.order_status.is_closed() {
            active_orders += 1;
            outside_universe_orders +=
                usize::from(!app.instruments.contains_key(&report.instrument_id));
            isolated_orders +=
                usize::from(app.isolated_instruments.contains(&report.instrument_id));
            if order.side == longbridge::trade::OrderSide::Buy
                && order.currency == app.currency.code.as_str()
            {
                pending_buy_notional += (order.quantity - order.executed_quantity)
                    .max(Decimal::ZERO)
                    * order.price.unwrap_or(Decimal::ZERO);
            }
            active_order_details.push(serde_json::json!({
                "instrument_id": report.instrument_id, "status": report.order_status,
                "quantity": order.quantity, "filled_quantity": order.executed_quantity,
            }));
        }
    }
    let mut position_records = 0;
    let mut nonflat_positions = 0;
    let mut outside_universe_positions = 0;
    let mut holdings = Vec::new();
    let mut isolated_positions = 0;
    let mut unsupported_isolation = false;
    let mut isolated_ids = BTreeSet::new();

    for position in positions
        .channels
        .iter()
        .flat_map(|channel| &channel.positions)
    {
        let report = parse_position_status_report(position, app.account_id, 0.into())?;
        position_records += 1;

        if !position.quantity.is_zero() {
            if app.isolated_instruments.contains(&report.instrument_id) {
                isolated_positions += 1;
                unsupported_isolation |= position.quantity < Decimal::ZERO
                    || position.currency != app.currency.code.as_str()
                    || !isolated_ids.insert(report.instrument_id);
            }
            holdings.push(serde_json::json!({
                "instrument_id": report.instrument_id,
                "quantity": position.quantity,
                "available_quantity": position.available_quantity,
                "currency": position.currency,
                "isolated": app.isolated_instruments.contains(&report.instrument_id),
            }));
            nonflat_positions += 1;
            outside_universe_positions +=
                usize::from(!app.instruments.contains_key(&report.instrument_id));
        }
    }
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if outside_universe_positions > 0 {
        blockers.push("OUTSIDE_UNIVERSE_POSITIONS");
    }

    if outside_universe_orders > 0 {
        blockers.push("OUTSIDE_UNIVERSE_ORDERS");
    }
    if isolated_orders > 0 {
        blockers.push("ORDERS_ON_ISOLATED_INSTRUMENT");
    }
    if unsupported_isolation {
        blockers.push("UNSUPPORTED_ISOLATED_POSITION");
    }

    // 标的属于配置不代表仓位属于策略；只读快照不能代替检查点与原生执行引擎的对账
    if !resuming && (nonflat_positions > isolated_positions || active_orders > 0) {
        blockers.push("BROKER_STATE_WITHOUT_CHECKPOINT");
    }
    let cash = native_balances
        .iter()
        .find(|balance| balance.currency == app.currency);
    let available = cash.map_or(Decimal::ZERO, |balance| balance.free.as_decimal());

    if cash.is_none() {
        blockers.push("MISSING_PORTFOLIO_CURRENCY");
    } else if !resuming && available <= Decimal::ZERO {
        blockers.push("NO_AVAILABLE_CASH");
    } else if !resuming && available < app.portfolio.capital {
        blockers.push("INSUFFICIENT_STARTING_CASH");
    }
    let mut frozen_cash_present = false;
    let mut settling_cash_present = false;
    let mut frozen = Decimal::ZERO;
    let mut settling = Decimal::ZERO;
    let frozen_fees: Decimal = balances
        .iter()
        .flat_map(|balance| &balance.frozen_transaction_fees)
        .filter(|fee| fee.currency == app.currency.code.as_str())
        .map(|fee| fee.frozen_transaction_fee)
        .sum();

    for cash in balances
        .iter()
        .flat_map(|balance| &balance.cash_infos)
        .filter(|cash| cash.currency == app.currency.code.as_str())
    {
        frozen_cash_present |= !cash.frozen_cash.is_zero();
        settling_cash_present |= !cash.settling_cash.is_zero();
        frozen += cash.frozen_cash;
        settling += cash.settling_cash;
    }

    if !resuming && frozen_cash_present {
        // 仅豁免模拟盘的归因门禁；冻结额不释放，现金预算、订单及持仓核对保持不变
        if app.mode == Mode::Paper && app.paper_allow_unattributed_frozen_cash {
            warnings.push("PAPER_UNATTRIBUTED_FROZEN_CASH_WAIVED");
        } else {
            blockers.push("UNATTRIBUTED_FROZEN_CASH");
        }
    }

    // 金额和持仓用于操作员核对；不输出账户编号、渠道编号或凭证。
    Ok(serde_json::json!({
        "account_records": balances.len(), "position_records": position_records,
        "nonflat_positions": nonflat_positions, "today_orders": orders.len(),
        "holdings": holdings,
        "active_orders": active_orders, "today_executions": executions,
        "active_order_details": active_order_details,
        "isolated_positions": isolated_positions,
        "isolated_instruments": app.isolated_instruments,
        "native_snapshot_parsing": "PASS",
        "fresh_account_candidate": !resuming && nonflat_positions == 0 && active_orders == 0 && !frozen_cash_present && blockers.is_empty(),
        "outside_universe_positions": outside_universe_positions,
        "outside_universe_orders": outside_universe_orders,
        "startup_blockers": blockers,
        "startup_warnings": warnings,
        "startup_reconciliation": "NOT_RUN",
        "cash_check": {
            "currency": app.currency.to_string(),
            "currency_present": cash.is_some(),
            "positive_available_cash": available > Decimal::ZERO,
            "configured_capital_covered_by_free_cash": available >= app.portfolio.capital,
            "frozen_cash_present": frozen_cash_present,
            "settling_cash_present": settling_cash_present,
            "available_cash": available,
            "frozen_cash": frozen,
            "settling_cash": settling,
            "frozen_transaction_fees": frozen_fees,
            "frozen_cash_matches_reported_fees": frozen > Decimal::ZERO && frozen == frozen_fees,
            "pending_buy_limit_notional": pending_buy_notional,
            "freeze_attribution": if frozen.is_zero() { "NONE" } else { "UNVERIFIED" },
            // 汇总冻结额没有订单归属和原子快照证据；本检查不验证或释放本地预留
            "reservation_overlap_verification": "NOT_PERFORMED",
        },
    }))
}

#[cfg(test)]
mod tests {
    use longbridge::{
        Market,
        trade::{CashInfo, StockPosition, StockPositionChannel},
    };
    use rust_decimal::Decimal;

    use super::*;

    fn app() -> AppConfig {
        let mut app = AppConfig::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/dynamic_grid_paper.json"),
        )
        .unwrap();
        app.state_path = std::env::temp_dir().join(format!(
            "grid-preflight-{}.json",
            nautilus_core::UUID4::new()
        ));
        app
    }

    fn balances() -> Vec<AccountBalance> {
        vec![AccountBalance {
            total_cash: Decimal::from(100_000),
            max_finance_amount: Decimal::ZERO,
            remaining_finance_amount: Decimal::ZERO,
            risk_level: 0,
            margin_call: Decimal::ZERO,
            currency: "USD".into(),
            cash_infos: vec![CashInfo {
                withdraw_cash: Decimal::from(100_000),
                available_cash: Decimal::from(100_000),
                frozen_cash: Decimal::ZERO,
                settling_cash: Decimal::ZERO,
                currency: "USD".into(),
            }],
            net_assets: Decimal::from(100_000),
            init_margin: Decimal::ZERO,
            maintenance_margin: Decimal::ZERO,
            buy_power: Decimal::from(100_000),
            frozen_transaction_fees: Vec::new(),
        }]
    }

    #[rstest::rstest]
    #[case("F.US", 1, vec!["OUTSIDE_UNIVERSE_POSITIONS", "BROKER_STATE_WITHOUT_CHECKPOINT"])]
    #[case("AAPL.US", 0, vec!["BROKER_STATE_WITHOUT_CHECKPOINT"])]
    fn paper_preflight_reports_existing_inventory_without_checkpoint(
        #[case] symbol: &str,
        #[case] outside: usize,
        #[case] blockers: Vec<&str>,
    ) {
        let positions = StockPositionsResponse {
            channels: vec![StockPositionChannel {
                account_channel: "paper".into(),
                positions: vec![StockPosition {
                    symbol: symbol.into(),
                    symbol_name: symbol.into(),
                    quantity: Decimal::ONE,
                    available_quantity: Decimal::ONE,
                    currency: "USD".into(),
                    cost_price: Decimal::from(12),
                    market: Market::US,
                    init_quantity: Some(Decimal::ONE),
                }],
            }],
        };
        let result = broker_snapshot(&app(), &balances(), &positions, &[], 0).unwrap();
        assert_eq!(result["fresh_account_candidate"], false);
        assert_eq!(result["outside_universe_positions"], outside);
        assert_eq!(result["startup_blockers"], serde_json::json!(blockers));
    }

    #[rstest::rstest]
    #[case(60_000, 0)]
    #[case(0, 60_000)]
    fn paper_preflight_never_credits_frozen_or_settling_cash(
        #[case] frozen: i64,
        #[case] settling: i64,
    ) {
        let mut balances = balances();
        let cash = &mut balances[0].cash_infos[0];
        cash.available_cash = Decimal::from(40_000);
        cash.frozen_cash = Decimal::from(frozen);
        cash.settling_cash = Decimal::from(settling);
        let positions = StockPositionsResponse {
            channels: Vec::new(),
        };
        let result = broker_snapshot(&app(), &balances, &positions, &[], 0).unwrap();
        assert_eq!(
            result["cash_check"]["configured_capital_covered_by_free_cash"],
            false
        );
        assert_eq!(result["cash_check"]["frozen_cash_present"], frozen != 0);
        assert_eq!(result["cash_check"]["settling_cash_present"], settling != 0);
        assert_eq!(
            result["cash_check"]["reservation_overlap_verification"],
            "NOT_PERFORMED"
        );
    }

    #[rstest::rstest]
    #[case("HKD", "MISSING_PORTFOLIO_CURRENCY")]
    #[case("USD", "NO_AVAILABLE_CASH")]
    fn paper_preflight_needs_usable_portfolio_currency(
        #[case] currency: &str,
        #[case] blocker: &str,
    ) {
        let mut balances = balances();
        let cash = &mut balances[0].cash_infos[0];
        cash.currency = currency.into();
        if currency == "USD" {
            cash.available_cash = Decimal::ZERO;
            cash.settling_cash = Decimal::from(100_000);
        }
        let positions = StockPositionsResponse {
            channels: Vec::new(),
        };
        let result = broker_snapshot(&app(), &balances, &positions, &[], 0).unwrap();
        assert_eq!(result["fresh_account_candidate"], false);
        assert_eq!(result["startup_blockers"], serde_json::json!([blocker]));
        assert_eq!(
            result["cash_check"]["configured_capital_covered_by_free_cash"],
            false
        );
    }

    #[rstest::rstest]
    #[case(longbridge::trade::OrderStatus::PartialFilled)]
    #[case(longbridge::trade::OrderStatus::PendingCancel)]
    fn paper_preflight_keeps_nonterminal_orders_blocked(
        #[case] status: longbridge::trade::OrderStatus,
    ) {
        let mut order: Order =
            serde_json::from_str(include_str!("../test_data/order_reconciliation.json")).unwrap();
        order.status = status;
        let positions = StockPositionsResponse {
            channels: Vec::new(),
        };
        let mut app = app();
        let result = broker_snapshot(
            &app,
            &balances(),
            &positions,
            std::slice::from_ref(&order),
            0,
        )
        .unwrap();
        assert_eq!(result["active_orders"], 1);
        assert_eq!(result["fresh_account_candidate"], false);
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["BROKER_STATE_WITHOUT_CHECKPOINT"])
        );

        // 仅存在一个文件不能证明检查点有效，更不能消除订单的对账义务
        app.state_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        order.symbol = "F.US".into();
        let result = broker_snapshot(&app, &balances(), &positions, &[order], 0).unwrap();
        assert_eq!(result["outside_universe_orders"], 1);
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["OUTSIDE_UNIVERSE_ORDERS"])
        );
        assert_eq!(result["startup_reconciliation"], "NOT_RUN");
        assert_eq!(result["fresh_account_candidate"], false);
    }

    #[rstest::rstest]
    fn paper_preflight_empty_account_is_only_a_candidate() {
        let positions = StockPositionsResponse {
            channels: Vec::new(),
        };
        let result = broker_snapshot(&app(), &balances(), &positions, &[], 0).unwrap();
        assert_eq!(result["fresh_account_candidate"], true);
        assert_eq!(result["startup_blockers"], serde_json::json!([]));
        assert_eq!(result["startup_reconciliation"], "NOT_RUN");
        assert_eq!(
            result["cash_check"]["configured_capital_covered_by_free_cash"],
            true
        );
        assert!(broker_snapshot(&app(), &[], &positions, &[], 0).is_err());
    }

    #[rstest::rstest]
    fn isolation_does_not_claim_inventory_or_release_frozen_cash() {
        let mut app = app();
        let id = "AAPL.US.LONGBRIDGE".parse().unwrap();
        app.isolated_instruments.insert(id);
        let positions = StockPositionsResponse {
            channels: vec![StockPositionChannel {
                account_channel: "test".into(),
                positions: vec![StockPosition {
                    symbol: "AAPL.US".into(),
                    symbol_name: "AAPL".into(),
                    quantity: Decimal::from(20),
                    available_quantity: Decimal::from(20),
                    currency: "USD".into(),
                    cost_price: Decimal::from(100),
                    market: Market::US,
                    init_quantity: None,
                }],
            }],
        };
        let mut balances = balances();
        let result = broker_snapshot(&app, &balances, &positions, &[], 0).unwrap();
        require_startup_ready(&result).unwrap();
        assert_eq!(result["isolated_positions"], 1);
        assert!(app.strategy().unwrap().base.external_order_claims.is_none());
        balances[0].cash_infos[0].frozen_cash = Decimal::from(10386);
        balances[0]
            .frozen_transaction_fees
            .push(longbridge::trade::FrozenTransactionFee {
                currency: "USD".into(),
                frozen_transaction_fee: Decimal::from(4),
            });
        let result = broker_snapshot(&app, &balances, &positions, &[], 0).unwrap();
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["UNATTRIBUTED_FROZEN_CASH"])
        );
        assert_eq!(result["cash_check"]["available_cash"], "100000.00");
        assert_eq!(result["cash_check"]["frozen_cash"], "10386");
        assert_eq!(result["cash_check"]["frozen_transaction_fees"], "4");
        assert_eq!(result["cash_check"]["freeze_attribution"], "UNVERIFIED");
        assert_eq!(
            result["cash_check"]["frozen_cash_matches_reported_fees"],
            false
        );
        assert!(require_startup_ready(&result).is_err());

        let mut duplicate = positions.clone();
        duplicate.channels.push(positions.channels[0].clone());
        let result = broker_snapshot(&app, &balances, &duplicate, &[], 0).unwrap();
        assert!(
            result["startup_blockers"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("UNSUPPORTED_ISOLATED_POSITION"))
        );
        let mut short = positions;
        short.channels[0].positions[0].quantity = Decimal::NEGATIVE_ONE;
        let result = broker_snapshot(&app, &balances, &short, &[], 0).unwrap();
        assert!(
            result["startup_blockers"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("UNSUPPORTED_ISOLATED_POSITION"))
        );
    }

    #[rstest::rstest]
    fn isolation_does_not_allow_manual_orders_even_with_a_checkpoint() {
        let mut app = app();
        app.isolated_instruments
            .insert("AAPL.US.LONGBRIDGE".parse().unwrap());
        app.state_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut order: Order =
            serde_json::from_str(include_str!("../test_data/order_reconciliation.json")).unwrap();
        order.status = longbridge::trade::OrderStatus::PendingCancel;
        order.symbol = "AAPL.US".into();
        let result = broker_snapshot(
            &app,
            &balances(),
            &StockPositionsResponse { channels: vec![] },
            &[order],
            0,
        )
        .unwrap();
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["ORDERS_ON_ISOLATED_INSTRUMENT"])
        );
        assert!(require_startup_ready(&result).is_err());
    }

    #[rstest::rstest]
    fn fresh_start_requires_funded_budget_without_unattributed_locks() {
        let positions = StockPositionsResponse {
            channels: Vec::new(),
        };
        let app = app();
        let mut balances = balances();
        balances[0].cash_infos[0].available_cash = app.portfolio.capital - Decimal::ONE;
        let result = broker_snapshot(&app, &balances, &positions, &[], 0).unwrap();
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["INSUFFICIENT_STARTING_CASH"])
        );

        balances[0].cash_infos[0].available_cash = app.portfolio.capital;
        balances[0].cash_infos[0].frozen_cash = Decimal::ONE;
        let result = broker_snapshot(&app, &balances, &positions, &[], 0).unwrap();
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["UNATTRIBUTED_FROZEN_CASH"])
        );
    }

    #[rstest::rstest]
    #[case(Mode::Paper, false, false)]
    #[case(Mode::Paper, true, true)]
    #[case(Mode::Live, false, false)]
    #[case(Mode::Live, true, false)]
    fn frozen_cash_waiver_is_explicit_and_paper_only(
        #[case] mode: Mode,
        #[case] opted_in: bool,
        #[case] allowed: bool,
    ) {
        let mut app = app();
        assert!(!app.paper_allow_unattributed_frozen_cash);
        app.mode = mode;
        app.paper_allow_unattributed_frozen_cash = opted_in;
        assert_eq!(app.strategy().is_ok(), !opted_in || mode == Mode::Paper);
        let mut balances = balances();
        balances[0].cash_infos[0].frozen_cash = Decimal::from(10_386);
        let result = broker_snapshot(
            &app,
            &balances,
            &StockPositionsResponse { channels: vec![] },
            &[],
            0,
        )
        .unwrap();
        assert_eq!(require_startup_ready(&result).is_ok(), allowed);
        assert_eq!(
            result["startup_blockers"],
            if allowed {
                serde_json::json!([])
            } else {
                serde_json::json!(["UNATTRIBUTED_FROZEN_CASH"])
            }
        );
        assert_eq!(
            result["startup_warnings"],
            if allowed {
                serde_json::json!(["PAPER_UNATTRIBUTED_FROZEN_CASH_WAIVED"])
            } else {
                serde_json::json!([])
            }
        );
        assert_eq!(result["cash_check"]["available_cash"], "100000.00");
        assert_eq!(result["cash_check"]["frozen_cash"], "10386");
        assert_eq!(result["cash_check"]["freeze_attribution"], "UNVERIFIED");
        assert_eq!(result["fresh_account_candidate"], false);
    }

    #[rstest::rstest]
    fn paper_frozen_cash_waiver_keeps_cash_and_order_gates() {
        let mut app = app();
        app.paper_allow_unattributed_frozen_cash = true;
        let positions = StockPositionsResponse { channels: vec![] };
        let mut balances = balances();
        balances[0].cash_infos[0].frozen_cash = Decimal::from(100_000);
        balances[0].cash_infos[0].available_cash = app.portfolio.capital - Decimal::ONE;
        let result = broker_snapshot(&app, &balances, &positions, &[], 0).unwrap();
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["INSUFFICIENT_STARTING_CASH"])
        );
        assert!(require_startup_ready(&result).is_err());

        balances[0].cash_infos[0].available_cash = app.portfolio.capital;
        let mut order: Order =
            serde_json::from_str(include_str!("../test_data/order_reconciliation.json")).unwrap();
        order.status = longbridge::trade::OrderStatus::PendingCancel;
        let result = broker_snapshot(&app, &balances, &positions, &[order], 0).unwrap();
        assert_eq!(
            result["startup_blockers"],
            serde_json::json!(["BROKER_STATE_WITHOUT_CHECKPOINT"])
        );
        assert!(require_startup_ready(&result).is_err());
    }

    #[rstest::rstest]
    fn resume_does_not_require_funding_the_original_budget_again() {
        let positions = StockPositionsResponse {
            channels: Vec::new(),
        };
        let mut app = app();
        // 这里只测现金门禁；真实 --run 必须先由策略校验检查点内容。
        app.state_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut balances = balances();
        balances[0].cash_infos[0].available_cash = Decimal::ZERO;
        balances[0].cash_infos[0].frozen_cash = Decimal::from(100_000);
        let result = broker_snapshot(&app, &balances, &positions, &[], 0).unwrap();
        assert_eq!(result["startup_blockers"], serde_json::json!([]));
        assert_eq!(result["cash_check"]["positive_available_cash"], false);
        assert_eq!(result["fresh_account_candidate"], false);
        assert_eq!(result["startup_reconciliation"], "NOT_RUN");
        require_startup_ready(&result).unwrap();
    }

    #[rstest::rstest]
    fn startup_gate_fails_closed() {
        assert!(require_startup_ready(&serde_json::json!({})).is_err());
        for blocker in [
            "BROKER_STATE_WITHOUT_CHECKPOINT",
            "OUTSIDE_UNIVERSE_POSITIONS",
            "OUTSIDE_UNIVERSE_ORDERS",
            "MISSING_PORTFOLIO_CURRENCY",
            "NO_AVAILABLE_CASH",
            "INSUFFICIENT_STARTING_CASH",
            "UNATTRIBUTED_FROZEN_CASH",
        ] {
            assert!(
                require_startup_ready(&serde_json::json!({"startup_blockers": [blocker]})).is_err()
            );
        }
    }

    #[rstest::rstest]
    fn routing_requires_unambiguous_mode_and_flags() {
        for mode in [Mode::Sandbox, Mode::Paper, Mode::Live] {
            assert_eq!(action(mode, &[]).unwrap(), Action::Validate);
            for (flags, expected) in [
                (
                    vec!["--check-paper"],
                    (mode == Mode::Paper).then_some(Action::Check),
                ),
                (
                    vec!["--check-live"],
                    (mode == Mode::Live).then_some(Action::Check),
                ),
                (vec!["--run"], (mode != Mode::Live).then_some(Action::Run)),
                (
                    vec!["--run", "--live"],
                    (mode == Mode::Live).then_some(Action::Run),
                ),
                (vec!["--live"], None),
                (vec!["--check-paper", "--run"], None),
                (vec!["--check-live", "--run", "--live"], None),
                (vec!["--run", "--run"], None),
                (vec!["--unknown"], None),
            ] {
                let flags: Vec<_> = flags.into_iter().map(String::from).collect();
                assert_eq!(action(mode, &flags).ok(), expected, "{mode:?} {flags:?}");
            }
        }
    }
}
