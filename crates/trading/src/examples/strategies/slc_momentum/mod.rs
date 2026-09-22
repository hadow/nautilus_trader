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
//  See the License for the specific language governing permissions and limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Causal cross-sectional selection with session-aware SLC intraday execution.
//!
//! 中文概览：先按同一事件时间做横截面动量选股，再依次评估市场环境、
//! SLC（结构、关键区、确认）、组合风险和订单生命周期。回测与模拟盘共用本模块，
//! 任何输入只能在其 `ts_init` 到达后参与决策，以避免未来数据泄漏。

mod config;
mod data;
mod market;
mod risk;
mod selection;
mod signal;
mod strategy;
mod structure;

pub use config::{
    Ablation, ConfirmationMode, EntryMode, ExitConfig, MomentumConfig, RiskConfig, Session,
    SlcConfig, SlcMomentumConfig, StructureMode, SymbolMetadata, TargetMode, TradeSide,
};
pub use data::minute_bar_type;
pub use market::{MarketObservation, MarketSelectionConfig, MarketUpdate, UniverseSelection};
pub use risk::{PortfolioRiskManager, RiskAllocation, RiskHolding};
pub use selection::{CrossSectionalMomentumRanker, MomentumRank, RankSnapshot};
pub use signal::{MarketRegime, NoTrade, SetupState, SlcSignal, Structure};
pub use strategy::{
    EquityObservation, SlcMomentumReport, SlcMomentumStrategy, SlcObservation, SlcTrade,
};

#[cfg(test)]
mod tests;
