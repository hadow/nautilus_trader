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

//! 权益曲线与网格周期指标；统计比例可用浮点数，现金核算始终使用 Decimal。

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

/// 一个只使用当时可得信息的因果权益估值点。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EquityPoint {
    /// 各独立标的中处于趋势状态的库存市值合计；None 表示使用本点的 regime 判断。
    #[serde(default)]
    pub trend_inventory: Option<Decimal>,
    /// 截至该时刻已知的累计真实/估算费用，用于因果成本诊断。
    #[serde(default)]
    pub cumulative_fees: Decimal,
    /// 截至该时刻已知的累计成交名义金额，用于增量滑点压力测试。
    #[serde(default)]
    pub cumulative_turnover: Decimal,
    /// 观测时间戳。
    pub ts_ns: u64,
    /// 库存估值使用的市场价格。
    pub price: Decimal,
    /// 策略现金与库存市值之和。
    pub equity: Decimal,
    /// 库存市值。
    pub exposure: Decimal,
    /// 实际成交持仓数量。
    pub position: Decimal,
    /// 库存与未终结买入预留之和占权益的比例。
    pub utilization: f64,
    /// 当前市场状态分类。
    pub regime: MarketRegime,
}

/// Runner 共用的输出容器，不依赖具体执行客户端。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PerformanceTracker {
    /// 被动诊断数据；旧检查点缺失的历史不会被虚构重建。
    #[serde(default)]
    pub diagnostics: GridDiagnostics,
    /// 已完成 K 线时及最终停止时记录的权益估值点。
    pub equity: Vec<EquityPoint>,
    /// 已完成库存周期。
    pub cycles: Vec<GridCycle>,
    /// 汇总指标；总收益与回撤均包含未平库存。
    pub metrics: GridMetrics,
    /// 已锁存的硬性风险原因（如有）。
    pub risk_off_reason: Option<String>,
    #[serde(default)]
    mark_peak: Decimal,
    #[serde(default)]
    mark_peak_ns: Option<u64>,
}

/// 报告统计指标；无定义的比例返回 None，不伪造无穷大。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GridMetrics {
    /// 已观测日度权益收益的年化标准差，按每年 252 个交易日计算。
    #[serde(default)]
    pub annualized_volatility: Option<f64>,
    /// 按 UTC 日期统计的精确权益盈亏，包含未平库存与费用。
    #[serde(default)]
    pub daily_pnl: BTreeMap<String, Decimal>,
    /// 按 UTC 月份统计的精确权益盈亏，包含尚未恢复的库存亏损。
    #[serde(default)]
    pub monthly_pnl: BTreeMap<String, Decimal>,
    /// 至少发生一次成交的订单意图比例；部分成交只计一次。
    #[serde(default)]
    pub grid_fill_rate: Option<f64>,
    /// 全部标的在策略生命周期内完成的网格重置总数。
    #[serde(default)]
    pub total_grid_resets: u64,
    /// 绝对成交名义金额，而非归一化换手率。
    #[serde(default)]
    pub turnover: Decimal,
    /// 最终按市值收益除以初始资金。
    pub total_return: f64,
    /// 几何年化收益率，与 CAGR 使用相同定义。
    pub annualized_return: Option<f64>,
    /// 复合年增长率；要求期末权益为正。
    pub cagr: Option<f64>,
    /// 无风险利率取零的日度 Sharpe，按每年 252 个交易日年化。
    pub sharpe: Option<f64>,
    /// 使用日度下行标准差计算的 Sortino。
    pub sortino: Option<f64>,
    /// 权益从峰值到谷值的最大下降比例。
    pub max_drawdown: f64,
    /// CAGR 除以最大回撤。
    pub calmar: Option<f64>,
    /// 权益低于此前高水位的最长持续时间，包含期末尚未修复的回撤。
    pub drawdown_duration_secs: f64,
    /// 盈利完成周期占全部完成周期的比例。
    pub win_rate: Option<f64>,
    /// 盈利战术网格周期比例，不包含核心仓再平衡。
    #[serde(default)]
    pub grid_cycle_win_rate: Option<f64>,
    /// 完成周期正净利润之和除以负净亏损绝对值之和。
    pub profit_factor: Option<f64>,
    /// 去重后的成交次数，包含部分成交。
    pub number_of_trades: u64,
    /// 已完成库存周期数量。
    pub number_of_grid_cycles: usize,
    /// 已完成核心仓减仓次数，不计入网格周期统计。
    #[serde(default)]
    pub number_of_core_rebalances: usize,
    /// 已完成网格周期平均净利润。
    pub average_grid_profit: Option<f64>,
    /// 从首次入场到最终出场的平均持续时间。
    pub average_holding_time_secs: Option<f64>,
    /// 已完成周期按决策价格计算、未扣费用与滑点的利润。
    pub gross_pnl: Decimal,
    /// 包含未卖库存和全部费用的总权益盈亏。
    pub net_pnl: Decimal,
    /// 已实现盈亏，包含部分减仓。
    pub realized_pnl: Decimal,
    /// 归属于长期核心仓的已实现盈亏。
    #[serde(default)]
    pub core_realized_pnl: Decimal,
    /// 归属于战术网格库存的已实现盈亏。
    #[serde(default)]
    pub grid_realized_pnl: Decimal,
    /// 归属于未卖库存的按市值盈亏，包含其剩余入场成本。
    #[serde(default)]
    pub unrealized_pnl: Decimal,
    /// 开盘价差乘上一时段最后观测库存的归因盈亏；不是与净盈亏相加的独立现金流。
    #[serde(default)]
    pub gap_pnl: Decimal,
    /// 常规交易时段开盘跳空中亏损部分的绝对值。
    #[serde(default)]
    pub gap_loss: Decimal,
    /// 归因于上升/下降趋势状态的亏损绝对值。
    #[serde(default)]
    pub trend_loss: Decimal,
    /// 全部入场与出场费用，包含未平库存的入场费用。
    pub fees: Decimal,
    /// 使用估算费用而非券商回报费用的成交数量。
    pub estimated_fee_fills: u64,
    /// 有符号执行损耗，已经计入盈亏。
    pub slippage: Decimal,
    /// 非负的不利执行损耗。
    #[serde(default)]
    pub slippage_cost: Decimal,
    /// 非负的有利成交价格改善。
    #[serde(default)]
    pub price_improvement: Decimal,
    /// 按时间加权的平均资金利用率。
    pub capital_utilization: f64,
    /// 总权益收益除以时间加权资金利用率；资金从未使用时无定义。
    pub capital_efficiency: Option<f64>,
    /// 最大库存市值暴露。
    pub maximum_exposure: Decimal,
    /// 最大实际成交持仓数量。
    pub maximum_position: Decimal,
    /// 按时间加权的平均实际持仓数量。
    #[serde(default)]
    pub average_inventory: f64,
    /// 实际库存非零的累计秒数。
    #[serde(default)]
    pub inventory_duration_secs: f64,
    /// 未出现盈利完成周期时的最大连续重置次数。
    pub maximum_grid_reset_count: u32,
    /// 已完成网格周期的已实现利润除以初始资金。
    pub grid_efficiency: f64,
    /// 成交名义金额除以初始资金。
    pub grid_turnover: f64,
    /// 已完成周期净利润除以按决策价格计算的毛利润。
    pub grid_capture_ratio: Option<f64>,
    /// 总费用除以正向的已完成周期决策价毛利润。
    pub fee_gross_profit_ratio: Option<f64>,
    /// 时间加权库存暴露除以初始资金。
    pub inventory_exposure: f64,
    /// 仅做多方向暴露，与库存暴露相同。
    pub directional_exposure: f64,
    /// 时间加权趋势状态库存暴露除以初始资金。
    pub trend_exposure: f64,
    /// 每个自然流逝日的网格重置次数。
    pub reset_frequency: f64,
    /// 回放期间实际创建的网格数量。
    #[serde(default)]
    pub grid_creation_count: usize,
    /// 网格下边界到上边界完整宽度相对中心价的平均比例。
    #[serde(default)]
    pub average_grid_width: Option<f64>,
    /// 相邻网格层级间距比例的平均值。
    #[serde(default)]
    pub average_grid_spacing: Option<f64>,
    /// 已完成对账、且退出网格没有任何入场成交的重置次数。
    #[serde(default)]
    pub false_reset_count: usize,
    /// 退出网格没有入场成交的重置占全部已对账重置的比例。
    #[serde(default)]
    pub false_reset_rate: Option<f64>,
    /// 已发送的旧网格撤单请求数量，用于衡量网格 churn。
    #[serde(default)]
    pub grid_churn: u64,
    /// 最差的已完成战术网格周期净盈亏。
    #[serde(default)]
    pub worst_grid_cycle: Option<Decimal>,
    /// 按区间起点的因果市场状态归因的权益变化。
    #[serde(default)]
    pub regime_pnl: BTreeMap<String, Decimal>,
}

impl PerformanceTracker {
    /// 使用组合自身权益曲线与各标的精确金额账本计算组合统计。
    pub fn finish_portfolio(
        &mut self,
        capital: Decimal,
        ledgers: &[(&OrderManager, &RiskManager)],
        instrument_reports: &[&Self],
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
        self.metrics.regime_pnl.clear();
        for report in instrument_reports {
            for (regime, pnl) in &report.metrics.regime_pnl {
                *self.metrics.regime_pnl.entry(regime.clone()).or_default() += pnl;
            }
        }
        self.metrics.gap_pnl = instrument_reports
            .iter()
            .map(|report| report.metrics.gap_pnl)
            .sum();
        self.metrics.gap_loss = instrument_reports
            .iter()
            .map(|report| report.metrics.gap_loss)
            .sum();
        self.metrics.trend_loss = instrument_reports
            .iter()
            .map(|report| report.metrics.trend_loss)
            .sum();
        self.metrics.grid_creation_count = instrument_reports
            .iter()
            .map(|report| report.metrics.grid_creation_count)
            .sum();
        let creations = self.metrics.grid_creation_count as f64;
        self.metrics.average_grid_width = (creations > 0.0).then(|| {
            instrument_reports
                .iter()
                .filter_map(|report| {
                    report
                        .metrics
                        .average_grid_width
                        .map(|width| width * report.metrics.grid_creation_count as f64)
                })
                .sum::<f64>()
                / creations
        });
        self.metrics.average_grid_spacing = (creations > 0.0).then(|| {
            instrument_reports
                .iter()
                .filter_map(|report| {
                    report
                        .metrics
                        .average_grid_spacing
                        .map(|spacing| spacing * report.metrics.grid_creation_count as f64)
                })
                .sum::<f64>()
                / creations
        });
        self.metrics.false_reset_count = instrument_reports
            .iter()
            .map(|report| report.metrics.false_reset_count)
            .sum();
        self.metrics.false_reset_rate = (self.metrics.total_grid_resets > 0)
            .then(|| self.metrics.false_reset_count as f64 / self.metrics.total_grid_resets as f64);
        self.metrics.grid_churn = instrument_reports
            .iter()
            .map(|report| report.metrics.grid_churn)
            .sum();
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

    /// 不保留全部报价，仅更新盘中风险极值。
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

    /// 根据订单账本与风险状态更新最终统计。
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
        m.trend_loss = ["TrendUp", "TrendDown"]
            .iter()
            .filter_map(|regime| m.regime_pnl.get(*regime))
            .map(|pnl| (-*pnl).max(Decimal::ZERO))
            .sum();
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
        m.slippage_cost = orders.adverse_slippage;
        m.price_improvement = orders.price_improvement;
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
        m.grid_creation_count = self.diagnostics.grids.len();
        m.average_grid_width = (!self.diagnostics.grids.is_empty()).then(|| {
            self.diagnostics
                .grids
                .iter()
                .map(|grid| number((grid.upper_bound - grid.lower_bound) / grid.center))
                .sum::<f64>()
                / self.diagnostics.grids.len() as f64
        });
        m.average_grid_spacing = (!self.diagnostics.grids.is_empty()).then(|| {
            self.diagnostics
                .grids
                .iter()
                .map(|grid| number(grid.spacing))
                .sum::<f64>()
                / self.diagnostics.grids.len() as f64
        });
        m.false_reset_count = self
            .diagnostics
            .resets
            .iter()
            .filter(|reset| reset.entry_orders_with_fills == 0)
            .count();
        m.false_reset_rate = (!self.diagnostics.resets.is_empty())
            .then(|| m.false_reset_count as f64 / self.diagnostics.resets.len() as f64);
        m.grid_churn = self.diagnostics.cancellations.len() as u64;
        m.worst_grid_cycle = grid_cycles.iter().map(|cycle| cycle.net_pnl).min();
    }
}

pub(super) fn number(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}
