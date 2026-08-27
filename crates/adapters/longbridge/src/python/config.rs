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

//! Python constructors for Longbridge configuration.

use nautilus_model::enums::AccountType;
use pyo3::pymethods;

use crate::config::{
    DEFAULT_OAUTH_CALLBACK_PORT, LongbridgeDataClientConfig, LongbridgeExecClientConfig,
};

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl LongbridgeDataClientConfig {
    /// Configuration for the Longbridge live data client.
    #[new]
    #[pyo3(signature = (
        oauth_client_id = None,
        oauth_callback_port = DEFAULT_OAUTH_CALLBACK_PORT,
        http_url = None,
        quote_ws_url = None,
        enable_overnight = false,
    ))]
    fn py_new(
        oauth_client_id: Option<String>,
        oauth_callback_port: u16,
        http_url: Option<String>,
        quote_ws_url: Option<String>,
        enable_overnight: bool,
    ) -> Self {
        Self {
            oauth_client_id,
            oauth_callback_port,
            http_url,
            quote_ws_url,
            enable_overnight,
        }
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl LongbridgeExecClientConfig {
    /// Configuration for the Longbridge live execution client.
    #[new]
    #[pyo3(signature = (
        oauth_client_id = None,
        oauth_callback_port = DEFAULT_OAUTH_CALLBACK_PORT,
        http_url = None,
        trade_ws_url = None,
        account_type = AccountType::Margin,
        papertrading = false,
        outside_rth = false,
    ))]
    fn py_new(
        oauth_client_id: Option<String>,
        oauth_callback_port: u16,
        http_url: Option<String>,
        trade_ws_url: Option<String>,
        account_type: AccountType,
        papertrading: bool,
        outside_rth: bool,
    ) -> Self {
        Self {
            oauth_client_id,
            oauth_callback_port,
            http_url,
            trade_ws_url,
            account_type,
            papertrading,
            outside_rth,
        }
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }
}
