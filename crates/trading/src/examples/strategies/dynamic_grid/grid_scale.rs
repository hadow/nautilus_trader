// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
// -------------------------------------------------------------------------------------------------

//! 网格尺度分类：观察跨日价格路径相对真实网格间距的漂移和反弹，不预测支撑位。

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    regime::{MarketRegime, rebound_opportunities},
    stock::StockMarketState,
};

const BAR_NS: u64 = 15 * 60_000_000_000;
const SESSION_BARS: usize = 26;
const HALF_SESSION_BARS: usize = 14;

/// 独立对照分类与仓位政策，避免把整套改动的效果都归因于指标。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GridScaleMode {
    /// 仅记录观测，订单、仓位与重置全部沿用旧方案。
    #[default]
    Shadow,
    /// 替换分类，沿用既有状态对应的目标仓位政策。
    Classifier,
    /// 替换分类并限制新增网格库存，不因软预算下降直接清仓。
    Adaptive,
}

/// 研究起点而非已优化参数；只适用于完整 15 分钟常规时段收盘。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridScaleConfig {
    /// 是否只观测、替换分类或同时启用柔性预算。
    pub mode: GridScaleMode,
    /// 保留最近实际观测到的交易时段数，不按自然日清空记忆。
    pub lookback_sessions: usize,
    /// 进入趋势要求的路径效率，定义为绝对净位移/总绝对路径长度。
    pub trend_enter_efficiency: Decimal,
    /// 已处于趋势时维持趋势的较低效率门槛，构成滞回。
    pub trend_exit_efficiency: Decimal,
    /// 趋势进入至少漂移多少格；退出距离为此值的一半。
    pub trend_drift_grids: Decimal,
    /// 恢复完整网格预算至少需要的已确认一格反弹次数。
    pub minimum_rebounds: usize,
    /// 不确定但非禁入状态的预算乘数；只能缩减，不能增加原有额度。
    pub uncertain_budget: Decimal,
    /// 窗口高点回撤此格数时，新增预算减半；None 保留原来的等待时间模型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drawdown_budget_grids: Option<Decimal>,
    /// 独立重置消融开关；Shadow 忽略此值，始终保留旧行为。
    pub reset_on_regime_change: bool,
}

impl Default for GridScaleConfig {
    fn default() -> Self {
        Self {
            mode: GridScaleMode::Shadow,
            lookback_sessions: 10,
            trend_enter_efficiency: Decimal::new(6, 1),
            trend_exit_efficiency: Decimal::new(4, 1),
            trend_drift_grids: Decimal::from(2),
            minimum_rebounds: 2,
            uncertain_budget: Decimal::new(5, 1),
            drawdown_budget_grids: None,
            reset_on_regime_change: true,
        }
    }
}

impl GridScaleConfig {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (2..=60).contains(&self.lookback_sessions),
            "Invalid grid-scale lookback"
        );
        anyhow::ensure!(
            self.trend_exit_efficiency > Decimal::ZERO
                && self.trend_exit_efficiency < self.trend_enter_efficiency
                && self.trend_enter_efficiency <= Decimal::ONE
                && self.trend_drift_grids >= Decimal::ONE
                && self.trend_drift_grids <= Decimal::from(100)
                && (1..=SESSION_BARS * self.lookback_sessions).contains(&self.minimum_rebounds)
                && (Decimal::ZERO..=Decimal::ONE).contains(&self.uncertain_budget),
            "Invalid grid-scale hysteresis or budget"
        );
        anyhow::ensure!(
            self.drawdown_budget_grids
                .is_none_or(|grids| (Decimal::ONE..=Decimal::from(100)).contains(&grids)),
            "Invalid grid-scale drawdown budget"
        );
        Ok(())
    }
}

/// 每个完整分类 Bar 的审计观测；反弹是价格机会，不是已实现盈利周期。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridScaleSnapshot {
    /// 当前已完成分类 Bar 的收盘时间。
    pub ts_ns: u64,
    /// 跨时段历史是否足够；不足时禁止候选方案新增库存。
    pub initialized: bool,
    /// 用既有市场状态枚举接入组合和订单政策。
    pub regime: MarketRegime,
    /// `WARMUP`、`QUIET`、`TREND`、`TWO_WAY` 或 `UNCERTAIN`。
    pub reason: String,
    /// 实际用于此次观察的网格百分比间距。
    pub spacing: Decimal,
    /// 净位移/窗口首价/间距；有符号，可跨多个网格。
    pub drift_grids: Decimal,
    /// 绝对净位移/总绝对位移，零路径时为零。
    pub efficiency: Decimal,
    /// 先下跌一格、再从低点反弹一格的次数。
    pub rebounds: usize,
    /// 已确认反弹平均等待多少个观测 Bar，不包含隔夜自然时间。
    pub average_rebound_bars: Option<f64>,
    /// 最近一次跌出一格后尚未反弹一格的观测 Bar 数。
    pub unresolved_bars: usize,
    /// 相对窗口最高收盘价尚未收复的格数，仅在深度折扣启用时记录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drawdown_grids: Option<Decimal>,
    /// 只乘于原有网格目标的 [0,1] 预算系数，不作用于 Core。
    pub budget: Decimal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Close {
    ts_ns: u64,
    session: u64,
    price: Decimal,
}

/// 有界历史与滞回状态随原有检查点保存；没有第二套订单/账户账本。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GridScaleRegime {
    closes: Vec<Close>,
    previous_regime: MarketRegime,
    pub(super) snapshot: GridScaleSnapshot,
}

impl GridScaleRegime {
    pub(super) fn update(
        &mut self,
        config: &GridScaleConfig,
        ts_ns: u64,
        price: Decimal,
        spacing: Decimal,
    ) -> anyhow::Result<()> {
        let session = StockMarketState::session_open_ns(ts_ns)
            .ok_or_else(|| anyhow::anyhow!("Grid-scale close outside regular session"))?;
        anyhow::ensure!(
            ts_ns > self.snapshot.ts_ns
                && (ts_ns - session).is_multiple_of(BAR_NS)
                && price >= Decimal::new(1, 12)
                && price <= Decimal::from(1_000_000_000_000_u64)
                && spacing > Decimal::ZERO
                && spacing < Decimal::ONE,
            "Invalid grid-scale completed observation"
        );
        self.closes.push(Close {
            ts_ns,
            session,
            price,
        });
        // 最多 60×26 个点；仅每 15 分钟扫描，不扫描成交和订单历史。
        let sessions: Vec<_> = self
            .closes
            .iter()
            .map(|p| p.session)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if sessions.len() > config.lookback_sessions {
            let first = sessions[sessions.len() - config.lookback_sessions];
            self.closes.retain(|p| p.session >= first);
        }
        self.previous_regime = self.snapshot.regime;
        self.snapshot = self.calculate(config, spacing)?;
        Ok(())
    }

    fn calculate(
        &self,
        config: &GridScaleConfig,
        spacing: Decimal,
    ) -> anyhow::Result<GridScaleSnapshot> {
        let Some(first) = self.closes.first() else {
            return Ok(GridScaleSnapshot::default());
        };
        let Some(last) = self.closes.last() else {
            return Ok(GridScaleSnapshot::default());
        };
        let prices: Vec<_> = self.closes.iter().map(|p| p.price).collect();
        let path: Decimal = prices.windows(2).map(|p| (p[1] - p[0]).abs()).sum();
        let drift = last.price - first.price;
        let efficiency = if path.is_zero() {
            Decimal::ZERO
        } else {
            drift.abs() / path
        };
        let drift_grids = (drift / first.price)
            .checked_div(spacing)
            .ok_or_else(|| anyhow::anyhow!("Grid-scale drift overflow"))?;
        let (rebounds, average_rebound_bars, unresolved_bars) =
            rebound_opportunities(&prices, spacing);
        // 每个计入暖机的时段必须观测过首个完整桶；半日市至少有 14 根 15 分钟 Bar。
        // 不臆造交易所日历：缺失整日不能当成已观测交易日，盘尾缺数仍需数据质量审计。
        let starts = self
            .closes
            .iter()
            .filter(|p| p.ts_ns == p.session + BAR_NS)
            .count();
        let mut counts = BTreeMap::<u64, usize>::new();
        for point in &self.closes {
            *counts.entry(point.session).or_default() += 1;
        }
        let initialized = starts == config.lookback_sessions
            && counts
                .iter()
                .all(|(session, count)| *session == last.session || *count >= HALF_SESSION_BARS);
        let staying = matches!(self.previous_regime, MarketRegime::TrendUp)
            && drift > Decimal::ZERO
            || matches!(self.previous_regime, MarketRegime::TrendDown) && drift < Decimal::ZERO;
        let threshold = if staying {
            config.trend_exit_efficiency
        } else {
            config.trend_enter_efficiency
        };
        let distance = if staying {
            config.trend_drift_grids / Decimal::from(2)
        } else {
            config.trend_drift_grids
        };
        let low = prices.iter().copied().min().unwrap_or(first.price);
        let high = prices.iter().copied().max().unwrap_or(first.price);
        let (regime, reason, mut budget) = if !initialized {
            (MarketRegime::Disabled, "WARMUP", Decimal::ZERO)
        } else if efficiency >= threshold && drift_grids.abs() >= distance {
            if drift < Decimal::ZERO {
                (MarketRegime::TrendDown, "TREND", Decimal::ZERO)
            } else {
                (MarketRegime::TrendUp, "TREND", config.uncertain_budget)
            }
        } else if high / low - Decimal::ONE < spacing {
            (MarketRegime::LowVolatility, "QUIET", Decimal::ZERO)
        } else if rebounds >= config.minimum_rebounds && efficiency <= config.trend_exit_efficiency
        {
            (MarketRegime::Range, "TWO_WAY", Decimal::ONE)
        } else {
            (MarketRegime::Range, "UNCERTAIN", config.uncertain_budget)
        };
        // 尚未收复一格的等待越占据历史窗口，新增库存越少；已有库存不因此市价平仓。
        budget *= Decimal::ONE - Decimal::from(unresolved_bars) / Decimal::from(prices.len());
        let drawdown_grids = if let Some(scale) = config.drawdown_budget_grids {
            let depth = ((high - last.price) / high)
                .checked_div(spacing)
                .ok_or_else(|| anyhow::anyhow!("Grid-scale drawdown overflow"))?;
            // 一次小反弹不等于收复全部回撤；连续折扣仍允许买入，不新增禁入或强平政策。
            budget *= scale / (scale + depth);
            Some(depth)
        } else {
            None
        };
        Ok(GridScaleSnapshot {
            ts_ns: last.ts_ns,
            initialized,
            regime,
            reason: reason.into(),
            spacing,
            drift_grids,
            efficiency,
            rebounds,
            average_rebound_bars,
            unresolved_bars,
            drawdown_grids,
            budget,
        })
    }

    pub(super) fn validate(&self, config: &GridScaleConfig, latest_ns: u64) -> anyhow::Result<()> {
        config.validate()?;
        anyhow::ensure!(
            self.closes.len() <= SESSION_BARS * config.lookback_sessions,
            "Oversized grid-scale history"
        );
        let mut previous = 0;
        for point in &self.closes {
            anyhow::ensure!(
                point.ts_ns > previous
                    && point.ts_ns <= latest_ns
                    && point.price >= Decimal::new(1, 12)
                    && point.price <= Decimal::from(1_000_000_000_000_u64)
                    && StockMarketState::session_open_ns(point.ts_ns) == Some(point.session)
                    && (point.ts_ns - point.session).is_multiple_of(BAR_NS),
                "Invalid recovered grid-scale close"
            );
            previous = point.ts_ns;
        }
        anyhow::ensure!(
            self.closes
                .iter()
                .map(|p| p.session)
                .collect::<BTreeSet<_>>()
                .len()
                <= config.lookback_sessions,
            "Too many recovered grid-scale sessions"
        );
        anyhow::ensure!(
            self.closes.is_empty()
                || (self.snapshot.spacing > Decimal::ZERO && self.snapshot.spacing < Decimal::ONE),
            "Invalid recovered grid-scale spacing"
        );
        anyhow::ensure!(
            matches!(
                self.previous_regime,
                MarketRegime::Disabled
                    | MarketRegime::Range
                    | MarketRegime::LowVolatility
                    | MarketRegime::TrendUp
                    | MarketRegime::TrendDown
            ),
            "Invalid grid-scale hysteresis state"
        );
        let expected = self.calculate(config, self.snapshot.spacing)?;
        // 均值使用 f64，仅作诊断；JSON 往返可能存在一个 ULP 的变化。
        let mut actual = self.snapshot.clone();
        anyhow::ensure!(
            match (actual.average_rebound_bars, expected.average_rebound_bars) {
                (Some(a), Some(b)) => a.is_finite() && (a - b).abs() < 1e-10,
                (None, None) => true,
                _ => false,
            },
            "Invalid recovered rebound duration"
        );
        actual.average_rebound_bars = expected.average_rebound_bars;
        anyhow::ensure!(
            actual == expected && previous == latest_ns,
            "Grid-scale snapshot differs from completed history"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    const OPEN: u64 = 1_735_828_200_000_000_000;
    const DAY: u64 = 86_400_000_000_000;

    #[rstest]
    fn grid_scale_depth_budget_is_opt_in_bounded_and_monotonic() {
        let baseline = GridScaleConfig {
            lookback_sessions: 2,
            ..Default::default()
        };
        assert!(
            serde_json::to_value(&baseline)
                .unwrap()
                .get("drawdown_budget_grids")
                .is_none()
        );
        for value in [dec!(0), dec!(-1), dec!(0.5), dec!(101)] {
            let config = GridScaleConfig {
                drawdown_budget_grids: Some(value),
                ..baseline.clone()
            };
            assert!(config.validate().is_err());
        }
        let prices: Vec<_> = (0..52)
            .map(|i| [dec!(100), dec!(99), dec!(100), dec!(101)][i % 4])
            .collect();
        for scale in [dec!(2), dec!(4), dec!(8)] {
            let config = GridScaleConfig {
                drawdown_budget_grids: Some(scale),
                ..baseline.clone()
            };
            let peak = observe(&config, &prices, dec!(0.005));
            assert_eq!(peak.snapshot.drawdown_grids, Some(Decimal::ZERO));
            assert_eq!(
                peak.snapshot.budget,
                observe(&baseline, &prices, dec!(0.005)).snapshot.budget
            );
            let mut previous = Decimal::ONE;
            for last in [dec!(100), dec!(99), dec!(96), dec!(90), dec!(80)] {
                let mut path = prices.clone();
                *path.last_mut().unwrap() = last;
                let candidate = observe(&config, &path, dec!(0.005));
                let original = observe(&baseline, &path, dec!(0.005));
                let depth = candidate.snapshot.drawdown_grids.unwrap();
                assert_eq!(
                    candidate.snapshot.budget,
                    original.snapshot.budget * (scale / (scale + depth))
                );
                assert!(
                    (Decimal::ZERO..=original.snapshot.budget).contains(&candidate.snapshot.budget)
                );
                assert!(candidate.snapshot.budget <= previous);
                previous = candidate.snapshot.budget;
            }
        }
    }

    #[rstest]
    fn grid_scale_depth_budget_does_not_treat_a_small_bounce_as_full_recovery() {
        let baseline = GridScaleConfig {
            lookback_sessions: 2,
            ..Default::default()
        };
        let mut document = serde_json::to_value(&baseline).unwrap();
        document["drawdown_budget_grids"] = serde_json::json!("4");
        let candidate: GridScaleConfig = serde_json::from_value(document).unwrap();
        candidate.validate().unwrap();
        let mut prices: Vec<_> = (0..51)
            .map(|i| [dec!(100), dec!(99), dec!(100), dec!(101)][i % 4])
            .collect();
        prices.push(dec!(80));
        let original = observe(&baseline, &prices, dec!(0.005));
        let mut depth = observe(&candidate, &prices, dec!(0.005));
        assert_eq!(original.snapshot.regime, MarketRegime::Range);
        assert_eq!(depth.snapshot.regime, original.snapshot.regime);
        assert!(original.snapshot.budget > dec!(0.9));
        assert!(depth.snapshot.budget > Decimal::ZERO);
        assert!(depth.snapshot.budget < dec!(0.1));

        // 一格反弹能完成一个价格机会，但不能消除仍然相差几十格的库存风险。
        let ts = OPEN + 4 * DAY + BAR_NS;
        depth.update(&candidate, ts, dec!(81), dec!(0.005)).unwrap();
        assert_eq!(depth.snapshot.unresolved_bars, 0);
        assert!(depth.snapshot.budget < dec!(0.1));
        let restored: GridScaleRegime =
            serde_json::from_value(serde_json::to_value(&depth).unwrap()).unwrap();
        restored.validate(&candidate, ts).unwrap();
        assert_eq!(restored.snapshot, depth.snapshot);
    }

    fn observe(config: &GridScaleConfig, prices: &[Decimal], spacing: Decimal) -> GridScaleRegime {
        let mut state = GridScaleRegime::default();
        for (index, price) in prices.iter().enumerate() {
            // 跳过周末不改变观察数量；这里仅生成可验证的已完成桶。
            let ts = OPEN
                + (index / SESSION_BARS) as u64 * DAY
                + (index % SESSION_BARS + 1) as u64 * BAR_NS;
            state.update(config, ts, *price, spacing).unwrap();
        }
        state
    }

    #[rstest]
    fn grid_scale_distinguishes_noise_two_way_and_directional_inventory_risk() {
        let config = GridScaleConfig {
            lookback_sessions: 2,
            ..Default::default()
        };
        let noise: Vec<_> = (0..52)
            .map(|i| if i % 2 == 0 { dec!(100) } else { dec!(100.1) })
            .collect();
        let two_way: Vec<_> = (0..52)
            .map(|i| [dec!(100), dec!(96), dec!(100), dec!(97)][i % 4])
            .collect();
        let down: Vec<_> = (0..52)
            .map(|i| dec!(100) - Decimal::from(i) / dec!(2))
            .collect();
        let quiet = observe(&config, &noise, dec!(0.03));
        let range = observe(&config, &two_way, dec!(0.03));
        let trend = observe(&config, &down, dec!(0.03));
        assert_eq!(quiet.snapshot.reason, "QUIET");
        assert_eq!(quiet.snapshot.budget, Decimal::ZERO);
        assert_eq!(range.snapshot.reason, "TWO_WAY");
        assert!(range.snapshot.rebounds >= 2);
        assert_eq!(trend.snapshot.regime, MarketRegime::TrendDown);
        assert_eq!(trend.snapshot.budget, Decimal::ZERO);
        assert_eq!(trend.snapshot.efficiency, Decimal::ONE);
    }

    #[rstest]
    fn grid_scale_uses_actual_spacing_and_never_counts_a_gap_as_multiple_rebounds() {
        let config = GridScaleConfig {
            lookback_sessions: 2,
            ..Default::default()
        };
        let prices: Vec<_> = (0..52)
            .map(|i| if i % 2 == 0 { dec!(100) } else { dec!(96) })
            .collect();
        let wide = observe(&config, &prices, dec!(0.06));
        let narrow = observe(&config, &prices, dec!(0.03));
        assert_eq!(wide.snapshot.rebounds, 0);
        assert_eq!(narrow.snapshot.rebounds, 25);
        assert_eq!(
            rebound_opportunities(&[dec!(100), dec!(60), dec!(110)], dec!(0.03)).0,
            1
        );
    }

    #[rstest]
    fn grid_scale_recovery_preserves_hysteresis_memory_and_bounded_history() {
        let config = GridScaleConfig {
            lookback_sessions: 2,
            ..Default::default()
        };
        let prices: Vec<_> = (0..52)
            .map(|i| dec!(100) + Decimal::from(i) / dec!(2))
            .collect();
        let mut state = observe(&config, &prices, dec!(0.03));
        let mut restored: GridScaleRegime =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        restored.validate(&config, state.snapshot.ts_ns).unwrap();
        for i in 1..=26 {
            // 三个自然日后再开盘，不清空过去两个观测时段，也不造周末 K 线。
            let ts = OPEN + 4 * DAY + i * BAR_NS;
            state.update(&config, ts, dec!(128), dec!(0.03)).unwrap();
            restored.update(&config, ts, dec!(128), dec!(0.03)).unwrap();
            restored.validate(&config, ts).unwrap();
            assert_eq!(state.snapshot, restored.snapshot);
            assert!(state.closes.len() <= 52);
            assert!((Decimal::ZERO..=Decimal::ONE).contains(&state.snapshot.budget));
        }
        restored.snapshot.budget = dec!(1.01);
        assert!(restored.validate(&config, state.snapshot.ts_ns).is_err());
    }

    #[rstest]
    fn grid_scale_trend_hysteresis_and_future_suffix_are_causal() {
        let config = GridScaleConfig {
            lookback_sessions: 2,
            ..Default::default()
        };
        let prices: Vec<_> = (0..51)
            .map(|i| dec!(100) + Decimal::from(i) / dec!(2))
            .collect();
        let mut state = observe(&config, &prices, dec!(0.03));
        let prefix = state.snapshot.clone();
        // 本已处于上涨，回撤后效率落至进入门槛之下、退出门槛之上，应维持方向。
        let ts = OPEN + DAY + 27 * BAR_NS; // 16:15 非常规时段，必须拒绝。
        assert!(state.update(&config, ts, dec!(120), dec!(0.03)).is_err());
        assert_eq!(state.snapshot, prefix);
        let ts = OPEN + DAY + 26 * BAR_NS;
        state.update(&config, ts, dec!(118), dec!(0.03)).unwrap();
        assert_eq!(state.snapshot.regime, MarketRegime::TrendUp);
        assert!(state.snapshot.efficiency < config.trend_enter_efficiency);
        assert!(state.snapshot.efficiency > config.trend_exit_efficiency);
        assert_eq!(prefix.efficiency, Decimal::ONE);
        assert!(state.update(&config, ts, dec!(118), dec!(0.03)).is_err());
    }
}
