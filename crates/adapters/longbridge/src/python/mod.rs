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

//! PyO3 bindings for the Longbridge adapter.

pub mod config;
pub mod factories;

use nautilus_common::factories::{ClientConfig, DataClientFactory, ExecutionClientFactory};
use nautilus_core::python::{to_pyruntime_err, to_pyvalue_err};
use nautilus_system::get_global_pyo3_registry;
use pyo3::prelude::*;

use crate::{
    common::consts::{LONGBRIDGE, LONGBRIDGE_CLIENT_ID, LONGBRIDGE_VENUE},
    config::{LongbridgeDataClientConfig, LongbridgeExecClientConfig},
    factories::{LongbridgeDataClientFactory, LongbridgeExecutionClientFactory},
};

#[expect(clippy::needless_pass_by_value)]
fn extract_data_factory(
    py: Python<'_>,
    factory: Py<PyAny>,
) -> PyResult<Box<dyn DataClientFactory>> {
    factory
        .extract::<LongbridgeDataClientFactory>(py)
        .map(|factory| Box::new(factory) as Box<dyn DataClientFactory>)
        .map_err(|e| to_pyvalue_err(format!("Failed to extract Longbridge data factory: {e}")))
}

#[expect(clippy::needless_pass_by_value)]
fn extract_exec_factory(
    py: Python<'_>,
    factory: Py<PyAny>,
) -> PyResult<Box<dyn ExecutionClientFactory>> {
    factory
        .extract::<LongbridgeExecutionClientFactory>(py)
        .map(|factory| Box::new(factory) as Box<dyn ExecutionClientFactory>)
        .map_err(|e| {
            to_pyvalue_err(format!(
                "Failed to extract Longbridge execution factory: {e}"
            ))
        })
}

#[expect(clippy::needless_pass_by_value)]
fn extract_data_config(py: Python<'_>, config: Py<PyAny>) -> PyResult<Box<dyn ClientConfig>> {
    config
        .extract::<LongbridgeDataClientConfig>(py)
        .map(|config| Box::new(config) as Box<dyn ClientConfig>)
        .map_err(|e| to_pyvalue_err(format!("Failed to extract Longbridge data config: {e}")))
}

#[expect(clippy::needless_pass_by_value)]
fn extract_exec_config(py: Python<'_>, config: Py<PyAny>) -> PyResult<Box<dyn ClientConfig>> {
    config
        .extract::<LongbridgeExecClientConfig>(py)
        .map(|config| Box::new(config) as Box<dyn ClientConfig>)
        .map_err(|e| {
            to_pyvalue_err(format!(
                "Failed to extract Longbridge execution config: {e}"
            ))
        })
}

/// Exposes Longbridge bindings through `nautilus_trader.adapters.longbridge`.
///
/// # Errors
///
/// Returns an error if a class or registry extractor cannot be registered.
#[pymodule]
pub fn longbridge(_: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add(stringify!(LONGBRIDGE), LONGBRIDGE)?;
    module.add(stringify!(LONGBRIDGE_CLIENT_ID), *LONGBRIDGE_CLIENT_ID)?;
    module.add(stringify!(LONGBRIDGE_VENUE), *LONGBRIDGE_VENUE)?;
    module.add_class::<LongbridgeDataClientConfig>()?;
    module.add_class::<LongbridgeExecClientConfig>()?;
    module.add_class::<LongbridgeDataClientFactory>()?;
    module.add_class::<LongbridgeExecutionClientFactory>()?;

    let registry = get_global_pyo3_registry();
    registry
        .register_factory_extractor(LONGBRIDGE.to_string(), extract_data_factory)
        .map_err(|e| {
            to_pyruntime_err(format!("Failed to register Longbridge data factory: {e}"))
        })?;
    registry
        .register_exec_factory_extractor(LONGBRIDGE.to_string(), extract_exec_factory)
        .map_err(|e| {
            to_pyruntime_err(format!(
                "Failed to register Longbridge execution factory: {e}"
            ))
        })?;
    registry
        .register_config_extractor(
            "LongbridgeDataClientConfig".to_string(),
            extract_data_config,
        )
        .map_err(|e| to_pyruntime_err(format!("Failed to register Longbridge data config: {e}")))?;
    registry
        .register_config_extractor(
            "LongbridgeExecClientConfig".to_string(),
            extract_exec_config,
        )
        .map_err(|e| {
            to_pyruntime_err(format!(
                "Failed to register Longbridge execution config: {e}"
            ))
        })?;
    Ok(())
}
