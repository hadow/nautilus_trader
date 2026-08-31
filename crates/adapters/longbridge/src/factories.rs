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

//! Factories for Longbridge data and execution clients.

use std::{any::Any, cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::CacheView,
    clients::{DataClient, ExecutionClient},
    clock::Clock,
    factories::{ClientConfig, DataClientFactory, ExecutionClientFactory},
};
use nautilus_live::ExecutionClientCore;
use nautilus_model::{
    enums::{AccountType, OmsType},
    identifiers::{AccountId, ClientId, TraderId},
};

use crate::{
    common::consts::{LONGBRIDGE, LONGBRIDGE_VENUE},
    config::{LongbridgeDataClientConfig, LongbridgeExecClientConfig},
    data::LongbridgeDataClient,
    execution::LongbridgeExecutionClient,
};

impl ClientConfig for LongbridgeDataClientConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ClientConfig for LongbridgeExecClientConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Factory for Longbridge data clients.
#[derive(Debug, Clone, Default)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_trader.adapters.longbridge", from_py_object)
)]
#[cfg_attr(
    feature = "python",
    pyo3_stub_gen::derive::gen_stub_pyclass(module = "nautilus_trader.adapters.longbridge")
)]
pub struct LongbridgeDataClientFactory;

impl LongbridgeDataClientFactory {
    /// Creates a new factory.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DataClientFactory for LongbridgeDataClientFactory {
    fn create(
        &self,
        name: &str,
        config: &dyn ClientConfig,
        _cache: CacheView,
        _clock: Rc<RefCell<dyn Clock>>,
    ) -> anyhow::Result<Box<dyn DataClient>> {
        let config = config
            .as_any()
            .downcast_ref::<LongbridgeDataClientConfig>()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid config for LongbridgeDataClientFactory: expected LongbridgeDataClientConfig, was {config:?}",
                )
            })?
            .clone();
        config.validate()?;
        Ok(Box::new(LongbridgeDataClient::new(
            ClientId::from(name),
            config,
        )))
    }

    fn name(&self) -> &'static str {
        LONGBRIDGE
    }

    fn config_type(&self) -> &'static str {
        "LongbridgeDataClientConfig"
    }
}

/// Factory for Longbridge execution clients.
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_trader.adapters.longbridge", from_py_object)
)]
#[cfg_attr(
    feature = "python",
    pyo3_stub_gen::derive::gen_stub_pyclass(module = "nautilus_trader.adapters.longbridge")
)]
pub struct LongbridgeExecutionClientFactory {
    trader_id: TraderId,
    account_id: AccountId,
}

impl LongbridgeExecutionClientFactory {
    /// Creates a new factory.
    #[must_use]
    pub const fn new(trader_id: TraderId, account_id: AccountId) -> Self {
        Self {
            trader_id,
            account_id,
        }
    }
}

impl ExecutionClientFactory for LongbridgeExecutionClientFactory {
    fn create(
        &self,
        name: &str,
        config: &dyn ClientConfig,
        cache: CacheView,
    ) -> anyhow::Result<Box<dyn ExecutionClient>> {
        let config = config
            .as_any()
            .downcast_ref::<LongbridgeExecClientConfig>()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid config for LongbridgeExecutionClientFactory: expected LongbridgeExecClientConfig, was {config:?}",
                )
            })?
            .clone();

        if !matches!(config.account_type, AccountType::Cash | AccountType::Margin) {
            anyhow::bail!(
                "Longbridge supports CASH or MARGIN accounts, was {:?}",
                config.account_type,
            );
        }
        let core = ExecutionClientCore::new(
            self.trader_id,
            ClientId::from(name),
            *LONGBRIDGE_VENUE,
            OmsType::Netting,
            self.account_id,
            config.account_type,
            None,
            cache,
        );
        Ok(Box::new(LongbridgeExecutionClient::new(core, config)))
    }

    fn name(&self) -> &'static str {
        LONGBRIDGE
    }

    fn config_type(&self) -> &'static str {
        "LongbridgeExecClientConfig"
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use nautilus_common::{
        cache::Cache,
        clock::TestClock,
        factories::{DataClientFactory, ExecutionClientFactory},
        live::runner::{replace_exec_event_sender, set_data_event_sender},
        messages::{DataEvent, ExecutionEvent},
    };
    use rstest::rstest;

    use super::*;

    fn setup_senders() {
        let (data_tx, _data_rx) = tokio::sync::mpsc::unbounded_channel::<DataEvent>();
        set_data_event_sender(data_tx);
        let (exec_tx, _exec_rx) = tokio::sync::mpsc::unbounded_channel::<ExecutionEvent>();
        replace_exec_event_sender(exec_tx);
    }

    #[rstest]
    fn test_data_factory_creates_client_without_network_io() {
        setup_senders();
        let factory = LongbridgeDataClientFactory::new();
        let cache = Rc::new(RefCell::new(Cache::default()));
        let clock = Rc::new(RefCell::new(TestClock::new()));
        let client = factory
            .create(
                "LONGBRIDGE-DATA",
                &LongbridgeDataClientConfig::default(),
                cache.into(),
                clock,
            )
            .unwrap();
        assert_eq!(client.client_id(), ClientId::from("LONGBRIDGE-DATA"));
        assert_eq!(factory.name(), LONGBRIDGE);
    }

    #[rstest]
    fn test_execution_factory_creates_client_without_network_io() {
        setup_senders();
        let trader_id = TraderId::from("TRADER-001");
        let account_id = AccountId::from("LONGBRIDGE-001");
        let factory = LongbridgeExecutionClientFactory::new(trader_id, account_id);
        let cache = Rc::new(RefCell::new(Cache::default()));
        let client = factory
            .create(
                "LONGBRIDGE-EXEC",
                &LongbridgeExecClientConfig::default(),
                cache.into(),
            )
            .unwrap();
        assert_eq!(client.account_id(), account_id);
        assert_eq!(client.oms_type(), OmsType::Netting);
    }
}
