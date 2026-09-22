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

//! Equity-path and grid-cycle metrics. Ratios are statistics; cash accounting stays Decimal.

use std::collections::BTreeMap;

use nautilus_core::UnixNanos;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use super::{
    diagnostics::GridDiagnostics,
    orders::{GridCycle, OrderManager, PositionComponent},
    regime::MarketRegime,
    risk::RiskManager,
};

/// One causal marked-equity observation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EquityPoint {
    /// Aggregate marked inventory of independently trending sleeves; None uses this point's regime.
    #[serde(default)]
    pub trend_inventory: Option<Decimal>,
    /// Cumulative charged/estimated fees known at this timestamp, for causal cost diagnostics.
    #[serde(default)]
    pub cumulative_fees: Decimal,
    /// Cumulative traded notional known at this timestamp, for incremental slippage stress.
    #[serde(default)]
    pub cumulative_turnover: Decimal,
    /// Observation timestamp.
    pub ts_ns: u64,
    /// Price used to mark inventory.
    pub price: Decimal,
    /// Strategy cash plus marked inventory.
    pub equity: Decimal,
    /// Marked inventory.
    pub exposure: Decimal,
    /// Filled quantity.
    pub position: Decimal,
    /// Inventory plus unresolved buy reservations divided by equity.
    pub utilization: f64,
    /// Current market classification.
    pub regime: MarketRegime,
}

/// Shared output captured by runners without depending on a particular execution client.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PerformanceTracker {
    /// Passive diagnostics; absent legacy observations remain empty rather than reconstructed.
    #[serde(default)]
    pub diagnostics: GridDiagnostics,
    /// Marked observations at completed bars and final shutdown.
    pub equity: Vec<EquityPoint>,
    /// Completed inventory cycles.
    pub cycles: Vec<GridCycle>,
    /// Computed metrics, including open inventory in total return and drawdown.
    pub metrics: GridMetrics,
    /// Latched hard-limit reason, if any.
    pub risk_off_reason: Option<String>,
    #[serde(default)]
    mark_peak: Decimal,
    #[serde(default)]
    mark_peak_ns: Option<u64>,
}

/// Report statistics; undefined ratios are None rather than fabricated infinities.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GridMetrics {
    /// Annualized standard deviation of observed daily marked returns, using 252 days.
    #[serde(default)]
    pub annualized_volatility: Option<f64>,
    /// Exact marked PnL per observed UTC date, including open inventory and fees.
    #[serde(default)]
    pub daily_pnl: BTreeMap<String, Decimal>,
    /// Exact marked PnL per observed UTC month, including unrecovered inventory losses.
    #[serde(default)]
    pub monthly_pnl: BTreeMap<String, Decimal>,
    /// Fraction of retained order intents with at least one fill; partial fills count once.
    #[serde(default)]
    pub grid_fill_rate: Option<f64>,
    /// Lifetime completed resets across all instruments.
    #[serde(default)]
    pub total_grid_resets: u64,
    /// Absolute traded notional, not the normalized turnover ratio.
    #[serde(default)]
    pub turnover: Decimal,
    /// Final marked return / initial capital.
    pub total_return: f64,
    /// Geometrically annualized return (same definition as CAGR).
    pub annualized_return: Option<f64>,
    /// Compound annual growth rate, requiring positive terminal equity.
    pub cagr: Option<f64>,
    /// Zero-risk-free daily Sharpe, 252 trading days per year.
    pub sharpe: Option<f64>,
    /// Daily downside deviation Sortino.
    pub sortino: Option<f64>,
    /// Maximum peak-to-trough equity decline fraction.
    pub max_drawdown: f64,
    /// CAGR divided by maximum drawdown.
    pub calmar: Option<f64>,
    /// Maximum duration below the prior high-water mark, including unrecovered tails.
    pub drawdown_duration_secs: f64,
    /// Winning completed cycle fraction.
    pub win_rate: Option<f64>,
    /// Winning tactical grid-cycle fraction, excluding core rebalances.
    #[serde(default)]
    pub grid_cycle_win_rate: Option<f64>,
    /// Positive completed net profits divided by negative completed net losses.
    pub profit_factor: Option<f64>,
    /// Number of unique executions, including partial fills.
    pub number_of_trades: u64,
    /// Number of completed inventory cycles.
    pub number_of_grid_cycles: usize,
    /// Completed core-sleeve reductions, excluded from grid-cycle statistics.
    #[serde(default)]
    pub number_of_core_rebalances: usize,
    /// Average completed cycle net profit.
    pub average_grid_profit: Option<f64>,
    /// Mean first-entry-to-final-exit duration.
    pub average_holding_time_secs: Option<f64>,
    /// Completed-cycle profit at decision prices before fees and slippage.
    pub gross_pnl: Decimal,
    /// Total marked PnL including unsold inventory and all fees.
    pub net_pnl: Decimal,
    /// Realized PnL, including partial reductions.
    pub realized_pnl: Decimal,
    /// Realized PnL attributed to the long-lived core sleeve.
    #[serde(default)]
    pub core_realized_pnl: Decimal,
    /// Realized PnL attributed to tactical grid inventory.
    #[serde(default)]
    pub grid_realized_pnl: Decimal,
    /// Marked PnL attributable to unsold inventory, including its remaining entry costs.
    #[serde(default)]
    pub unrealized_pnl: Decimal,
    /// All entry/exit fees, including open inventory.
    pub fees: Decimal,
    /// Number of fills with estimated instead of reported fees.
    pub estimated_fee_fills: u64,
    /// Signed execution shortfall; already included in PnL.
    pub slippage: Decimal,
    /// Time-weighted mean capital utilization.
    pub capital_utilization: f64,
    /// Total marked return divided by time-weighted capital utilization; undefined when unused.
    pub capital_efficiency: Option<f64>,
    /// Maximum marked exposure.
    pub maximum_exposure: Decimal,
    /// Maximum filled quantity.
    pub maximum_position: Decimal,
    /// Time-weighted mean filled quantity.
    #[serde(default)]
    pub average_inventory: f64,
    /// Elapsed seconds with nonzero filled inventory.
    #[serde(default)]
    pub inventory_duration_secs: f64,
    /// Maximum resets without a profitable completed cycle.
    pub maximum_grid_reset_count: u32,
    /// Realized completed grid profit divided by initial capital.
    pub grid_efficiency: f64,
    /// Traded notional divided by initial capital.
    pub grid_turnover: f64,
    /// Completed net cycle profit / completed decision-price gross profit.
    pub grid_capture_ratio: Option<f64>,
    /// Total fees / positive completed decision-price gross profit.
    pub fee_gross_profit_ratio: Option<f64>,
    /// Time-weighted inventory exposure / initial capital.
    pub inventory_exposure: f64,
    /// Long-only directional exposure, equal to inventory exposure.
    pub directional_exposure: f64,
    /// Time-weighted trend inventory exposure / initial capital.
    pub trend_exposure: f64,
    /// Resets per elapsed day.
    pub reset_frequency: f64,
    /// Worst completed tactical grid cycle.
    #[serde(default)]
    pub worst_grid_cycle: Option<Decimal>,
    /// Marked equity changes assigned to the causal regime at interval start.
    #[serde(default)]
    pub regime_pnl: BTreeMap<String, Decimal>,
}

impl PerformanceTracker {
    /// Computes portfolio statistics from its own marked path and independent exact-money ledgers.
    pub fn finish_portfolio(
        &mut self,
        capital: Decimal,
        ledgers: &[(&OrderManager, &RiskManager)],
        risk_off_reason: Option<String>,
    ) {
        let mut merged = OrderManager::new(capital);
        let mut risk = RiskManager::new(capital);
        risk.risk_off_reason = risk_off_reason;
        for (orders, local_risk) in ledgers {
            merged.merge_report(orders);
            risk.total_resets += local_risk.total_resets;
            risk.maximum_reset_count = risk.maximum_reset_count.max(local_risk.maximum_reset_count);
        }
        merged
            .cycles
            .sort_by(|a, b| a.entry_order_id.cmp(&b.entry_order_id));
        self.finish(capital, &merged, &risk);
        let submitted: usize = ledgers
            .iter()
            .map(|(o, _)| {
                o.orders()
                    .values()
                    .filter(|order| order.component == PositionComponent::Grid)
                    .count()
            })
            .sum();
        let filled: usize = ledgers
            .iter()
            .map(|(o, _)| {
                o.orders()
                    .values()
                    .filter(|order| {
                        order.component == PositionComponent::Grid && order.filled > Decimal::ZERO
                    })
                    .count()
            })
            .sum();
        self.metrics.grid_fill_rate = (submitted > 0).then(|| filled as f64 / submitted as f64);
    }

    /// Tracks intrabar risk extrema without retaining every quote in memory.
    pub fn observe_mark(
        &mut self,
        capital: Decimal,
        equity: Decimal,
        exposure: Decimal,
        position: Decimal,
        now: u64,
    ) {
        self.mark_peak = self.mark_peak.max(capital);
        let peak_ns = self.mark_peak_ns.get_or_insert(now);
        if equity >= self.mark_peak {
            self.mark_peak = equity;
            *peak_ns = now;
        } else if self.mark_peak > Decimal::ZERO {
            self.metrics.max_drawdown = self
                .metrics
                .max_drawdown
                .max(number((self.mark_peak - equity) / self.mark_peak));
            self.metrics.drawdown_duration_secs = self
                .metrics
                .drawdown_duration_secs
                .max(now.saturating_sub(*peak_ns) as f64 / 1e9);
        }
        self.metrics.maximum_exposure = self.metrics.maximum_exposure.max(exposure);
        self.metrics.maximum_position = self.metrics.maximum_position.max(position);
    }

    /// Updates final statistics from the order/risk ledgers.
    pub fn finish(&mut self, capital: Decimal, orders: &OrderManager, risk: &RiskManager) {
        self.cycles.clone_from(&orders.cycles);
        self.risk_off_reason.clone_from(&risk.risk_off_reason);
        let m = &mut self.metrics;
        let Some(last) = self.equity.last() else {
            return;
        };
        let first = &self.equity[0];
        let seconds = last.ts_ns.saturating_sub(first.ts_ns) as f64 / 1e9;
        m.net_pnl = last.equity - capital;
        m.total_return = number(m.net_pnl / capital);
        m.cagr = if seconds >= 86_400.0 && last.equity > Decimal::ZERO {
            finite((1.0 + m.total_return).powf(365.25 * 86_400.0 / seconds) - 1.0)
        } else {
            None
        };
        m.annualized_return = m.cagr;
        let mut peak = capital;
        let mut peak_time = first.ts_ns;
        let mut daily = std::collections::BTreeMap::new();
        let mut daily_equity = BTreeMap::new();
        let mut monthly_equity = BTreeMap::new();
        let mut exposure_time = 0.0;
        let mut inventory_time = 0.0;
        let mut inventory_duration = 0.0;
        let mut trend_time = 0.0;
        let mut utilization_time = 0.0;
        let mut regime_pnl = BTreeMap::new();
        for (i, point) in self.equity.iter().enumerate() {
            if point.equity >= peak {
                peak = point.equity;
                peak_time = point.ts_ns;
            } else {
                m.max_drawdown = m.max_drawdown.max(number((peak - point.equity) / peak));
                m.drawdown_duration_secs = m
                    .drawdown_duration_secs
                    .max(point.ts_ns.saturating_sub(peak_time) as f64 / 1e9);
            }
            daily.insert(point.ts_ns / 86_400_000_000_000, number(point.equity));
            let date = UnixNanos::from(point.ts_ns).to_rfc3339();
            daily_equity.insert(date[..10].to_string(), point.equity);
            monthly_equity.insert(date[..7].to_string(), point.equity);
            m.maximum_exposure = m.maximum_exposure.max(point.exposure);
            m.maximum_position = m.maximum_position.max(point.position);
            if let Some(next) = self.equity.get(i + 1) {
                let dt = next.ts_ns.saturating_sub(point.ts_ns) as f64 / 1e9;
                exposure_time += number(point.exposure / capital) * dt;
                inventory_time += number(point.position) * dt;
                if point.position > Decimal::ZERO {
                    inventory_duration += dt;
                }
                utilization_time += point.utilization * dt;
                let trend = point.trend_inventory.unwrap_or({
                    if matches!(
                        point.regime,
                        MarketRegime::TrendUp | MarketRegime::TrendDown
                    ) {
                        point.exposure
                    } else {
                        Decimal::ZERO
                    }
                });
                trend_time += number(trend / capital) * dt;
                *regime_pnl
                    .entry(format!("{:?}", point.regime))
                    .or_insert(Decimal::ZERO) += next.equity - point.equity;
            }
        }
        m.regime_pnl = regime_pnl;
        let mut previous = number(capital);
        let changes = |mut values: BTreeMap<String, Decimal>| {
            let mut previous = capital;
            for value in values.values_mut() {
                let equity = *value;
                *value -= previous;
                previous = equity;
            }
            values
        };
        m.daily_pnl = changes(daily_equity);
        m.monthly_pnl = changes(monthly_equity);
        let mut returns = Vec::new();
        for value in daily.values() {
            if previous > 0.0 {
                returns.push(value / previous - 1.0);
            }
            previous = *value;
        }
        if returns.len() >= 2 {
            let mean = returns.iter().sum::<f64>() / returns.len() as f64;
            let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                / (returns.len() - 1) as f64;
            let downside =
                returns.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / returns.len() as f64;
            m.annualized_volatility = finite(variance.sqrt() * 252.0_f64.sqrt());
            m.sharpe = (variance > 0.0)
                .then(|| mean / variance.sqrt() * 252.0_f64.sqrt())
                .and_then(finite);
            m.sortino = (downside > 0.0)
                .then(|| mean / downside.sqrt() * 252.0_f64.sqrt())
                .and_then(finite);
        }
        m.calmar = m
            .cagr
            .filter(|_| m.max_drawdown > 0.0)
            .map(|r| r / m.max_drawdown);
        let count = self.cycles.len();
        let grid_cycles: Vec<_> = self
            .cycles
            .iter()
            .filter(|cycle| cycle.component == PositionComponent::Grid)
            .collect();
        let grid_count = grid_cycles.len();
        let grid_profit: Decimal = grid_cycles.iter().map(|cycle| cycle.net_pnl).sum();
        let grid_gross: Decimal = grid_cycles.iter().map(|cycle| cycle.gross_pnl).sum();
        let wins: Decimal = self
            .cycles
            .iter()
            .map(|c| c.net_pnl.max(Decimal::ZERO))
            .sum();
        let losses: Decimal = self
            .cycles
            .iter()
            .map(|c| -c.net_pnl.min(Decimal::ZERO))
            .sum();
        m.win_rate = (count > 0).then(|| {
            self.cycles
                .iter()
                .filter(|c| c.net_pnl > Decimal::ZERO)
                .count() as f64
                / count as f64
        });
        m.profit_factor = (losses > Decimal::ZERO).then(|| number(wins / losses));
        m.grid_cycle_win_rate = (grid_count > 0).then(|| {
            grid_cycles
                .iter()
                .filter(|cycle| cycle.net_pnl > Decimal::ZERO)
                .count() as f64
                / grid_count as f64
        });
        m.number_of_trades = orders.fill_count;
        m.turnover = orders.turnover;
        m.total_grid_resets = risk.total_resets;
        let grid_orders: Vec<_> = orders
            .orders()
            .values()
            .filter(|order| order.component == PositionComponent::Grid)
            .collect();
        m.grid_fill_rate = (!grid_orders.is_empty()).then(|| {
            grid_orders
                .iter()
                .filter(|order| order.filled > Decimal::ZERO)
                .count() as f64
                / grid_orders.len() as f64
        });
        m.number_of_grid_cycles = grid_count;
        m.number_of_core_rebalances = count - grid_count;
        m.average_grid_profit = (grid_count > 0).then(|| number(grid_profit) / grid_count as f64);
        m.average_holding_time_secs = (grid_count > 0).then(|| {
            grid_cycles
                .iter()
                .map(|cycle| cycle.holding_ns as f64 / 1e9)
                .sum::<f64>()
                / grid_count as f64
        });
        m.gross_pnl = self.cycles.iter().map(|c| c.gross_pnl).sum();
        m.realized_pnl = orders.realized_pnl;
        m.core_realized_pnl = orders.component_realized_pnl(PositionComponent::Core);
        m.grid_realized_pnl = orders.component_realized_pnl(PositionComponent::Grid);
        m.unrealized_pnl = m.net_pnl - m.realized_pnl;
        m.fees = orders.fees;
        m.estimated_fee_fills = orders.estimated_fee_fills;
        m.slippage = orders.slippage;
        m.capital_utilization = if seconds > 0.0 {
            utilization_time / seconds
        } else {
            0.0
        };
        m.capital_efficiency = (m.capital_utilization > 0.0)
            .then(|| m.total_return / m.capital_utilization)
            .and_then(finite);
        m.inventory_exposure = if seconds > 0.0 {
            exposure_time / seconds
        } else {
            0.0
        };
        m.directional_exposure = m.inventory_exposure;
        m.average_inventory = if seconds > 0.0 {
            inventory_time / seconds
        } else {
            0.0
        };
        m.inventory_duration_secs = inventory_duration;
        m.trend_exposure = if seconds > 0.0 {
            trend_time / seconds
        } else {
            0.0
        };
        m.maximum_grid_reset_count = risk.maximum_reset_count;
        m.grid_efficiency = number(grid_profit / capital);
        m.grid_turnover = number(orders.component_turnover(PositionComponent::Grid) / capital);
        m.grid_capture_ratio =
            (grid_gross > Decimal::ZERO).then(|| number(grid_profit / grid_gross));
        m.fee_gross_profit_ratio =
            (m.gross_pnl > Decimal::ZERO).then(|| number(m.fees / m.gross_pnl));
        m.reset_frequency = if seconds > 0.0 {
            risk.total_resets as f64 * 86_400.0 / seconds
        } else {
            0.0
        };
        m.worst_grid_cycle = grid_cycles.iter().map(|cycle| cycle.net_pnl).min();
    }
}

pub(super) fn number(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}
