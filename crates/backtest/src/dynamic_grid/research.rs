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

//! Chronological model selection, training-only sensitivity and seeded Monte Carlo diagnostics.

use std::collections::BTreeSet;

use nautilus_model::data::Bar;
use nautilus_trading::examples::strategies::dynamic_grid::{
    analytics::{GridMetrics, PerformanceTracker},
    config::{GridConfig, SpacingMode},
};
use rand::{RngExt, SeedableRng, rngs::StdRng, seq::SliceRandom};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use super::{Benchmark, GridBacktestConfig, run_grid_backtest};

/// All model-selection choices are fixed before inspecting test returns.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalkForwardConfig {
    /// Rolling training length in observed trading days.
    pub train_days: usize,
    /// Validation length in observed trading days.
    pub validation_days: usize,
    /// Nonoverlapping out-of-sample length in observed trading days.
    pub test_days: usize,
    /// Parameter candidates; every `GridConfig` parameter can be varied.
    pub candidates: Vec<GridConfig>,
    /// Number of training finalists evaluated on validation.
    pub validation_finalists: usize,
    /// Selection objective is total return minus this multiple of MDD.
    pub drawdown_penalty: f64,
    /// Training-only grid spacings for the sensitivity matrix.
    pub sensitivity_spacings: Vec<Decimal>,
    /// Training-only levels per side for the sensitivity matrix.
    pub sensitivity_levels: Vec<usize>,
    /// Isolated return improvement over every immediate neighbor triggers a cliff warning.
    pub parameter_cliff_threshold: f64,
    /// Seeded path simulations per Monte Carlo method.
    pub simulations: usize,
    /// Ruin means crossing this fraction of starting equity.
    pub ruin_equity_fraction: f64,
    /// Reproducible random stream seed.
    pub random_seed: u64,
}

impl Default for WalkForwardConfig {
    fn default() -> Self {
        Self {
            train_days: 252,
            validation_days: 63,
            test_days: 63,
            candidates: vec![GridConfig::default()],
            validation_finalists: 3,
            drawdown_penalty: 1.0,
            sensitivity_spacings: [5, 10, 15, 20, 25, 30]
                .into_iter()
                .map(|n| Decimal::new(n, 3))
                .collect(),
            sensitivity_levels: vec![5, 10, 15, 20, 30],
            parameter_cliff_threshold: 0.05,
            simulations: 1000,
            ruin_equity_fraction: 0.5,
            random_seed: 42,
        }
    }
}

/// Result for one strictly ordered train/validation/test fold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalkForwardFold {
    /// First training timestamp.
    pub train_start_ns: u64,
    /// First validation timestamp.
    pub validation_start_ns: u64,
    /// First untouched test timestamp.
    pub test_start_ns: u64,
    /// Last test timestamp.
    pub test_end_ns: u64,
    /// Selected candidate index, chosen without test information.
    pub selected_candidate: usize,
    /// Objective values for training candidates in input order.
    pub training_scores: Vec<f64>,
    /// Validation objectives for training finalists.
    pub validation_scores: Vec<(usize, f64)>,
    /// Frozen selected configuration.
    pub parameters: GridConfig,
    /// Untouched test metrics.
    pub test: GridMetrics,
    /// Test metrics for buy-and-hold.
    pub buy_and_hold: GridMetrics,
    /// Test metrics for fixed-grid.
    pub fixed_grid: GridMetrics,
    /// Test halt reason, including a legitimate risk-off event.
    pub risk_off_reason: Option<String>,
}

/// A sensitivity cell evaluated exclusively on the first training window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SensitivityCell {
    /// Fixed spacing fraction.
    pub spacing: Decimal,
    /// Levels per side.
    pub levels: usize,
    /// Return, drawdown and Sharpe alongside the other diagnostics.
    pub metrics: GridMetrics,
    /// All immediate grid neighbors underperform by the configured return threshold.
    pub potential_overfitting: bool,
}

/// Simulation quantiles and empirical path probabilities.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonteCarloResult {
    /// Number of seeded simulations.
    pub simulations: usize,
    /// 5th, 50th and 95th percentile maximum drawdown.
    pub mdd_quantiles: [f64; 3],
    /// 5th, 50th and 95th percentile terminal return.
    pub return_quantiles: [f64; 3],
    /// Fraction with negative terminal return.
    pub probability_of_loss: f64,
    /// Fraction touching the specified ruin threshold at any step.
    pub probability_of_ruin: f64,
}

/// Conditional daily-path stress, not a resimulation of orders or a forecast of future prices.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionStress {
    /// Uniform signed daily return shock, scaled by observed exposure / prior equity.
    pub return_noise: f64,
    /// Uniform multiplier from one to this maximum; only additional fees are subtracted.
    pub fee_multiplier_max: f64,
    /// Uniform additional one-way slippage fraction of actual traded notional.
    pub additional_slippage_max: f64,
}

impl Default for ExecutionStress {
    fn default() -> Self {
        Self {
            return_noise: 0.002,
            fee_multiplier_max: 2.0,
            additional_slippage_max: 0.001,
        }
    }
}

/// Perturbs the marked daily path and actual costs while retaining the historical execution path.
///
/// Cash-only days receive no price shock. Slippage stress is additional cost per traded notional,
/// so favorable historical fills cannot incorrectly become more profitable when costs increase.
///
/// # Errors
///
/// Returns an error for invalid assumptions, missing cost history or nonfinite simulation paths.
pub fn execution_stress_monte_carlo(
    report: &PerformanceTracker,
    capital: Decimal,
    simulations: usize,
    seed: u64,
    ruin: f64,
    stress: &ExecutionStress,
) -> anyhow::Result<Option<MonteCarloResult>> {
    monte_carlo(&[], simulations, seed, ruin, true, false)?;
    let simulation_count = f64::from(u32::try_from(simulations)?);
    anyhow::ensure!(
        capital > Decimal::ZERO
            && stress.return_noise.is_finite()
            && (0.0..1.0).contains(&stress.return_noise)
            && stress.fee_multiplier_max.is_finite()
            && (1.0..=10.0).contains(&stress.fee_multiplier_max)
            && stress.additional_slippage_max.is_finite()
            && (0.0..=0.1).contains(&stress.additional_slippage_max),
        "Invalid execution stress assumptions"
    );
    let Some(last) = report.equity.last() else {
        return Ok(None);
    };
    anyhow::ensure!(
        last.cumulative_fees == report.metrics.fees
            && last.cumulative_turnover == report.metrics.turnover,
        "Execution stress requires complete fee and turnover observations"
    );
    let mut daily = std::collections::BTreeMap::new();
    let mut previous_fees = Decimal::ZERO;
    let mut previous_turnover = Decimal::ZERO;
    let mut previous_time = 0;
    for point in &report.equity {
        anyhow::ensure!(
            point.ts_ns >= previous_time
                && point.cumulative_fees >= previous_fees
                && point.cumulative_turnover >= previous_turnover
                && point.equity >= Decimal::ZERO,
            "Invalid historical execution cost path"
        );
        let entry = daily.entry(point.ts_ns / 86_400_000_000_000).or_insert((
            point.equity,
            point.cumulative_fees,
            point.cumulative_turnover,
            Decimal::ZERO,
        ));
        *entry = (
            point.equity,
            point.cumulative_fees,
            point.cumulative_turnover,
            entry.3.max(point.exposure),
        );
        previous_time = point.ts_ns;
        previous_fees = point.cumulative_fees;
        previous_turnover = point.cumulative_turnover;
    }
    let mut previous = capital;
    previous_fees = Decimal::ZERO;
    previous_turnover = Decimal::ZERO;
    let mut observations = Vec::new();
    for (equity, fees, turnover, exposure) in daily.values() {
        if previous > Decimal::ZERO {
            observations.push((
                (*equity / previous - Decimal::ONE)
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("Unrepresentable return"))?,
                ((*fees - previous_fees) / previous)
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("Unrepresentable fees"))?,
                ((*turnover - previous_turnover) / previous)
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("Unrepresentable turnover"))?,
                (*exposure / previous)
                    .min(Decimal::ONE)
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("Unrepresentable exposure"))?,
            ));
        }
        previous = *equity;
        previous_fees = *fees;
        previous_turnover = *turnover;
    }
    // ponytail: daily shocks hold fills fixed; use native perturbed-cost replays for order-policy feedback.
    let mut rng = StdRng::seed_from_u64(seed);
    let mut endings = Vec::with_capacity(simulations);
    let mut drawdowns = Vec::with_capacity(simulations);
    let mut losses = 0;
    let mut ruins = 0;
    for _ in 0..simulations {
        let mut equity: f64 = 1.0;
        let mut peak: f64 = 1.0;
        let mut mdd: f64 = 0.0;
        let mut ruined = false;
        for (value, fees, turnover, exposure) in &observations {
            let noise = rng.random_range(-1.0..=1.0) * stress.return_noise * exposure;
            let extra_fees = rng.random_range(0.0..=1.0) * (stress.fee_multiplier_max - 1.0) * fees;
            let extra_slippage =
                rng.random_range(0.0..=1.0) * stress.additional_slippage_max * turnover;
            equity *= (1.0 + value + noise - extra_fees - extra_slippage).max(0.0);
            peak = peak.max(equity);
            mdd = mdd.max((peak - equity) / peak);
            ruined |= equity <= ruin;
        }
        anyhow::ensure!(
            equity.is_finite() && mdd.is_finite(),
            "Execution stress path overflow"
        );
        endings.push(equity - 1.0);
        drawdowns.push(mdd);
        losses += usize::from(equity < 1.0);
        ruins += usize::from(ruined);
    }
    endings.sort_by(f64::total_cmp);
    drawdowns.sort_by(f64::total_cmp);
    let quantiles = |v: &[f64]| {
        [
            v[(simulations - 1) * 5 / 100],
            v[(simulations - 1) / 2],
            v[(simulations - 1) * 95 / 100],
        ]
    };
    Ok(Some(MonteCarloResult {
        simulations,
        return_quantiles: quantiles(&endings),
        mdd_quantiles: quantiles(&drawdowns),
        probability_of_loss: f64::from(u32::try_from(losses)?) / simulation_count,
        probability_of_ruin: f64::from(u32::try_from(ruins)?) / simulation_count,
    }))
}

/// Walk-forward output never reports a full-sample optimized winner.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalkForwardReport {
    /// Chronological nonoverlapping OOS folds.
    pub folds: Vec<WalkForwardFold>,
    /// Training-only sensitivity matrix.
    pub sensitivity: Vec<SensitivityCell>,
    /// Completed-cycle cash-profit permutation; excludes open inventory risk.
    pub trade_shuffle: Option<MonteCarloResult>,
    /// OOS daily marked-return permutation; terminal return is invariant to order.
    pub return_shuffle: Option<MonteCarloResult>,
    /// OOS daily returns sampled with replacement; assesses terminal-return uncertainty.
    pub return_bootstrap: Option<MonteCarloResult>,
    /// Method limitations and data sufficiency flags.
    pub notes: Vec<String>,
}

/// Builds nonoverlapping trading-day splits; no data from a later partition can enter selection.
///
/// # Errors
///
/// Returns an error for invalid lengths or insufficient observations for one complete fold.
pub fn walk_forward_ranges(
    bars: &[Bar],
    config: &WalkForwardConfig,
) -> anyhow::Result<Vec<[usize; 4]>> {
    anyhow::ensure!(
        config.train_days > 0 && config.validation_days > 0 && config.test_days > 0,
        "Walk-forward windows must be positive"
    );
    anyhow::ensure!(
        bars.windows(2)
            .all(|w| (w[0].ts_event, w[0].bar_type.instrument_id())
                < (w[1].ts_event, w[1].bar_type.instrument_id())),
        "Walk-forward input must be time ordered"
    );
    let mut starts = Vec::new();
    let mut previous = None;
    for (i, bar) in bars.iter().enumerate() {
        let day = bar.ts_event.as_u64() / 86_400_000_000_000;
        if previous != Some(day) {
            starts.push(i);
            previous = Some(day);
        }
    }
    let days = starts.len();
    starts.push(bars.len());
    let total = config
        .train_days
        .checked_add(config.validation_days)
        .and_then(|x| x.checked_add(config.test_days))
        .ok_or_else(|| anyhow::anyhow!("Walk-forward window overflow"))?;
    anyhow::ensure!(days >= total, "Need {total} trading days, available {days}");
    let mut ranges = Vec::new();
    for start in (0..=days - total).step_by(config.test_days) {
        ranges.push([
            starts[start],
            starts[start + config.train_days],
            starts[start + config.train_days + config.validation_days],
            starts[start + total],
        ]);
    }
    Ok(ranges)
}

/// Runs training selection, validation selection, frozen OOS tests and robustness diagnostics.
///
/// # Errors
///
/// Returns an error for invalid configurations or any failed native backtest.
pub fn walk_forward(
    bars: &[Bar],
    base: &GridBacktestConfig,
    config: &WalkForwardConfig,
) -> anyhow::Result<WalkForwardReport> {
    anyhow::ensure!(
        !config.candidates.is_empty() && config.validation_finalists > 0,
        "At least one candidate and finalist required"
    );
    anyhow::ensure!(
        config.drawdown_penalty.is_finite()
            && config.drawdown_penalty >= 0.0
            && config.parameter_cliff_threshold.is_finite()
            && config.parameter_cliff_threshold > 0.0,
        "Invalid selection thresholds"
    );
    anyhow::ensure!(
        config.simulations > 0
            && config.simulations <= 100_000
            && config.ruin_equity_fraction > 0.0
            && config.ruin_equity_fraction < 1.0,
        "Invalid Monte Carlo configuration"
    );
    for candidate in &config.candidates {
        candidate.validate()?;
        anyhow::ensure!(
            candidate.capital == base.grid.capital,
            "Candidates must use identical initial capital"
        );
    }
    let ranges = walk_forward_ranges(bars, config)?;
    let mut folds = Vec::new();
    let mut returns = Vec::new();
    let mut trades = Vec::new();
    for [a, b, c, d] in &ranges {
        let mut scores = Vec::new();
        for candidate in &config.candidates {
            let mut trial = base.clone();
            trial.grid = candidate.clone();
            let report = run_grid_backtest(&bars[*a..*b], &[], &trial, Benchmark::Dynamic)?;
            scores.push(score(&report.metrics, config));
        }
        let mut ranking: Vec<_> = (0..scores.len()).collect();
        ranking.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
        let mut validation = Vec::new();
        for index in ranking.into_iter().take(config.validation_finalists) {
            let mut trial = base.clone();
            trial.grid = config.candidates[index].clone();
            let report = run_grid_backtest(&bars[*b..*c], &[], &trial, Benchmark::Dynamic)?;
            validation.push((index, score(&report.metrics, config)));
        }
        validation.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let selected = validation[0].0;
        let mut trial = base.clone();
        trial.grid = config.candidates[selected].clone();
        let report = run_grid_backtest(&bars[*c..*d], &[], &trial, Benchmark::Dynamic)?;
        returns.extend(daily_returns(&report, base.grid.capital));
        trades.extend(report.cycles.iter().map(|cycle| {
            (cycle.net_pnl / base.grid.capital)
                .to_f64()
                .unwrap_or_default()
        }));
        folds.push(WalkForwardFold {
            train_start_ns: bars[*a].ts_event.as_u64(),
            validation_start_ns: bars[*b].ts_event.as_u64(),
            test_start_ns: bars[*c].ts_event.as_u64(),
            test_end_ns: bars[d - 1].ts_event.as_u64(),
            selected_candidate: selected,
            training_scores: scores,
            validation_scores: validation,
            parameters: trial.grid.clone(),
            test: report.metrics,
            buy_and_hold: run_grid_backtest(&bars[*c..*d], &[], &trial, Benchmark::BuyHold)?
                .metrics,
            fixed_grid: run_grid_backtest(&bars[*c..*d], &[], &trial, Benchmark::Fixed)?.metrics,
            risk_off_reason: report.risk_off_reason,
        });
    }
    let [a, b, _, _] = ranges[0];
    let mut sensitivity = Vec::new();
    let spacings: Vec<_> = config
        .sensitivity_spacings
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let levels: Vec<_> = config
        .sensitivity_levels
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for spacing in &spacings {
        for level in &levels {
            let mut trial = base.clone();
            trial.grid.spacing_mode = SpacingMode::Percentage;
            trial.grid.spacing_pct = *spacing;
            trial.grid.grid_levels = *level;
            let report = run_grid_backtest(&bars[a..b], &[], &trial, Benchmark::Dynamic)?;
            sensitivity.push(SensitivityCell {
                spacing: *spacing,
                levels: *level,
                metrics: report.metrics,
                potential_overfitting: false,
            });
        }
    }
    let values: Vec<_> = sensitivity
        .iter()
        .map(|cell| cell.metrics.total_return)
        .collect();
    for (i, cell) in sensitivity.iter_mut().enumerate() {
        let row = i / levels.len();
        let col = i % levels.len();
        let mut neighbors = Vec::new();
        if row > 0 {
            neighbors.push(i - levels.len());
        }
        if row + 1 < spacings.len() {
            neighbors.push(i + levels.len());
        }
        if col > 0 {
            neighbors.push(i - 1);
        }
        if col + 1 < levels.len() {
            neighbors.push(i + 1);
        }
        cell.potential_overfitting = neighbors.len() >= 2
            && neighbors
                .iter()
                .all(|j| values[i] - values[*j] > config.parameter_cliff_threshold);
    }
    Ok(WalkForwardReport { folds, sensitivity,
        trade_shuffle: monte_carlo(&trades, config.simulations, config.random_seed, config.ruin_equity_fraction, false, false)?,
        return_shuffle: monte_carlo(&returns, config.simulations, config.random_seed, config.ruin_equity_fraction, true, false)?,
        return_bootstrap: monte_carlo(&returns, config.simulations, config.random_seed, config.ruin_equity_fraction, true, true)?,
        notes: vec!["Every fold starts flat with independent warm-up and initial capital; test windows do not overlap".to_string(), "Sensitivity uses the first training partition only; it does not select a full-sample winner".to_string(), "Trade shuffle excludes unsold inventory and serial dependence; daily marked returns include inventory".to_string(), "Permutation changes path drawdown, not the mathematical terminal return; bootstrap adds resampling uncertainty but is not a market forecast".to_string(), "Bar execution uses Nautilus OHLC path assumptions; confirm with actual tick data before deployment".to_string()] })
}

/// Permutes or bootstraps a finite sequence with a reproducible RNG.
///
/// # Errors
///
/// Returns an error for invalid probabilities, counts or nonfinite returns.
pub fn monte_carlo(
    values: &[f64],
    simulations: usize,
    seed: u64,
    ruin: f64,
    compound: bool,
    bootstrap: bool,
) -> anyhow::Result<Option<MonteCarloResult>> {
    anyhow::ensure!(
        simulations > 0
            && simulations <= 100_000
            && ruin > 0.0
            && ruin < 1.0
            && values
                .iter()
                .all(|v| v.is_finite() && (!compound || *v >= -1.0)),
        "Invalid Monte Carlo input"
    );
    if values.is_empty() {
        return Ok(None);
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let mut drawdowns = Vec::new();
    let mut endings = Vec::new();
    let mut losses = 0;
    let mut ruins = 0;
    for _ in 0..simulations {
        let mut sequence = values.to_vec();
        if bootstrap {
            for value in &mut sequence {
                *value = values[rng.random_range(0..values.len())];
            }
        } else {
            sequence.shuffle(&mut rng);
        }
        let mut equity: f64 = 1.0;
        let mut peak: f64 = 1.0;
        let mut mdd: f64 = 0.0;
        let mut ruined = false;
        for value in sequence {
            equity = if compound {
                equity * (1.0 + value)
            } else {
                equity + value
            };
            peak = peak.max(equity);
            mdd = mdd.max((peak - equity) / peak);
            ruined |= equity <= ruin;
        }
        anyhow::ensure!(equity.is_finite(), "Monte Carlo path overflow");
        drawdowns.push(mdd);
        endings.push(equity - 1.0);
        losses += usize::from(equity < 1.0);
        ruins += usize::from(ruined);
    }
    drawdowns.sort_by(f64::total_cmp);
    endings.sort_by(f64::total_cmp);
    let quantiles = |v: &[f64]| {
        [
            v[(simulations - 1) * 5 / 100],
            v[(simulations - 1) / 2],
            v[(simulations - 1) * 95 / 100],
        ]
    };
    Ok(Some(MonteCarloResult {
        simulations,
        mdd_quantiles: quantiles(&drawdowns),
        return_quantiles: quantiles(&endings),
        probability_of_loss: f64::from(u32::try_from(losses)?)
            / f64::from(u32::try_from(simulations)?),
        probability_of_ruin: f64::from(u32::try_from(ruins)?)
            / f64::from(u32::try_from(simulations)?),
    }))
}

fn score(metrics: &GridMetrics, config: &WalkForwardConfig) -> f64 {
    metrics.total_return - config.drawdown_penalty * metrics.max_drawdown
}

pub(super) fn daily_returns(report: &PerformanceTracker, capital: Decimal) -> Vec<f64> {
    let mut daily = std::collections::BTreeMap::new();
    for point in &report.equity {
        daily.insert(point.ts_ns / 86_400_000_000_000, point.equity);
    }
    let mut previous = capital;
    let mut returns = Vec::new();
    for equity in daily.values() {
        if previous > Decimal::ZERO {
            returns.push(
                (*equity / previous - Decimal::ONE)
                    .to_f64()
                    .unwrap_or_default(),
            );
        }
        previous = *equity;
    }
    returns
}
