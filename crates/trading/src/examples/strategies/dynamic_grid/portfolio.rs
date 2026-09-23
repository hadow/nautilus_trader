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

//! 共享资金订单准入、因果相关性控制与带节流的单标的动态预算。

use std::collections::{BTreeMap, BTreeSet};

use nautilus_model::identifiers::InstrumentId;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use super::{
    config::RiskPolicy,
    engine::floor_tick,
    regime::{MarketRegime, is_fresh},
};

const DAY_NS: u64 = 86_400_000_000_000;

/// 账户级比例以当前组合权益为分母，而不是各标的虚拟现金。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortfolioConfig {
    /// 共享初始资金；外部入金必须通过可审计的重启或迁移流程处理。
    pub capital: Decimal,
    /// 单笔订单最大成本，包含预留；原生执行风控也会再次检查。
    pub max_order_value: Decimal,
    /// 滚动一分钟内的原生订单提交上限，买卖订单均计入。
    pub max_orders_per_minute: usize,
    /// 同时存在真实库存或未终结买单的最大标的数量。
    pub max_concurrent_symbols: usize,
    /// 由操作员配置、触发后锁存的新增库存停止开关。
    pub kill_switch: bool,
    /// 组合硬性限制的处置方式；恢复状态不确定时始终优先保证安全。
    pub risk_policy: RiskPolicy,
    /// 单标的允许配置的最大初始资金比例。
    pub max_instrument_allocation: Decimal,
    /// 库存与全部未终结买单成本占组合权益的最大比例。
    pub max_total_exposure: Decimal,
    /// 全部网格仓库存与待买成本占组合权益的最大比例。
    pub max_total_grid_exposure: Decimal,
    /// 已成交股票库存占组合权益的最大比例；新买入前也会预留容量。
    pub max_total_equity_exposure: Decimal,
    /// 不可被新订单占用的最低现金储备占权益比例。
    pub min_cash_reserve: Decimal,
    /// 组合权益从高水位到低点的最大亏损比例。
    pub max_portfolio_drawdown: Decimal,
    /// 相对前一 UTC 日最终权益的最大日内亏损。
    pub max_portfolio_daily_loss: Decimal,
    /// 同行业库存与未终结买单占权益的上限；缺失行业统一归入 Unknown。
    pub max_sector_exposure: Decimal,
    /// 相关性连通簇的库存与未终结买单占权益的上限。
    pub max_correlated_exposure: Decimal,
    /// 构成相关性连通边的正 Pearson 相关系数阈值。
    pub correlation_threshold: f64,
    /// 日收益相关性保留的最大滚动已完成 UTC 日数。
    pub correlation_lookback_days: usize,
    /// 计算相关性所需的最少对齐日收益样本；样本不足时保守按完全相关处理。
    pub correlation_min_observations: usize,
    /// 两次主动调整资金预算之间的最短时间。
    pub min_reallocation_interval_secs: u64,
    /// 触发预算调整所需的最小绝对比例变化。
    pub min_allocation_change_pct: Decimal,
    /// 上升趋势中对基础预算使用的受限缩减系数。
    pub trend_up_allocation_factor: Decimal,
    /// 下降趋势中对基础预算使用的受限缩减系数。
    pub trend_down_allocation_factor: Decimal,
    /// 开始因波动率缩减预算的 ATR/价格目标值。
    pub allocation_volatility_target: Decimal,
    /// 预算对单标的累计按市值收益的响应强度，最终不超过初始分配。
    pub allocation_pnl_weight: Decimal,
}

impl Default for PortfolioConfig {
    fn default() -> Self {
        Self {
            capital: Decimal::from(100_000),
            max_order_value: Decimal::from(20_000),
            max_orders_per_minute: 120,
            max_concurrent_symbols: 16,
            kill_switch: false,
            risk_policy: RiskPolicy::Hold,
            max_instrument_allocation: Decimal::new(20, 2),
            max_total_exposure: Decimal::new(60, 2),
            max_total_grid_exposure: Decimal::new(60, 2),
            max_total_equity_exposure: Decimal::new(60, 2),
            min_cash_reserve: Decimal::new(30, 2),
            max_portfolio_drawdown: Decimal::new(15, 2),
            max_portfolio_daily_loss: Decimal::new(5, 2),
            max_sector_exposure: Decimal::new(30, 2),
            max_correlated_exposure: Decimal::new(30, 2),
            correlation_threshold: 0.8,
            correlation_lookback_days: 60,
            correlation_min_observations: 20,
            min_reallocation_interval_secs: 3600,
            min_allocation_change_pct: Decimal::new(1, 2),
            trend_up_allocation_factor: Decimal::new(75, 2),
            trend_down_allocation_factor: Decimal::new(25, 2),
            allocation_volatility_target: Decimal::new(2, 2),
            allocation_pnl_weight: Decimal::from(2),
        }
    }
}

impl PortfolioConfig {
    /// 校验比例有限性、时间边界及非空相关性窗口。
    ///
    /// # Errors
    ///
    /// 限制无效或时间间隔无法表示时返回错误。
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.capital > Decimal::ZERO
                && self.max_order_value > Decimal::ZERO
                && self.max_orders_per_minute > 0
                && self.max_concurrent_symbols > 0,
            "Portfolio capital and order limits must be positive"
        );
        for (name, value) in [
            ("max_instrument_allocation", self.max_instrument_allocation),
            ("max_total_exposure", self.max_total_exposure),
            ("max_total_grid_exposure", self.max_total_grid_exposure),
            ("max_total_equity_exposure", self.max_total_equity_exposure),
            ("max_portfolio_drawdown", self.max_portfolio_drawdown),
            ("max_portfolio_daily_loss", self.max_portfolio_daily_loss),
            ("max_sector_exposure", self.max_sector_exposure),
            ("max_correlated_exposure", self.max_correlated_exposure),
            (
                "allocation_volatility_target",
                self.allocation_volatility_target,
            ),
        ] {
            anyhow::ensure!(
                value > Decimal::ZERO && value <= Decimal::ONE,
                "Invalid {name}"
            );
        }
        for value in [
            self.min_cash_reserve,
            self.min_allocation_change_pct,
            self.trend_up_allocation_factor,
            self.trend_down_allocation_factor,
        ] {
            anyhow::ensure!(
                (Decimal::ZERO..=Decimal::ONE).contains(&value),
                "Invalid allocation fraction"
            );
        }
        anyhow::ensure!(
            self.min_cash_reserve < Decimal::ONE
                && self.allocation_pnl_weight >= Decimal::ZERO
                && self.correlation_threshold.is_finite()
                && (0.0..=1.0).contains(&self.correlation_threshold)
                && self.correlation_min_observations >= 2
                && self.correlation_lookback_days > self.correlation_min_observations
                && self.correlation_lookback_days.checked_add(2).is_some()
                && self.min_reallocation_interval_secs <= u64::MAX / 1_000_000_000,
            "Invalid portfolio allocation/correlation window"
        );
        Ok(())
    }
}

/// 单标的当前独立暴露快照，包含结果仍未知的订单。
#[derive(Clone, Debug)]
pub(super) struct InstrumentExposure {
    pub id: InstrumentId,
    pub sector: Option<String>,
    pub base_allocation: Decimal,
    pub max_position_pct: Decimal,
    pub enabled: bool,
    pub cash_delta: Decimal,
    pub exposure: Decimal,
    pub pending: Decimal,
    pub net_pnl: Decimal,
    pub regime: MarketRegime,
    pub atr_pct: Decimal,
    pub risk_off: bool,
    pub mark_ns: u64,
    pub max_age_secs: u64,
}

/// 新增库存订单的组合准入结果；有库存覆盖的减仓不受新增风险限制。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderDecision {
    /// 全部请求数量均满足组合约束。
    Allow,
    /// 仅较小的整 lot 数量满足约束。
    Reduce,
    /// 因暂时缺少现金、风险容量或新鲜行情而延后提交。
    Defer,
    /// 组合风险已锁存或标的已禁用，拒绝提交。
    Reject,
}

/// 可持久化的组合风险、预算与相关性历史；任何订单都没有独立现金池。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioRiskManager {
    /// 组合权益高水位，进程重启后继续保留。
    pub peak_equity: Decimal,
    /// 最近一次权益观测，包含前一交易日收盘。
    pub last_equity: Decimal,
    /// 当前 UTC 日开始前的组合权益。
    pub day_start_equity: Decimal,
    /// 当前 UTC 日期。
    pub day: Option<u64>,
    /// 组合级停机原因不能被单标的重置清除。
    pub risk_off_reason: Option<String>,
    /// 当前允许的单标的预算比例，与已持库存分开记录。
    pub allocations: BTreeMap<InstrumentId, Decimal>,
    /// 各标的最近一次实际预算变更时间。
    pub last_reallocation_ns: BTreeMap<InstrumentId, u64>,
    /// 因果日收盘历史；包含但不使用尚未完成的当前日计算相关性。
    closes: BTreeMap<InstrumentId, BTreeMap<u64, Decimal>>,
    /// 各类准入结果的累计次数，供运行监控与审计。
    pub decisions: [u64; 4],
    #[serde(skip)]
    matrix_day: Option<u64>,
    #[serde(skip)]
    matrix: BTreeMap<InstrumentId, BTreeMap<InstrumentId, Option<f64>>>,
}

impl PortfolioRiskManager {
    /// 使用单一账户初始资金建立组合风险基线。
    #[must_use]
    pub fn new(capital: Decimal) -> Self {
        Self {
            peak_equity: capital,
            last_equity: capital,
            day_start_equity: capital,
            day: None,
            risk_off_reason: None,
            allocations: BTreeMap::new(),
            last_reallocation_ns: BTreeMap::new(),
            closes: BTreeMap::new(),
            decisions: [0; 4],
            matrix_day: None,
            matrix: BTreeMap::new(),
        }
    }

    pub(super) fn trip(&mut self, reason: impl Into<String>) {
        if self.risk_off_reason.is_none() {
            let reason = reason.into();
            log::warn!("PORTFOLIO_RISK_OFF reason={reason}");
            self.risk_off_reason = Some(reason);
        }
    }

    pub(super) fn observe(&mut self, config: &PortfolioConfig, equity: Decimal, now: u64) {
        let day = now / DAY_NS;
        if self.day != Some(day) {
            self.day = Some(day);
            self.day_start_equity = self.last_equity;
        }
        self.peak_equity = self.peak_equity.max(equity);
        if config.kill_switch {
            self.trip("Operator kill switch");
        } else if equity <= Decimal::ZERO {
            self.trip("Nonpositive portfolio equity");
        } else if self.peak_equity - equity >= self.peak_equity * config.max_portfolio_drawdown {
            self.trip("Maximum portfolio drawdown");
        } else if self.day_start_equity - equity
            >= self.day_start_equity * config.max_portfolio_daily_loss
        {
            self.trip("Maximum portfolio daily loss");
        }
        self.last_equity = equity;
    }

    pub(super) fn close(
        &mut self,
        config: &PortfolioConfig,
        id: InstrumentId,
        now: u64,
        price: Decimal,
    ) {
        if self.matrix_day.is_some_and(|day| now / DAY_NS < day) {
            self.matrix_day = None;
        }
        let days = self.closes.entry(id).or_default();
        days.insert(now / DAY_NS, price);
        while days.len() > config.correlation_lookback_days + 2 {
            days.pop_first();
        }
    }

    /// 仅使用相互对齐且已经完成的日收益区间计算 Pearson 相关系数。
    #[must_use]
    pub fn correlation(
        &self,
        config: &PortfolioConfig,
        a: InstrumentId,
        b: InstrumentId,
        now: u64,
    ) -> Option<f64> {
        let day = now / DAY_NS;
        let returns = |id| -> Option<BTreeMap<(u64, u64), f64>> {
            let days = self.closes.get(&id)?;
            let points: Vec<_> = days
                .iter()
                .filter(|(d, _)| {
                    **d < day && day.saturating_sub(**d) <= config.correlation_lookback_days as u64
                })
                .collect();
            Some(
                points
                    .windows(2)
                    .filter_map(|p| {
                        let value = (*p[1].1 / *p[0].1 - Decimal::ONE).to_f64()?;
                        Some(((*p[0].0, *p[1].0), value))
                    })
                    .collect(),
            )
        };
        let a = returns(a)?;
        let b = returns(b)?;
        let pairs: Vec<_> = a
            .iter()
            .filter_map(|(day, x)| b.get(day).map(|y| (*x, *y)))
            .collect();
        if pairs.len() < config.correlation_min_observations {
            return None;
        }
        let n = pairs.len() as f64;
        let mx = pairs.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pairs.iter().map(|p| p.1).sum::<f64>() / n;
        let cov = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum::<f64>();
        let vx = pairs.iter().map(|(x, _)| (x - mx).powi(2)).sum::<f64>();
        let vy = pairs.iter().map(|(_, y)| (y - my).powi(2)).sum::<f64>();
        let value = cov / (vx * vy).sqrt();
        value.is_finite().then(|| value.clamp(-1.0, 1.0))
    }

    pub(super) fn reallocate(
        &mut self,
        c: &PortfolioConfig,
        instruments: &[InstrumentExposure],
        now: u64,
    ) {
        for view in instruments {
            let current = *self
                .allocations
                .entry(view.id)
                .or_insert(view.base_allocation);
            let hard_stop =
                !view.enabled || view.risk_off || view.regime == MarketRegime::HighVolatility;
            let mut factor = match view.regime {
                MarketRegime::TrendUp => c.trend_up_allocation_factor,
                MarketRegime::TrendDown => c.trend_down_allocation_factor,
                _ => Decimal::ONE,
            };
            if view.atr_pct > c.allocation_volatility_target {
                factor *= c.allocation_volatility_target / view.atr_pct;
            }
            let funded = c.capital * view.base_allocation;
            if funded > Decimal::ZERO {
                factor *= (Decimal::ONE + c.allocation_pnl_weight * view.net_pnl / funded)
                    .clamp(Decimal::ZERO, Decimal::ONE);
            }
            // 调整的是新增仓位预算，不是立即调仓；盈利不会把预算放大到初始分配以上
            let target = if hard_stop {
                Decimal::ZERO
            } else {
                view.base_allocation * factor
            };
            let elapsed = self.last_reallocation_ns.get(&view.id).is_none_or(|t| {
                now.saturating_sub(*t) / 1_000_000_000 >= c.min_reallocation_interval_secs
            });
            if target != current
                && (hard_stop
                    || (elapsed && (target - current).abs() >= c.min_allocation_change_pct))
            {
                self.allocations.insert(view.id, target);
                self.last_reallocation_ns.insert(view.id, now);
                log::info!(
                    "CAPITAL_REALLOCATED instrument={} previous={current} target={target}",
                    view.id
                );
            }
        }
    }

    // ponytail: 当前配置的股票池很小，O(n^3) 相关簇扩展有明确上界；
    // 只有需要同时管理数百个标的时，才改用缓存的并查集。
    fn cluster(
        &mut self,
        c: &PortfolioConfig,
        views: &[InstrumentExposure],
        id: InstrumentId,
        now: u64,
    ) -> BTreeSet<InstrumentId> {
        self.refresh_matrix(c, views, now);
        let mut cluster = BTreeSet::from([id]);
        loop {
            let previous = cluster.len();
            for view in views {
                if cluster.iter().any(|member| {
                    self.matrix[member][&view.id].unwrap_or(1.0) > c.correlation_threshold
                }) {
                    cluster.insert(view.id);
                }
            }
            if cluster.len() == previous {
                return cluster;
            }
        }
    }

    fn refresh_matrix(&mut self, c: &PortfolioConfig, views: &[InstrumentExposure], now: u64) {
        if self.matrix_day == Some(now / DAY_NS) && self.matrix.len() == views.len() {
            return;
        }
        self.matrix = views
            .iter()
            .map(|a| {
                (
                    a.id,
                    views
                        .iter()
                        .map(|b| {
                            (
                                b.id,
                                if a.id == b.id {
                                    Some(1.0)
                                } else {
                                    self.correlation(c, a.id, b.id, now)
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        self.matrix_day = Some(now / DAY_NS);
    }

    pub(super) fn correlation_matrix(
        &mut self,
        c: &PortfolioConfig,
        views: &[InstrumentExposure],
        now: u64,
    ) -> BTreeMap<InstrumentId, BTreeMap<InstrumentId, Option<f64>>> {
        self.refresh_matrix(c, views, now);
        self.matrix.clone()
    }

    pub(super) fn concentration_breaches(
        &mut self,
        c: &PortfolioConfig,
        views: &[InstrumentExposure],
        equity: Decimal,
        now: u64,
    ) -> BTreeSet<InstrumentId> {
        let mut blocked = BTreeSet::new();
        for view in views {
            let sector: Decimal = views
                .iter()
                .filter(|v| v.sector == view.sector)
                .map(|v| v.exposure + v.pending)
                .sum();
            let cluster = self.cluster(c, views, view.id, now);
            let correlated: Decimal = views
                .iter()
                .filter(|v| cluster.contains(&v.id))
                .map(|v| v.exposure + v.pending)
                .sum();
            if sector > equity * c.max_sector_exposure
                || correlated > equity * c.max_correlated_exposure
            {
                blocked.insert(view.id);
            }
        }
        blocked
    }

    pub(super) fn validate(
        &self,
        config: &PortfolioConfig,
        ids: &BTreeSet<InstrumentId>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.peak_equity > Decimal::ZERO
                && self.peak_equity >= self.last_equity
                && self.day_start_equity <= self.peak_equity,
            "Invalid recovered portfolio equity baseline"
        );
        for (id, closes) in &self.closes {
            anyhow::ensure!(
                ids.contains(id)
                    && closes.len() <= config.correlation_lookback_days + 2
                    && closes.values().all(|p| *p > Decimal::ZERO),
                "Invalid recovered correlation history"
            );
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "explicit current risk snapshot at the sole order gate"
    )]
    pub(super) fn admit(
        &mut self,
        c: &PortfolioConfig,
        views: &[InstrumentExposure],
        id: InstrumentId,
        equity: Decimal,
        cash: Decimal,
        broker_free: Decimal,
        desired: Decimal,
        unit_cost: Decimal,
        lot: Decimal,
        now: u64,
    ) -> (OrderDecision, Decimal) {
        let (decision, quantity) = self.capacity(
            c,
            views,
            id,
            equity,
            cash,
            broker_free,
            desired,
            unit_cost,
            lot,
            now,
        );
        self.decisions[match decision {
            OrderDecision::Allow => 0,
            OrderDecision::Reduce => 1,
            OrderDecision::Defer => 2,
            OrderDecision::Reject => 3,
        }] += 1;
        (decision, quantity)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "explicit current risk snapshot at the sole order gate"
    )]
    fn capacity(
        &mut self,
        c: &PortfolioConfig,
        views: &[InstrumentExposure],
        id: InstrumentId,
        equity: Decimal,
        cash: Decimal,
        broker_free: Decimal,
        desired: Decimal,
        unit_cost: Decimal,
        lot: Decimal,
        now: u64,
    ) -> (OrderDecision, Decimal) {
        let Some(view) = views.iter().find(|v| v.id == id) else {
            return (OrderDecision::Reject, Decimal::ZERO);
        };
        if self.risk_off_reason.is_some()
            || c.kill_switch
            || !view.enabled
            || view.risk_off
            || equity <= Decimal::ZERO
            || unit_cost <= Decimal::ZERO
            || lot <= Decimal::ZERO
        {
            return (OrderDecision::Reject, Decimal::ZERO);
        }
        if view.exposure + view.pending <= Decimal::ZERO
            && views
                .iter()
                .filter(|v| v.exposure + v.pending > Decimal::ZERO)
                .count()
                >= c.max_concurrent_symbols
        {
            return (OrderDecision::Defer, Decimal::ZERO);
        }
        // 任一已持仓股票报价过期，就无法可靠评估总权益，其他股票也不能继续占用资金
        if views
            .iter()
            .any(|v| v.exposure > Decimal::ZERO && !is_fresh(v.mark_ns, now, v.max_age_secs))
        {
            return (OrderDecision::Defer, Decimal::ZERO);
        }
        let pending: Decimal = views.iter().map(|v| v.pending).sum();
        let total: Decimal = views.iter().map(|v| v.exposure + v.pending).sum();
        let sector: Decimal = views
            .iter()
            .filter(|v| v.sector == view.sector)
            .map(|v| v.exposure + v.pending)
            .sum();
        let cluster = self.cluster(c, views, id, now);
        let correlated: Decimal = views
            .iter()
            .filter(|v| cluster.contains(&v.id))
            .map(|v| v.exposure + v.pending)
            .sum();
        let allocation = self
            .allocations
            .get(&id)
            .copied()
            .unwrap_or(view.base_allocation)
            .min(c.max_instrument_allocation)
            .min(view.max_position_pct);
        // 所有限额取最小可用值，pending 包含其他股票及撤单待确认订单，不能重复花钱
        let capacity = (cash - pending - equity * c.min_cash_reserve)
            .min(c.max_order_value)
            .min(broker_free - pending)
            .min(
                equity
                    * c.max_total_exposure
                        .min(c.max_total_grid_exposure)
                        .min(c.max_total_equity_exposure)
                    - total,
            )
            .min(equity * c.max_sector_exposure - sector)
            .min(equity * c.max_correlated_exposure - correlated)
            .min(equity * allocation - view.exposure - view.pending);
        let quantity = floor_tick(desired.min(capacity / unit_cost).max(Decimal::ZERO), lot);
        let decision = if quantity <= Decimal::ZERO {
            OrderDecision::Defer
        } else if quantity < desired {
            OrderDecision::Reduce
        } else {
            OrderDecision::Allow
        };
        (decision, quantity)
    }
}
