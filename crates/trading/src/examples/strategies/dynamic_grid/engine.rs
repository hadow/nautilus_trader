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

//! 动态网格的精确价格几何与重置条件；订单执行统一交给 NautilusTrader。

use nautilus_model::enums::OrderSide;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::config::{GridConfig, PositionSizing, SpacingMode, StrategyMode};

/// 单个网格层级的生命周期。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LevelStatus {
    /// 尚未提交入场订单。
    Pending,
    /// 至少存在一笔未终结的入场或出场订单。
    Active,
    /// 已持有成交库存，正在等待对应出场。
    Filled,
    /// 因撤单或网格重置而退出本代网格。
    Cancelled,
    /// 该层级买入的库存已全部卖出。
    Completed,
}

/// 一组相邻买卖价及其跨越整个生命周期的稳定标识。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridLevel {
    /// 相对中心价的有符号层级；负数层级是常规回落买入层。
    pub level_index: i32,
    /// 买入限价，按交易场所最小价位向下取整。
    pub price: Decimal,
    /// 入场方向；任何卖单都必须由本层级真实成交的多头库存覆盖。
    pub side: OrderSide,
    /// 相邻出场价，按交易场所最小价位向上取整。
    pub exit_price: Decimal,
    /// 计划数量，按最小交易单位向下取整。
    pub quantity: Decimal,
    /// 当前生命周期状态。
    pub status: LevelStatus,
    /// 最近一次入场订单标识。
    pub entry_order_id: Option<String>,
    /// 最近一次出场订单标识。
    pub exit_order_id: Option<String>,
}

/// 单代网格内保持不变的价格几何。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridEngine {
    /// 单调递增的网格代号。
    pub grid_id: u64,
    /// 创建网格时已观测到的中心价。
    pub center: Decimal,
    /// 经过波动率、上下限和成本约束后的实际相邻间距比例。
    pub spacing: Decimal,
    /// 最低入场边界。
    pub lower_bound: Decimal,
    /// 最高出场边界。
    pub upper_bound: Decimal,
    /// 创建时间戳，单位为纳秒。
    pub created_ns: u64,
    /// 中心价上下两侧的相邻买卖对。
    pub levels: Vec<GridLevel>,
}

impl GridEngine {
    /// 围绕中心价构建上下对称的乘法网格买卖对。
    ///
    /// # Errors
    ///
    /// 输入无效、十进制定点数溢出，或价格按 tick 取整后层级重合时返回错误。
    #[allow(
        clippy::too_many_arguments,
        reason = "Explicit geometry, venue increments and generation identity"
    )]
    pub fn build(
        config: &GridConfig,
        grid_id: u64,
        center: Decimal,
        spacing: Decimal,
        capital: Decimal,
        tick: Decimal,
        lot: Decimal,
        now: u64,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            center > Decimal::ZERO
                && tick > Decimal::ZERO
                && lot > Decimal::ZERO
                && capital >= Decimal::ZERO,
            "Invalid grid inputs"
        );
        anyhow::ensure!(
            spacing >= config.min_spacing_pct
                && spacing <= config.max_spacing_pct
                && spacing >= config.cost_floor(),
            "Spacing cannot cover configured costs or bounds"
        );
        // 上下共用同一个乘法比率，下方为 center / (1 + spacing)^n，非 center * (1 - spacing)^n
        let ratio = Decimal::ONE + spacing;
        let mut lower = center;
        let mut upper = center;
        let mut levels = Vec::new();
        let total_weight: Decimal = (1..=config.grid_levels)
            .map(|i| weight(config, i, config.grid_levels))
            .sum();
        for i in 1..=config.grid_levels {
            let next_lower = lower
                .checked_div(ratio)
                .ok_or_else(|| anyhow::anyhow!("Grid price underflow"))?;
            let next_upper = upper
                .checked_mul(ratio)
                .ok_or_else(|| anyhow::anyhow!("Grid price overflow"))?;
            let allocation = capital * weight(config, i, config.grid_levels) / total_weight;
            for (index, buy, sell, fraction) in [
                (
                    -(i as i32),
                    floor_tick(next_lower, tick),
                    ceil_tick(lower, tick),
                    Decimal::ONE - config.initial_inventory_fraction,
                ),
                (
                    i as i32,
                    floor_tick(upper, tick),
                    ceil_tick(next_upper, tick),
                    config.initial_inventory_fraction,
                ),
            ] {
                anyhow::ensure!(
                    buy > Decimal::ZERO && sell > buy,
                    "Grid levels collapse at instrument tick size"
                );
                let quantity = floor_tick(allocation * fraction / buy, lot);
                levels.push(GridLevel {
                    level_index: index,
                    price: buy,
                    side: OrderSide::Buy,
                    exit_price: sell,
                    quantity,
                    status: LevelStatus::Pending,
                    entry_order_id: None,
                    exit_order_id: None,
                });
            }
            lower = next_lower;
            upper = next_upper;
        }
        let mut buys: Vec<_> = levels
            .iter()
            .filter(|l| l.level_index < 0)
            .map(|l| l.price)
            .collect();
        buys.sort();
        anyhow::ensure!(
            buys.windows(2).all(|p| p[0] < p[1]),
            "Grid prices collapse after tick rounding"
        );
        Ok(Self {
            grid_id,
            center,
            spacing,
            lower_bound: floor_tick(lower, tick),
            upper_bound: ceil_tick(upper, tick),
            created_ns: now,
            levels,
        })
    }

    /// 同时检查最短间隔、相对中心位移和 ATR 越界距离，判断是否允许重置中心。
    #[must_use]
    pub fn can_reset(&self, config: &GridConfig, price: Decimal, atr: Decimal, now: u64) -> bool {
        if price <= Decimal::ZERO || atr < Decimal::ZERO || self.center <= Decimal::ZERO {
            return false;
        }
        let distance = if price > self.upper_bound {
            price - self.upper_bound
        } else if price < self.lower_bound {
            self.lower_bound - price
        } else {
            (price - self.center).abs()
        };
        now.saturating_sub(self.created_ns) / 1_000_000_000 >= config.minimum_reset_interval_secs
            && (price - self.center).abs() >= self.center * config.minimum_reset_distance
            && atr
                .checked_mul(config.minimum_reset_atr_multiple)
                .is_some_and(|minimum| distance >= minimum)
    }

    /// 返回本次价格路径穿越的全部层级，并按真实穿越顺序排列；首次到达等价于穿越。
    #[must_use]
    pub fn crossed(&self, previous: Decimal, price: Decimal) -> Vec<i32> {
        let mut crossed: Vec<_> = self
            .levels
            .iter()
            .filter(|l| {
                if price < previous {
                    l.price < previous && l.price >= price
                } else {
                    l.exit_price > previous && l.exit_price <= price
                }
            })
            .collect();
        crossed.sort_by_key(|l| {
            if price < previous {
                -l.price
            } else {
                l.exit_price
            }
        });
        crossed.into_iter().map(|l| l.level_index).collect()
    }
}

/// 使用当前已经完成的数据计算 ATR 自适应间距，并纳入交易成本下限。
///
/// # Errors
///
/// 波动率输入无效，或成本下限与配置矛盾时返回错误。
pub fn spacing(
    config: &GridConfig,
    atr: Decimal,
    price: Decimal,
    multiplier: Decimal,
) -> anyhow::Result<Decimal> {
    spacing_components(config, atr, price, multiplier).map(|(_, effective)| effective)
}

pub(super) fn spacing_components(
    config: &GridConfig,
    atr: Decimal,
    price: Decimal,
    multiplier: Decimal,
) -> anyhow::Result<(Decimal, Decimal)> {
    anyhow::ensure!(
        price > Decimal::ZERO && atr >= Decimal::ZERO && multiplier > Decimal::ZERO,
        "Invalid volatility inputs"
    );
    let raw = match config.spacing_mode {
        SpacingMode::Percentage => config.spacing_pct,
        SpacingMode::Atr => {
            let width = atr
                .checked_div(price)
                .and_then(|value| value.checked_mul(config.atr_multiplier))
                .ok_or_else(|| anyhow::anyhow!("ATR grid width overflow"))?;
            if config.strategy_mode == StrategyMode::StockAdaptive {
                width / Decimal::from(config.grid_levels)
            } else {
                width
            }
        }
    }
    .checked_mul(multiplier)
    .ok_or_else(|| anyhow::anyhow!("Trend grid width overflow"))?;
    // 最小间距和交易成本都可成为实际下限，ATR 较小时继续变小也不会让网格更密
    let value = raw
        .max(config.min_spacing_pct)
        .max(config.cost_floor())
        .min(config.max_spacing_pct);
    anyhow::ensure!(value >= config.cost_floor(), "Grid cannot cover costs");
    Ok((raw, value))
}

pub(super) fn floor_tick(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).floor() * increment
}
pub(super) fn ceil_tick(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).ceil() * increment
}

fn weight(config: &GridConfig, index: usize, levels: usize) -> Decimal {
    Decimal::from(match config.position_sizing {
        PositionSizing::Equal | PositionSizing::VolatilityAdjusted => 1,
        PositionSizing::Progressive => index,
        PositionSizing::Inverse => levels + 1 - index,
    } as u64)
}
