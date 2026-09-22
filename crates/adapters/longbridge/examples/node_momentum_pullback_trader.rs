// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
// -------------------------------------------------------------------------------------------------

//! LongBridge paper/live node for `MomentumPullbackStrategy`.
//!
//! Paper trading is the safe default. Live routing requires both `papertrading = false` in the
//! configuration and the explicit `--live` command-line flag.

use std::{collections::HashMap, env, fs, path::Path};

use anyhow::Context;
use nautilus_common::enums::Environment;
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory, common::rate_limit::MAX_QUOTE_SUBSCRIPTION_SYMBOLS,
};
use nautilus_model::{
    enums::{AccountType, OmsType},
    identifiers::{AccountId, InstrumentId, StrategyId, TraderId},
};
use nautilus_trading::{
    examples::strategies::{MomentumPullbackConfig, MomentumPullbackStrategy},
    strategy::StrategyConfig,
};
use rust_decimal::Decimal;
use serde::Deserialize;

const DEFAULT_CONFIG_PATH: &str = "crates/adapters/longbridge/examples/momentum_pullback_live.toml";
const STRATEGY_ID: &str = "MOMENTUM-PULLBACK-001";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppConfig {
    trader_id: String,
    account_id: String,
    node_name: String,
    papertrading: bool,
    max_notional_per_order: Decimal,
    instrument_price_increments: HashMap<String, String>,
    strategy: MomentumPullbackConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            trader_id: "MOMENTUM-PULLBACK-001".to_string(),
            account_id: "LONGBRIDGE-001".to_string(),
            node_name: "LONGBRIDGE-MOMENTUM-PULLBACK-001".to_string(),
            papertrading: true,
            max_notional_per_order: Decimal::from(25_000),
            instrument_price_increments: HashMap::new(),
            strategy: MomentumPullbackConfig::default(),
        }
    }
}

impl AppConfig {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading live config {}", path.display()))?;
        let mut config: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid live config {}", path.display()))?;
        config.finalize()?;
        Ok(config)
    }

    fn finalize(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_notional_per_order > Decimal::ZERO,
            "max_notional_per_order must be positive",
        );
        let ids = self.instrument_ids();
        anyhow::ensure!(
            ids.len() <= MAX_QUOTE_SUBSCRIPTION_SYMBOLS,
            "LongBridge supports at most {MAX_QUOTE_SUBSCRIPTION_SYMBOLS} unique subscriptions; got {}",
            ids.len(),
        );
        anyhow::ensure!(
            self.strategy.historical_warmup_bars <= 1_000,
            "LongBridge historical_warmup_bars cannot exceed 1000",
        );
        for id in &ids {
            anyhow::ensure!(
                id.venue.as_str() == "LONGBRIDGE",
                "live instrument {id} must use venue LONGBRIDGE",
            );
            anyhow::ensure!(
                self.instrument_price_increments
                    .contains_key(&id.to_string()),
                "missing exact price increment for {id}",
            );
        }
        self.data_config().validate()?;
        self.strategy.bars_are_final = false;
        self.strategy.protective_stop_uses_market_if_touched = true;
        self.strategy.base = StrategyConfig {
            strategy_id: Some(StrategyId::from(STRATEGY_ID)),
            order_id_tag: Some("502".to_string()),
            oms_type: Some(OmsType::Netting),
            external_order_claims: Some(self.strategy.universe.clone()),
            ..self.strategy.base.clone()
        };
        self.strategy.validate()
    }

    fn instrument_ids(&self) -> Vec<InstrumentId> {
        let mut ids = self.strategy.universe.clone();
        ids.extend([
            self.strategy.market_regime_instrument_id,
            self.strategy.secondary_market_instrument_id,
            self.strategy.relative_strength_instrument_id,
        ]);
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            instrument_price_increments: self.instrument_price_increments.clone(),
            ..Default::default()
        }
    }

    fn execution_config(&self) -> LongbridgeExecClientConfig {
        LongbridgeExecClientConfig {
            account_type: AccountType::Margin,
            papertrading: self.papertrading,
            outside_rth: false,
            ..Default::default()
        }
    }

    fn risk_config(&self) -> LiveRiskEngineConfig {
        LiveRiskEngineConfig {
            bypass: false,
            max_order_submit_rate: "6/00:00:01".to_string(),
            max_order_modify_rate: "6/00:00:01".to_string(),
            max_notional_per_order: self
                .strategy
                .universe
                .iter()
                .map(|id| (id.to_string(), self.max_notional_per_order.to_string()))
                .collect(),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let live_acknowledged = args.iter().any(|arg| arg == "--live");
    let config_path = args
        .iter()
        .find(|arg| arg.as_str() != "--live")
        .map_or_else(|| Path::new(DEFAULT_CONFIG_PATH), Path::new);
    let config = AppConfig::load(config_path)?;
    anyhow::ensure!(
        config.papertrading || live_acknowledged,
        "papertrading=false routes real capital; rerun with --live after reviewing the configuration",
    );
    anyhow::ensure!(
        !config.papertrading || !live_acknowledged,
        "--live was supplied while papertrading=true; choose one routing mode explicitly",
    );

    let environment = if config.papertrading {
        Environment::Sandbox
    } else {
        Environment::Live
    };
    let trader_id = TraderId::from(config.trader_id.as_str());
    let account_id = AccountId::from(config.account_id.as_str());
    let instrument_ids = config.instrument_ids();
    let exec_config = LiveExecEngineConfig {
        reconciliation_lookback_mins: Some(60 * 24 * 60),
        reconciliation_instrument_ids: Some(
            instrument_ids.iter().map(ToString::to_string).collect(),
        ),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(30.0),
        ..Default::default()
    };
    let mut node = LiveNode::builder(trader_id, environment)?
        .with_name(config.node_name.clone())
        .with_load_state(false)
        .with_save_state(false)
        .with_exec_engine_config(exec_config)
        .with_risk_engine_config(config.risk_config())
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
            Box::new(config.execution_config()),
        )?
        .build()?;
    node.add_strategy(MomentumPullbackStrategy::new(config.strategy)?)?;
    node.run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_configuration_enforces_adapter_compatible_strategy_settings() {
        let mut config = AppConfig::default();
        config.strategy.universe = vec![InstrumentId::from("AAPL.US.LONGBRIDGE")];
        config.strategy.market_regime_instrument_id = InstrumentId::from("SPY.US.LONGBRIDGE");
        config.strategy.secondary_market_instrument_id = InstrumentId::from("QQQ.US.LONGBRIDGE");
        config.strategy.relative_strength_instrument_id = InstrumentId::from("SPY.US.LONGBRIDGE");
        config.instrument_price_increments = [
            ("AAPL.US.LONGBRIDGE".to_string(), "0.01".to_string()),
            ("SPY.US.LONGBRIDGE".to_string(), "0.01".to_string()),
            ("QQQ.US.LONGBRIDGE".to_string(), "0.01".to_string()),
        ]
        .into_iter()
        .collect();

        config.finalize().unwrap();

        assert!(!config.strategy.bars_are_final);
        assert!(config.strategy.protective_stop_uses_market_if_touched);
        assert!(config.execution_config().papertrading);
        assert!(!config.risk_config().bypass);
    }

    #[test]
    fn example_configuration_parses_and_defaults_to_paper() {
        let mut config: AppConfig =
            toml::from_str(include_str!("momentum_pullback_live.toml")).unwrap();

        config.finalize().unwrap();

        assert!(config.papertrading);
        assert_eq!(config.strategy.universe.len(), 3);
    }
}
