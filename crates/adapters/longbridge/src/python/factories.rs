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

//! Python constructors for Longbridge factories.

use nautilus_model::identifiers::{AccountId, TraderId};
use pyo3::pymethods;

use crate::{
    common::consts::LONGBRIDGE,
    factories::{LongbridgeDataClientFactory, LongbridgeExecutionClientFactory},
};

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl LongbridgeDataClientFactory {
    /// Factory for Longbridge data clients.
    #[new]
    fn py_new() -> Self {
        Self::new()
    }

    #[pyo3(name = "name")]
    fn py_name(&self) -> &'static str {
        LONGBRIDGE
    }
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl LongbridgeExecutionClientFactory {
    /// Factory for Longbridge execution clients.
    #[new]
    fn py_new(trader_id: TraderId, account_id: AccountId) -> Self {
        Self::new(trader_id, account_id)
    }

    #[pyo3(name = "name")]
    fn py_name(&self) -> &'static str {
        LONGBRIDGE
    }
}
