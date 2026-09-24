// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
// -------------------------------------------------------------------------------------------------

//! 股票目标仓位计算；订单与成交状态仍由 NautilusTrader 和 `OrderManager` 管理。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{config::GridConfig, engine::floor_tick, regime::MarketRegime};

/// 将期望多头库存拆分为相互独立的策略仓位组件。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionTarget {
    /// 用于参与中长期趋势的核心仓，不会被普通网格止盈卖出。
    pub core: Decimal,
    /// 跨全部网格代次允许持有的战术网格仓上限。
    pub grid: Decimal,
    /// 经过所有数量和名义金额约束后的精确目标总量。
    pub total: Decimal,
}

/// 根据当前权益和最近一根已完成 K 线的市场状态，计算受限目标仓位。
///
/// 这里计算“策略想持有多少”，不读取订单状态，也不绕过后续单标的和组合风控。
pub(super) fn target_position(
    config: &GridConfig,
    regime: MarketRegime,
    equity: Decimal,
    price: Decimal,
    lot: Decimal,
) -> anyhow::Result<PositionTarget> {
    anyhow::ensure!(equity > Decimal::ZERO && price > Decimal::ZERO && lot > Decimal::ZERO);

    let allocation = (equity * config.capital_allocation).min(config.max_notional);
    let (core_factor, grid_factor) = match regime {
        MarketRegime::TrendUp => (
            config.trend_up_core_multiplier,
            config.trend_up_grid_multiplier,
        ),
        MarketRegime::TrendDown => (
            config.trend_down_core_multiplier,
            config.trend_down_grid_multiplier,
        ),
        MarketRegime::HighVolatility => (
            config.high_volatility_position_multiplier,
            config.high_volatility_position_multiplier,
        ),
        MarketRegime::Range | MarketRegime::LowVolatility | MarketRegime::Disabled => {
            (Decimal::ONE, Decimal::ONE)
        }
    };
    let mut core = floor_tick(
        allocation * config.core_target_pct * core_factor / price,
        lot,
    );
    let mut grid = floor_tick(
        allocation * config.grid_max_pct * grid_factor.min(Decimal::ONE) / price,
        lot,
    )
    .min(floor_tick(config.max_grid_exposure / price, lot));

    // 总仓位先保留 Core，再削减 Grid；股票上行时不会因网格卖出而完全踏空。
    let cap = floor_tick(config.max_position, lot)
        .min(floor_tick(config.max_notional / price, lot))
        .min(floor_tick(equity * config.max_position_pct / price, lot));
    core = core.min(cap);
    grid = grid.min((cap - core).max(Decimal::ZERO));
    Ok(PositionTarget {
        core,
        grid,
        total: core + grid,
    })
}

/// 只扣除同方向未终结订单，不能提前使用反方向订单尚未成交的库存或容量。
#[must_use]
pub(super) fn position_delta(
    target: Decimal,
    current: Decimal,
    pending_buys: Decimal,
    pending_sells: Decimal,
    lot: Decimal,
) -> Decimal {
    if target >= current {
        floor_tick((target - current - pending_buys).max(Decimal::ZERO), lot)
    } else {
        -floor_tick((current - target - pending_sells).max(Decimal::ZERO), lot)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    #[rstest]
    #[case(dec!(100), dec!(100), dec!(0), dec!(100), dec!(0))]
    #[case(dec!(100), dec!(60), dec!(25), dec!(5), dec!(15))]
    #[case(dec!(80), dec!(100), dec!(25), dec!(5), dec!(-15))]
    #[case(dec!(80), dec!(100), dec!(0), dec!(30), dec!(0))]
    fn review_delta_never_spends_unfilled_opposite_orders(
        #[case] target: Decimal,
        #[case] current: Decimal,
        #[case] buys: Decimal,
        #[case] sells: Decimal,
        #[case] expected: Decimal,
    ) {
        assert_eq!(
            position_delta(target, current, buys, sells, dec!(1)),
            expected
        );
    }

    #[rstest]
    fn review_position_cap_is_rounded_to_whole_lots() {
        let config = GridConfig {
            max_position: dec!(7.5),
            ..Default::default()
        };
        let target = target_position(
            &config,
            MarketRegime::Range,
            dec!(100000),
            dec!(100),
            dec!(1),
        )
        .unwrap();
        assert_eq!(target.total, dec!(7));
        assert_eq!(target.core % dec!(1), Decimal::ZERO);
    }

    #[rstest]
    fn stock_targets_keep_core_reduce_downtrends_and_include_pending_orders() {
        let config = GridConfig {
            capital_allocation: Decimal::ONE,
            max_notional: dec!(10000),
            max_grid_exposure: dec!(10000),
            max_position: dec!(1000),
            max_position_pct: Decimal::ONE,
            ..Default::default()
        };
        let range = target_position(
            &config,
            MarketRegime::Range,
            dec!(10000),
            dec!(100),
            dec!(1),
        )
        .unwrap();
        let up = target_position(
            &config,
            MarketRegime::TrendUp,
            dec!(10000),
            dec!(100),
            dec!(1),
        )
        .unwrap();
        let down = target_position(
            &config,
            MarketRegime::TrendDown,
            dec!(10000),
            dec!(100),
            dec!(1),
        )
        .unwrap();
        assert_eq!(
            range,
            PositionTarget {
                core: dec!(40),
                grid: dec!(60),
                total: dec!(100)
            }
        );
        assert_eq!(
            up,
            PositionTarget {
                core: dec!(50),
                grid: dec!(30),
                total: dec!(80)
            }
        );
        assert_eq!(
            down,
            PositionTarget {
                core: dec!(20),
                grid: Decimal::ZERO,
                total: dec!(20)
            }
        );
        assert_eq!(
            position_delta(dec!(100), dec!(60), dec!(25), dec!(5), dec!(1)),
            dec!(15)
        );
    }
}
