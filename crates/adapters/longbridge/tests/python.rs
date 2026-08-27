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

#![cfg(feature = "python")]

use nautilus_longbridge::{
    config::{DEFAULT_OAUTH_CALLBACK_PORT, LongbridgeDataClientConfig, LongbridgeExecClientConfig},
    factories::{LongbridgeDataClientFactory, LongbridgeExecutionClientFactory},
    python,
};
use nautilus_model::identifiers::{AccountId, TraderId};
use pyo3::{
    Py, Python,
    types::{PyDict, PyModule},
};
use rstest::rstest;

#[rstest]
fn test_python_module_registers_configs_and_factories() {
    Python::initialize();
    Python::attach(|py| {
        let module = PyModule::new(py, "longbridge").unwrap();
        python::longbridge(py, &module).unwrap();
        assert!(Py::new(py, LongbridgeDataClientConfig::default()).is_ok());
        assert!(Py::new(py, LongbridgeExecClientConfig::default()).is_ok());
        assert!(Py::new(py, LongbridgeDataClientFactory::new()).is_ok());
        assert!(
            Py::new(
                py,
                LongbridgeExecutionClientFactory::new(
                    TraderId::from("TRADER-001"),
                    AccountId::from("LONGBRIDGE-001"),
                ),
            )
            .is_ok(),
        );

        let kwargs = PyDict::new(py);
        kwargs
            .set_item("oauth_client_id", "public-client-id")
            .unwrap();
        kwargs.set_item("oauth_callback_port", 60_400).unwrap();
        let config = module
            .getattr("LongbridgeDataClientConfig")
            .unwrap()
            .call((), Some(&kwargs))
            .unwrap();
        assert_eq!(
            config
                .getattr("oauth_client_id")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "public-client-id",
        );
        assert_eq!(
            config
                .getattr("oauth_callback_port")
                .unwrap()
                .extract::<u16>()
                .unwrap(),
            60_400,
        );

        let default_config = module
            .getattr("LongbridgeExecClientConfig")
            .unwrap()
            .call0()
            .unwrap();
        assert_eq!(
            default_config
                .getattr("oauth_callback_port")
                .unwrap()
                .extract::<u16>()
                .unwrap(),
            DEFAULT_OAUTH_CALLBACK_PORT,
        );
    });
}
