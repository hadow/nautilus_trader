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

//! Shared configuration loading for the Longbridge grid runner.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::Context;
use nautilus_model::{
    data::BarType,
    identifiers::{AccountId, InstrumentId, TraderId},
    types::{Currency, Price},
};
use nautilus_trading::examples::strategies::dynamic_grid::{
    DynamicGridConfig, InstrumentConfig, MultiAssetGridConfig, files::load_portfolio_config,
    portfolio::PortfolioConfig,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum Mode {
    Sandbox,
    Paper,
    Live,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AppConfig {
    pub(super) mode: Mode,
    pub(super) instruments: BTreeMap<InstrumentId, LiveInstrumentConfig>,
    pub(super) portfolio: PortfolioConfig,
    pub(super) currency: Currency,
    pub(super) trader_id: TraderId,
    pub(super) account_id: AccountId,
    pub(super) state_path: PathBuf,
    pub(super) report_path: PathBuf,
    /// Explicit per-share cash commission for the local simulator, not a broker fee quote.
    #[serde(default)]
    pub(super) sandbox_fee_per_share: Option<Decimal>,
    /// 显式隔离整个标的：仅估值和计入组合风险，不认领、交易或自动平仓。
    #[serde(default)]
    pub(super) isolated_instruments: BTreeSet<InstrumentId>,
    /// 仅模拟账户可豁免冻结资金归因门禁，不增加可用现金或跳过其他对账。
    #[serde(default)]
    pub(super) paper_allow_unattributed_frozen_cash: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LiveInstrumentConfig {
    pub(super) price_increment: Price,
    pub(super) strategy: InstrumentConfig,
}

impl AppConfig {
    pub(super) fn load(path: &Path) -> anyhow::Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Resolving runner configuration {}", path.display()))?;
        let mut document: serde_json::Value = serde_json::from_reader(File::open(&path)?)?;
        let object = document
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Runner configuration must be an object"))?;
        let mut inputs = vec![path.clone()];
        if let Some(reference) = object.remove("portfolio_config") {
            anyhow::ensure!(
                ["instruments", "portfolio", "currency"]
                    .iter()
                    .all(|key| !object.contains_key(*key)),
                "portfolio_config cannot be combined with inline instruments, portfolio or currency"
            );
            let reference = reference
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("portfolio_config must be a nonempty file path"))?;
            let source = path
                .parent()
                .expect("Canonical file has a parent")
                .join(reference);
            let (shared, configuration_inputs) = load_portfolio_config(&source)?;
            inputs.extend(configuration_inputs);
            let mapping: BTreeMap<InstrumentId, InstrumentId> =
                serde_json::from_value(object.remove("instrument_mapping").ok_or_else(|| {
                    anyhow::anyhow!("portfolio_config requires instrument_mapping")
                })?)?;
            anyhow::ensure!(
                mapping.keys().eq(shared.instruments.keys()),
                "instrument_mapping must cover exactly the source portfolio instruments"
            );
            anyhow::ensure!(
                mapping.values().collect::<BTreeSet<_>>().len() == mapping.len(),
                "instrument_mapping targets must be unique"
            );
            let mut instruments = BTreeMap::new();
            for (id, instrument) in shared.instruments {
                let target = mapping[&id];
                let target_symbol = target.symbol.as_str();
                anyhow::ensure!(
                    id.symbol.as_str() == target_symbol
                        || target_symbol
                            .rsplit_once('.')
                            .is_some_and(|(symbol, _)| symbol == id.symbol.as_str()),
                    "instrument_mapping must preserve the asset identity: {id} -> {target}"
                );
                let mut strategy = instrument.strategy;
                match &mut strategy.bar_type {
                    BarType::Standard { instrument_id, .. }
                    | BarType::Composite { instrument_id, .. } => *instrument_id = target,
                }
                instruments.insert(
                    target,
                    LiveInstrumentConfig {
                        price_increment: instrument.price_increment,
                        strategy,
                    },
                );
            }
            object.insert("instruments".into(), serde_json::to_value(instruments)?);
            object.insert("portfolio".into(), serde_json::to_value(shared.portfolio)?);
            object.insert("currency".into(), serde_json::to_value(shared.currency)?);
        }
        let app: Self = serde_json::from_value(document)
            .with_context(|| format!("Parsing runner configuration {}", path.display()))?;
        for output in [
            app.report_path.clone(),
            app.state_path.clone(),
            app.state_path.with_extension("lock"),
            app.state_path.with_extension("next"),
        ] {
            anyhow::ensure!(
                !output
                    .canonicalize()
                    .is_ok_and(|output| inputs.contains(&output)),
                "Runner output must not overwrite configuration inputs: {}",
                output.display()
            );
        }
        app.strategy()?;
        Ok(app)
    }

    pub(super) fn strategy(&self) -> anyhow::Result<MultiAssetGridConfig> {
        anyhow::ensure!(
            !self.paper_allow_unattributed_frozen_cash || self.mode == Mode::Paper,
            "paper_allow_unattributed_frozen_cash requires mode=Paper"
        );
        anyhow::ensure!(
            self.report_path != self.state_path
                && self.report_path != self.state_path.with_extension("lock")
                && self.report_path != self.state_path.with_extension("next"),
            "Report must not overwrite recovery files"
        );
        anyhow::ensure!(!self.instruments.is_empty(), "Empty instrument universe");
        for (id, instrument) in &self.instruments {
            anyhow::ensure!(
                id.venue.as_str() == "LONGBRIDGE",
                "Longbridge venue required"
            );
            anyhow::ensure!(
                instrument.price_increment.as_decimal() > Decimal::ZERO,
                "Positive instrument tick required"
            );
        }
        if self.mode == Mode::Sandbox {
            anyhow::ensure!(
                self.sandbox_fee_per_share
                    .is_some_and(|fee| fee > Decimal::ZERO),
                "Sandbox requires an explicit positive sandbox_fee_per_share"
            );
        }
        let first = *self.instruments.keys().next().expect("Nonempty universe");
        let mut base =
            DynamicGridConfig::new(first, self.instruments[&first].strategy.bar_type).base;
        // 有隔离持仓时不做按标的盲目认领；检查点恢复的原生订单已自带策略归属。
        base.external_order_claims = self
            .isolated_instruments
            .is_empty()
            .then(|| self.instruments.keys().copied().collect());
        let mut instruments: BTreeMap<_, _> = self
            .instruments
            .iter()
            .map(|(id, c)| (*id, c.strategy.clone()))
            .collect();
        for c in instruments.values_mut() {
            c.confirmed_custom_bars = true;
            c.tick_execution = true;
        }
        let config = MultiAssetGridConfig {
            base,
            instruments,
            portfolio: self.portfolio.clone(),
            state_path: Some(self.state_path.clone()),
            recovery_context: Some(format!(
                "{:?}:{}:{}",
                self.mode, self.trader_id, self.account_id
            )),
            isolated_instruments: self.isolated_instruments.clone(),
        };
        config.validate()?;
        Ok(config)
    }
}
