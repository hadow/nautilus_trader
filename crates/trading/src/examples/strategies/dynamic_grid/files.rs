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

//! 历史回放与券商 runner 共用的组合配置文件模型。

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

/// 单标的执行元数据与数据文件；与组合共享现金配置分离。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridInstrumentFile {
    /// 独立的信号、网格、风险与资金分配配置。
    pub strategy: InstrumentConfig,
    /// 交易场所固定最小价位。
    pub price_increment: Price,
    /// 可交易最小数量单位。
    pub lot_size: Quantity,
    /// 仅包含本标的已完成 K 线的 CSV 文件。
    pub bars_path: PathBuf,
    /// 可选的真实 Quote Tick 回放文件。
    pub quotes_path: Option<PathBuf>,
}

/// 一个账户管理多条异步标的行情流的文件配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridPortfolioFile {
    /// 以 InstrumentId 为键的多标的配置，而非单一全局 symbol。
    pub instruments: BTreeMap<InstrumentId, GridInstrumentFile>,
    /// 共享组合风险与初始资金配置。
    pub portfolio: PortfolioConfig,
    /// 单一账户报价币种。
    pub currency: Currency,
    /// 原生成交模拟器的确定性随机种子。
    pub random_seed: u64,
    /// 发生一个 tick 滑点的概率。
    pub slippage_probability: f64,
    /// 历史回放起始边界，包含该时刻。
    pub start_ns: Option<u64>,
    /// 历史回放结束边界，不包含该时刻。
    pub end_ns: Option<u64>,
}

impl GridPortfolioFile {
    /// 创建与 Longbridge runner 完全相同的生产策略实例。
    ///
    /// # Errors
    ///
    /// 元数据、资金分配或标的所有权无效时返回错误。
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

/// 加载内联标的配置，或相对组合文件解析各标的 JSON 路径。
///
/// 返回解析后的配置及规范化配置输入路径，以便检查输出覆盖输入。
/// 历史 CSV 路径继续保持相对当前工作目录的既有语义。
///
/// # Errors
///
/// 文件缺失、JSON 无效、存在未知字段或标的配置不一致时返回错误。
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
