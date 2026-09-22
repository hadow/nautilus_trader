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

//! Shared portfolio files for historical replay and broker runners.

use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::Context;
use nautilus_model::{
    identifiers::InstrumentId,
    types::{Currency, Price, Quantity},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    DynamicGridConfig, InstrumentConfig, MultiAssetGridConfig, portfolio::PortfolioConfig,
};

/// Per-instrument execution metadata and files, separate from portfolio cash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridInstrumentFile {
    /// Independent signal, grid, risk and allocation configuration.
    pub strategy: InstrumentConfig,
    /// Fixed venue tick size.
    pub price_increment: Price,
    /// Tradable lot size.
    pub lot_size: Quantity,
    /// Completed-bar CSV for this instrument only.
    pub bars_path: PathBuf,
    /// Optional actual quote replay file.
    pub quotes_path: Option<PathBuf>,
}

/// One account with multiple asynchronous instrument streams.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridPortfolioFile {
    /// Instrument IDs, rather than one global symbol.
    pub instruments: BTreeMap<InstrumentId, GridInstrumentFile>,
    /// Shared risk and initial capital.
    pub portfolio: PortfolioConfig,
    /// Single account quote currency.
    pub currency: Currency,
    /// Deterministic native fill seed.
    pub random_seed: u64,
    /// One-tick slippage probability.
    pub slippage_probability: f64,
    /// Inclusive historical boundary.
    pub start_ns: Option<u64>,
    /// Exclusive historical boundary.
    pub end_ns: Option<u64>,
}

impl GridPortfolioFile {
    /// Creates the same production strategy used by the Longbridge runner.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, allocation or instrument ownership.
    pub fn strategy_config(&self) -> anyhow::Result<MultiAssetGridConfig> {
        let first = self
            .instruments
            .keys()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty universe"))?;
        let base = DynamicGridConfig::new(*first, self.instruments[first].strategy.bar_type).base;
        let c = MultiAssetGridConfig {
            base,
            instruments: self
                .instruments
                .iter()
                .map(|(id, c)| (*id, c.strategy.clone()))
                .collect(),
            portfolio: self.portfolio.clone(),
            state_path: None,
            recovery_context: None,
        };
        c.validate()?;
        for instrument in self.instruments.values() {
            anyhow::ensure!(
                instrument.price_increment.as_decimal() > Decimal::ZERO
                    && instrument.lot_size.as_decimal() > Decimal::ZERO,
                "Invalid market increments"
            );
            anyhow::ensure!(
                !instrument.strategy.confirmed_custom_bars,
                "Historical CSV contains completed ordinary bars"
            );
        }
        Ok(c)
    }
}

/// Loads inline instruments or instrument JSON paths relative to the portfolio file.
///
/// Returns the resolved configuration and canonical configuration input paths for overwrite checks.
/// Historical CSV paths retain their existing working-directory-relative semantics.
///
/// # Errors
///
/// Returns an error for missing files, invalid JSON, unknown fields or inconsistent instruments.
pub fn load_portfolio_config(path: &Path) -> anyhow::Result<(GridPortfolioFile, Vec<PathBuf>)> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving portfolio configuration {}", path.display()))?;
    let mut document: serde_json::Value = serde_json::from_reader(File::open(&path)?)
        .with_context(|| format!("reading portfolio configuration {}", path.display()))?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing configuration directory"))?;
    let mut inputs = vec![path.clone()];
    let instruments = document
        .get_mut("instruments")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("Portfolio instruments must be an object"))?;
    for (id, instrument) in instruments {
        let Some(reference) = instrument.as_str() else {
            continue;
        };
        anyhow::ensure!(
            !reference.trim().is_empty(),
            "Empty instrument configuration path for {id}"
        );
        let source = directory.join(reference);
        let source = source.canonicalize().with_context(|| {
            format!(
                "resolving instrument configuration {id}: {}",
                source.display()
            )
        })?;
        let config: GridInstrumentFile = serde_json::from_reader(File::open(&source)?)
            .with_context(|| {
                format!(
                    "reading instrument configuration {id}: {}",
                    source.display()
                )
            })?;
        *instrument = serde_json::to_value(config)?;
        inputs.push(source);
    }
    let config: GridPortfolioFile = serde_json::from_value(document)
        .with_context(|| format!("parsing portfolio configuration {}", path.display()))?;
    config
        .strategy_config()
        .with_context(|| format!("validating portfolio configuration {}", path.display()))?;
    Ok((config, inputs))
}
