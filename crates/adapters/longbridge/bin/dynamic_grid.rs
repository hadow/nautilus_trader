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

//! Validate by default; --check-paper queries only; --run starts trading with explicit live routing.

mod dynamic_grid_config;

use std::{fs::File, path::Path, time::Duration};

use dynamic_grid_config::{AppConfig, Mode};
use longbridge::{
    TradeContext,
    trade::{GetTodayExecutionsOptions, GetTodayOrdersOptions},
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        !args.is_empty()
            && args
                .iter()
                .skip(1)
                .all(|a| matches!(a.as_str(), "--run" | "--live" | "--check-paper")),
        "Usage: longbridge-dynamic-grid CONFIG.json [--check-paper | --run [--live]]"
    );
    let app = AppConfig::load(Path::new(&args[0]))?;
    let strategy_config = app.strategy()?;
    let strategy_id = strategy_config
        .base
        .strategy_id
        .ok_or_else(|| anyhow::anyhow!("Missing strategy identity"))?;
    if args.iter().any(|a| a == "--check-paper") {
        anyhow::ensure!(
            app.mode == Mode::Paper && args.len() == 2,
            "--check-paper requires mode=Paper and cannot be combined with trading flags"
        );
        let exec = LongbridgeExecClientConfig {
            papertrading: true,
            account_type: AccountType::Cash,
            outside_rth: false,
            ..Default::default()
        };
        let snapshot = tokio::time::timeout(Duration::from_secs(45), async {
            let (context, _receiver) = TradeContext::new(exec.sdk_config().await?);
            let balances = trade_api_call(context.account_balance(None)).await?;
            anyhow::ensure!(!balances.is_empty(), "Paper account returned no balances");
            parse_account_state(&balances)?;
            let positions = trade_api_call(context.stock_positions(None)).await?;
            let orders = trade_api_call(context.today_orders(GetTodayOrdersOptions::new())).await?;
            let executions =
                trade_api_call(context.today_executions(GetTodayExecutionsOptions::new())).await?;
            let mut active_orders = 0;
            for order in &orders {
                let report = parse_order_status_report(order, app.account_id, None, 0.into())?;
                active_orders += usize::from(!report.order_status.is_closed());
            }
            let mut position_records = 0;
            let mut nonflat_positions = 0;
            for position in positions
                .channels
                .iter()
                .flat_map(|channel| &channel.positions)
            {
                parse_position_status_report(position, app.account_id, 0.into())?;
                position_records += 1;
                nonflat_positions += usize::from(!position.quantity.is_zero());
            }
            // 不输出余额、账户编号或持仓明细；只报告门禁结果，不把连通性称为成交验收
            Ok::<_, anyhow::Error>(serde_json::json!({
                "account_records": balances.len(), "position_records": position_records,
                "nonflat_positions": nonflat_positions, "today_orders": orders.len(),
                "active_orders": active_orders, "today_executions": executions.len(),
                "native_snapshot_parsing": "PASS",
                "fresh_account_candidate": nonflat_positions == 0 && active_orders == 0,
            }))
        })
        .await
        .map_err(|_| anyhow::anyhow!("Paper read-only check timed out; no orders submitted"))??;
        println!(
            "{}",
            serde_json::json!({
                "mode": "Paper", "read_only": true, "snapshot": snapshot,
                "checkpoint_exists": app.state_path.exists(),
                "broker_fill_acceptance": "NOT_RUN",
                "timeout_reconciliation_acceptance": "NOT_RUN",
                "note": "Connectivity only; startup reconciliation and execution are not validated"
            })
        );
        return Ok(());
    }
    if !args.iter().any(|a| a == "--run") {
        println!("{}", serde_json::to_string_pretty(&strategy_config)?);
        return Ok(());
    }
    anyhow::ensure!(
        (app.mode == Mode::Live) == args.iter().any(|a| a == "--live"),
        "Live routing requires both mode=Live and --live; omit --live for simulation"
    );
    let strategy = MultiAssetGridStrategy::new(strategy_config)?;
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
