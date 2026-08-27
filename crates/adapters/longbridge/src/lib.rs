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

//! [NautilusTrader](https://nautilustrader.io) adapter for
//! [Longbridge OpenAPI](https://open.longbridge.com/docs).
//!
//! The adapter uses the official [`longbridge`] Rust SDK. [`longbridge::quote::QuoteContext`]
//! owns market-data transport and [`longbridge::trade::TradeContext`] owns trading, account,
//! position, execution and private-push transport. Authentication uses the SDK's OAuth 2.0
//! authorization-code flow with persisted, automatically refreshed tokens.

#![warn(rustc::all)]
#![deny(unsafe_code)]
#![deny(nonstandard_style)]
#![deny(missing_debug_implementations)]
#![deny(clippy::missing_panics_doc)]
#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::clone_on_copy)]

pub mod common;
pub mod config;
pub mod data;
pub mod execution;
pub mod factories;

#[cfg(feature = "python")]
pub mod python;

pub use crate::{
    config::{LongbridgeDataClientConfig, LongbridgeExecClientConfig},
    data::LongbridgeDataClient,
    execution::LongbridgeExecutionClient,
    factories::{LongbridgeDataClientFactory, LongbridgeExecutionClientFactory},
};
