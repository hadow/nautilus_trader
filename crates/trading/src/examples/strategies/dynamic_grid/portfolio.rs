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

//! Shared-capital order admission, causal correlation and throttled instrument budgets.

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

/// Account-level fractions apply to current marked equity, not each instrument's virtual cash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortfolioConfig {
    /// Initial shared capital; external deposits require an audited restart/migration.
    pub capital: Decimal,
    /// Maximum per-order cost including reservations; also enforced by native execution risk.
    pub max_order_value: Decimal,
    /// Native submission throttle per rolling minute, covering buys and sells.
    pub max_orders_per_minute: usize,
    /// Maximum symbols with filled inventory or unresolved acquisitions.
    pub max_concurrent_symbols: usize,
    /// Operator-configured latched stop for new inventory.
    pub kill_switch: bool,
    /// Portfolio hard-limit action; recovery uncertainty always takes priority.
    pub risk_policy: RiskPolicy,
    /// Largest initial fraction assigned to one instrument.
    pub max_instrument_allocation: Decimal,
    /// Inventory plus all unresolved buy costs, as a fraction of equity.
    pub max_total_exposure: Decimal,
    /// Total grid inventory and pending buy costs / equity.
    pub max_total_grid_exposure: Decimal,
    /// Filled equity inventory / equity; also reserved before new buys.
    pub max_total_equity_exposure: Decimal,
    /// Cash unavailable for new orders / equity.
    pub min_cash_reserve: Decimal,
    /// Portfolio peak-to-trough loss fraction.
    pub max_portfolio_drawdown: Decimal,
    /// Loss since the preceding UTC day's final mark.
    pub max_portfolio_daily_loss: Decimal,
    /// Sector inventory and unresolved buys / equity. Missing sectors share an Unknown bucket.
    pub max_sector_exposure: Decimal,
    /// Connected correlated cluster inventory and unresolved buys / equity.
    pub max_correlated_exposure: Decimal,
    /// Positive Pearson correlation threshold for a cluster edge.
    pub correlation_threshold: f64,
    /// Maximum trailing completed UTC days retained for daily-return correlations.
    pub correlation_lookback_days: usize,
    /// Minimum aligned one-day return pairs; unknown correlation is conservatively treated as one.
    pub correlation_min_observations: usize,
    /// Minimum time between voluntary allocation changes.
    pub min_reallocation_interval_secs: u64,
    /// Minimum absolute allocation change as a fraction of portfolio equity.
    pub min_allocation_change_pct: Decimal,
    /// Bounded reduction in the base budget during an up trend.
    pub trend_up_allocation_factor: Decimal,
    /// Bounded reduction in the base budget during a down trend.
    pub trend_down_allocation_factor: Decimal,
    /// ATR/price at which volatility starts reducing the budget.
    pub allocation_volatility_target: Decimal,
    /// Budget response to cumulative marked sleeve returns, capped at the initial allocation.
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
    /// Validates finite ratios, time bounds and a nonempty correlation window.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits or an unrepresentable interval.
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

/// Current independent sleeve, including orders whose outcome is still unknown.
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

/// Admission result for a proposed acquisition. Covered reductions bypass acquisition limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderDecision {
    /// Full requested quantity fits every portfolio constraint.
    Allow,
    /// A smaller whole-lot quantity fits.
    Reduce,
    /// Transient lack of cash, capacity or fresh marks prevents submission.
    Defer,
    /// A latched portfolio failure or disabled instrument forbids submission.
    Reject,
}

/// Durable portfolio risk, budget and correlation history. No order has an independent cash pool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioRiskManager {
    /// Equity high-water mark, retained across process restarts.
    pub peak_equity: Decimal,
    /// Previous observation, including the prior day's close.
    pub last_equity: Decimal,
    /// Equity immediately before the current UTC day.
    pub day_start_equity: Decimal,
    /// Current UTC day.
    pub day: Option<u64>,
    /// A portfolio halt cannot be cleared by an instrument reset.
    pub risk_off_reason: Option<String>,
    /// Current permitted budget fractions, independent of held inventory.
    pub allocations: BTreeMap<InstrumentId, Decimal>,
    /// Last actual budget change for each instrument.
    pub last_reallocation_ns: BTreeMap<InstrumentId, u64>,
    /// Causal daily closes, including the incomplete current day which is excluded from correlation.
    closes: BTreeMap<InstrumentId, BTreeMap<u64, Decimal>>,
    /// Number of each gate outcome for operational inspection.
    pub decisions: [u64; 4],
    #[serde(skip)]
    matrix_day: Option<u64>,
    #[serde(skip)]
    matrix: BTreeMap<InstrumentId, BTreeMap<InstrumentId, Option<f64>>>,
}

impl PortfolioRiskManager {
    /// Initializes a single funded account risk baseline.
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

    /// Returns Pearson correlation using only aligned completed daily-return intervals.
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

    // ponytail: O(n^3) cluster expansion is bounded by the small configured stock universe;
    // replace with cached union-find if hundreds of simultaneous instruments are required.
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
