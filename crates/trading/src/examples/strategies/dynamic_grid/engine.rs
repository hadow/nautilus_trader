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

//! Exact adaptive geometric levels and reset guards; execution belongs to Nautilus.

use nautilus_model::enums::OrderSide;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::config::{GridConfig, PositionSizing, SpacingMode};

/// Grid level lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LevelStatus {
    /// No entry has been submitted.
    Pending,
    /// At least one entry or exit is outstanding.
    Active,
    /// Filled inventory is waiting for its exit.
    Filled,
    /// Retired by cancellation/reset.
    Cancelled,
    /// All entry inventory has been sold.
    Completed,
}

/// One adjacent buy/sell pair and its lifetime identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridLevel {
    /// Signed level relative to center; negative levels are ordinary entries.
    pub level_index: i32,
    /// Buy limit price, rounded down to the venue tick.
    pub price: Decimal,
    /// Entry side; every sell is backed by this pair's filled long inventory.
    pub side: OrderSide,
    /// Adjacent exit price, rounded up to the venue tick.
    pub exit_price: Decimal,
    /// Planned quantity rounded down to the lot size.
    pub quantity: Decimal,
    /// Current lifecycle status.
    pub status: LevelStatus,
    /// Latest entry identity.
    pub entry_order_id: Option<String>,
    /// Latest exit identity.
    pub exit_order_id: Option<String>,
}

/// Immutable geometry within a generation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridEngine {
    /// Monotonically increasing grid identity.
    pub grid_id: u64,
    /// Center observed at creation.
    pub center: Decimal,
    /// Effective spacing fraction.
    pub spacing: Decimal,
    /// Lowest entry boundary.
    pub lower_bound: Decimal,
    /// Highest exit boundary.
    pub upper_bound: Decimal,
    /// Creation timestamp in nanoseconds.
    pub created_ns: u64,
    /// Adjacent pairs below and above the center.
    pub levels: Vec<GridLevel>,
}

impl GridEngine {
    /// Builds multiplicative pairs above and below the center.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid inputs, decimal overflow or tick-collapsed levels.
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

    /// Whether time, anchor distance and current ATR overshoot permit re-centering.
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

    /// Crossed levels in execution order; equality triggers only on first arrival.
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

/// Calculates fee-aware spacing from current, already-observed ATR.
///
/// # Errors
///
/// Returns an error if the inputs or resulting cost floor are invalid.
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
        SpacingMode::Atr => atr
            .checked_div(price)
            .and_then(|value| value.checked_mul(config.atr_multiplier))
            .ok_or_else(|| anyhow::anyhow!("ATR grid width overflow"))?,
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
