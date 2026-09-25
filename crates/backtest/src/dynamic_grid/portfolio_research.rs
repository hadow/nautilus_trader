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

//! Chronological selection of complete multi-asset configurations using the native shared account.

use std::collections::{BTreeMap, BTreeSet};

use nautilus_model::{
    data::{Bar, QuoteTick},
    identifiers::InstrumentId,
};
use nautilus_trading::examples::strategies::dynamic_grid::{
    analytics::{GridMetrics, PerformanceTracker},
    config::SpacingMode,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    portfolio::{PortfolioBacktestConfig, PortfolioBenchmark, run_portfolio_backtest},
    research::{
        ExecutionStress, MonteCarloResult, WalkForwardConfig, daily_returns,
        execution_stress_monte_carlo, monte_carlo, walk_forward_ranges,
    },
};

/// Research-only overrides applied to existing complete configurations, never to live state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortfolioVariant {
    /// Stable report label for ablation and walk-forward audit output.
    pub name: String,
    /// Shared `GridConfig` field overrides; per-instrument values not named here are preserved.
    pub grid_overrides: Map<String, Value>,
    /// Instrument-specific `GridConfig` overrides applied after the shared overrides.
    pub instrument_overrides: BTreeMap<InstrumentId, Map<String, Value>>,
    /// Shared `PortfolioConfig` overrides. Model selection still requires identical initial capital.
    pub portfolio_overrides: Map<String, Value>,
}

impl PortfolioVariant {
    /// Resolves and validates one complete candidate without changing the source configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown fields, unknown instruments or invalid resulting limits.
    pub fn resolve(
        &self,
        base: &PortfolioBacktestConfig,
    ) -> anyhow::Result<PortfolioBacktestConfig> {
        anyhow::ensure!(
            !self.name.trim().is_empty(),
            "Candidate name cannot be empty"
        );
        anyhow::ensure!(
            self.instrument_overrides
                .keys()
                .all(|id| base.instruments.contains_key(id)),
            "Unknown candidate instrument"
        );
        let mut resolved = base.clone();
        for (id, instrument) in &mut resolved.instruments {
            let mut grid = serde_json::to_value(&instrument.strategy.grid)?;
            let fields = grid
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("Grid configuration must be an object"))?;
            fields.extend(self.grid_overrides.clone());
            if let Some(overrides) = self.instrument_overrides.get(id) {
                fields.extend(overrides.clone());
            }
            instrument.strategy.grid = serde_json::from_value(grid)?;
        }
        let mut portfolio = serde_json::to_value(&resolved.portfolio)?;
        portfolio
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Portfolio configuration must be an object"))?
            .extend(self.portfolio_overrides.clone());
        resolved.portfolio = serde_json::from_value(portfolio)?;
        resolved.strategy_config()?.validate()?;
        Ok(resolved)
    }
}

/// Research choices fixed before inspecting any test window. Each candidate retains per-asset grids.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortfolioResearchConfig {
    /// Observed trading days in each training window.
    pub train_days: usize,
    /// Observed validation days after training.
    pub validation_days: usize,
    /// Nonoverlapping untouched test days.
    pub test_days: usize,
    /// Number of training finalists admitted to validation.
    pub validation_finalists: usize,
    /// Objective: marked total return minus this multiple of MDD.
    pub drawdown_penalty: f64,
    /// First-training-window sensitivity rows: ATR spacing multipliers.
    pub sensitivity_atr_multipliers: Vec<Decimal>,
    /// First-training-window sensitivity columns: minimum anchor reset distance fractions.
    pub sensitivity_reset_distances: Vec<Decimal>,
    /// Return advantage over every immediate neighbor required for a parameter-cliff flag.
    pub parameter_cliff_threshold: f64,
    /// Reproducible simulations per method.
    pub simulations: usize,
    /// Ruin threshold as a fraction of starting equity.
    pub ruin_equity_fraction: f64,
    /// Seed for statistical resampling, independent of native execution seeds.
    pub random_seed: u64,
    /// Conditional daily-path shocks; execution fills remain fixed.
    pub stress: ExecutionStress,
}

impl Default for PortfolioResearchConfig {
    fn default() -> Self {
        Self {
            train_days: 30,
            validation_days: 10,
            test_days: 10,
            validation_finalists: 2,
            drawdown_penalty: 1.0,
            sensitivity_atr_multipliers: [6, 8, 10].into_iter().map(Decimal::from).collect(),
            sensitivity_reset_distances: [10, 15, 20]
                .into_iter()
                .map(|n| Decimal::new(n, 3))
                .collect(),
            parameter_cliff_threshold: 0.02,
            simulations: 1000,
            ruin_equity_fraction: 0.5,
            random_seed: 42,
            stress: ExecutionStress::default(),
        }
    }
}

/// One candidate chosen strictly before its test observations become available.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioResearchFold {
    /// First observed training timestamp.
    pub train_start_ns: u64,
    /// First validation timestamp, exclusive training boundary.
    pub validation_start_ns: u64,
    /// First untouched test timestamp.
    pub test_start_ns: u64,
    /// Last test bar timestamp.
    pub test_end_ns: u64,
    /// Candidate index selected by validation among training finalists.
    pub selected_candidate: usize,
    /// Training objectives in candidate order.
    pub training_scores: Vec<f64>,
    /// Validation objectives, best first with deterministic index tie-breaking.
    pub validation_scores: Vec<(usize, f64)>,
    /// Frozen complete per-instrument configuration used for the test.
    pub parameters: PortfolioBacktestConfig,
    /// Shared-account out-of-sample metrics, including marked inventory.
    pub test: GridMetrics,
    /// Same-window equal-weight buy-and-hold metrics.
    pub buy_and_hold: GridMetrics,
    /// Return-only perturbation of this frozen test path.
    pub return_perturbation: Option<MonteCarloResult>,
    /// Additional fees with the historical fills held fixed.
    pub fee_perturbation: Option<MonteCarloResult>,
    /// Additional slippage charged on actual test turnover.
    pub slippage_perturbation: Option<MonteCarloResult>,
}

/// Training-only sensitivity cell, not a full-sample optimization result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioSensitivityCell {
    /// ATR half-width multiplier applied to all independently observed ATR streams.
    pub atr_multiplier: Decimal,
    /// Minimum anchor movement fraction.
    pub reset_distance: Decimal,
    /// Native shared-account training metrics.
    pub metrics: GridMetrics,
    /// Isolated superior return versus every immediate tested neighbor.
    pub potential_overfitting: bool,
}

/// Out-of-sample evaluation with independent training-only sensitivity diagnostics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioResearchReport {
    /// Rolling chronological folds; test windows never overlap.
    pub folds: Vec<PortfolioResearchFold>,
    /// First-training-window ATR/reset matrix.
    pub sensitivity: Vec<PortfolioSensitivityCell>,
    /// Closed-cycle cash profit permutation, excluding open inventory.
    pub trade_shuffle: Option<MonteCarloResult>,
    /// Daily marked-return permutation; terminal return is invariant to ordering.
    pub return_shuffle: Option<MonteCarloResult>,
    /// Daily marked returns sampled with replacement.
    pub return_bootstrap: Option<MonteCarloResult>,
    /// Explicit experimental limitations.
    pub notes: Vec<String>,
}

/// Runs train → validation → frozen test using complete configurations and one native account.
///
/// Candidates may vary allocations, grids and risk, but not data, capital, costs or execution seeds.
/// Every partition starts flat and warms indicators from its own completed bars.
///
/// # Errors
///
/// Returns an error for invalid research choices, unmatched candidates, insufficient history or a
/// failed native replay. Rejections and missing instrument streams are never silently discarded.
pub fn portfolio_walk_forward(
    bars: &[Bar],
    quotes: &[QuoteTick],
    candidates: &[PortfolioBacktestConfig],
    config: &PortfolioResearchConfig,
) -> anyhow::Result<PortfolioResearchReport> {
    anyhow::ensure!(
        !candidates.is_empty()
            && candidates.len() <= 64
            && config.validation_finalists > 0
            && config.drawdown_penalty.is_finite()
            && config.drawdown_penalty >= 0.0
            && config.parameter_cliff_threshold.is_finite()
            && config.parameter_cliff_threshold > 0.0,
        "Invalid portfolio research choices"
    );
    // Validate simulation inputs before spending time on any replay.
    monte_carlo(
        &[],
        config.simulations,
        config.random_seed,
        config.ruin_equity_fraction,
        true,
        false,
    )?;
    let base = &candidates[0];
    execution_stress_monte_carlo(
        &PerformanceTracker::default(),
        base.portfolio.capital,
        config.simulations,
        config.random_seed,
        config.ruin_equity_fraction,
        &config.stress,
    )?;
    anyhow::ensure!(
        config
            .sensitivity_atr_multipliers
            .len()
            .saturating_mul(config.sensitivity_reset_distances.len())
            <= 100
            && config
                .sensitivity_atr_multipliers
                .iter()
                .all(|v| *v > Decimal::ZERO)
            && config
                .sensitivity_reset_distances
                .iter()
                .all(|v| *v >= Decimal::ZERO && *v <= Decimal::ONE),
        "Invalid sensitivity matrix (maximum 100 cells)"
    );
    for candidate in candidates {
        candidate.strategy_config()?.validate()?;
        anyhow::ensure!(
            candidate.portfolio.capital == base.portfolio.capital
                && candidate.currency == base.currency
                && candidate.random_seed == base.random_seed
                && candidate.slippage_probability.abs().to_bits()
                    == base.slippage_probability.abs().to_bits()
                && candidate.start_ns == base.start_ns
                && candidate.end_ns == base.end_ns
                && candidate.instruments.keys().eq(base.instruments.keys()),
            "Portfolio candidates must share capital, universe and execution assumptions"
        );
        for (id, asset) in &candidate.instruments {
            let original = &base.instruments[id];
            let grid = &asset.strategy.grid;
            let original_grid = &original.strategy.grid;
            anyhow::ensure!(
                asset.price_increment == original.price_increment
                    && asset.lot_size == original.lot_size
                    && asset.bars_path == original.bars_path
                    && asset.quotes_path == original.quotes_path
                    && asset.strategy.bar_type == original.strategy.bar_type
                    && asset.strategy.enabled == original.strategy.enabled
                    && asset.strategy.sector == original.strategy.sector
                    && asset.strategy.tick_execution == original.strategy.tick_execution
                    && asset.strategy.confirmed_custom_bars
                        == original.strategy.confirmed_custom_bars
                    && (
                        grid.maker_fee,
                        grid.taker_fee,
                        grid.commission,
                        grid.slippage
                    ) == (
                        original_grid.maker_fee,
                        original_grid.taker_fee,
                        original_grid.commission,
                        original_grid.slippage
                    ),
                "Portfolio candidates must share data and costs for {id}"
            );
        }
    }
    let windows = WalkForwardConfig {
        train_days: config.train_days,
        validation_days: config.validation_days,
        test_days: config.test_days,
        ..Default::default()
    };
    let ranges = walk_forward_ranges(bars, &windows)?;
    let replay = |a: usize, b: usize, candidate: &PortfolioBacktestConfig, benchmark| {
        let start = bars[a].ts_event.as_u64();
        let end = bars.get(b).map_or_else(
            || (bars[b - 1].ts_event.as_u64() / 86_400_000_000_000 + 1) * 86_400_000_000_000,
            |bar| bar.ts_event.as_u64(),
        );
        let slice: Vec<_> = quotes
            .iter()
            .filter(|q| q.ts_event.as_u64() >= start && q.ts_event.as_u64() < end)
            .copied()
            .collect();
        run_portfolio_backtest(&bars[a..b], &slice, candidate, benchmark)
    };
    let objective = |m: &GridMetrics| m.total_return - config.drawdown_penalty * m.max_drawdown;
    let mut folds = Vec::new();
    let mut returns = Vec::new();
    let mut trades = Vec::new();
    for [a, b, c, d] in &ranges {
        let mut training_scores = Vec::new();
        for candidate in candidates {
            training_scores.push(objective(
                &replay(*a, *b, candidate, PortfolioBenchmark::Dynamic)?
                    .portfolio
                    .metrics,
            ));
        }
        let mut ranking: Vec<_> = (0..candidates.len()).collect();
        ranking.sort_by(|a, b| {
            training_scores[*b]
                .total_cmp(&training_scores[*a])
                .then(a.cmp(b))
        });
        let mut validation_scores = Vec::new();
        for index in ranking.into_iter().take(config.validation_finalists) {
            validation_scores.push((
                index,
                objective(
                    &replay(*b, *c, &candidates[index], PortfolioBenchmark::Dynamic)?
                        .portfolio
                        .metrics,
                ),
            ));
        }
        validation_scores.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let selected = validation_scores[0].0;
        let report = replay(*c, *d, &candidates[selected], PortfolioBenchmark::Dynamic)?;
        let stress = |assumptions: ExecutionStress| {
            execution_stress_monte_carlo(
                &report.portfolio,
                base.portfolio.capital,
                config.simulations,
                config.random_seed,
                config.ruin_equity_fraction,
                &assumptions,
            )
        };
        let return_perturbation = stress(ExecutionStress {
            return_noise: config.stress.return_noise,
            fee_multiplier_max: 1.0,
            additional_slippage_max: 0.0,
        })?;
        let fee_perturbation = stress(ExecutionStress {
            return_noise: 0.0,
            fee_multiplier_max: config.stress.fee_multiplier_max,
            additional_slippage_max: 0.0,
        })?;
        let slippage_perturbation = stress(ExecutionStress {
            return_noise: 0.0,
            fee_multiplier_max: 1.0,
            additional_slippage_max: config.stress.additional_slippage_max,
        })?;
        returns.extend(daily_returns(&report.portfolio, base.portfolio.capital));
        trades.extend(
            report
                .portfolio
                .cycles
                .iter()
                .filter_map(|cycle| (cycle.net_pnl / base.portfolio.capital).to_f64()),
        );
        let benchmark = replay(
            *c,
            *d,
            &candidates[selected],
            PortfolioBenchmark::EqualWeightBuyHold,
        )?;
        folds.push(PortfolioResearchFold {
            train_start_ns: bars[*a].ts_event.as_u64(),
            validation_start_ns: bars[*b].ts_event.as_u64(),
            test_start_ns: bars[*c].ts_event.as_u64(),
            test_end_ns: bars[d - 1].ts_event.as_u64(),
            selected_candidate: selected,
            training_scores,
            validation_scores,
            parameters: candidates[selected].clone(),
            return_perturbation,
            fee_perturbation,
            slippage_perturbation,
            test: report.portfolio.metrics,
            buy_and_hold: benchmark.portfolio.metrics,
        });
    }
    let widths: Vec<_> = config
        .sensitivity_atr_multipliers
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let distances: Vec<_> = config
        .sensitivity_reset_distances
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let [a, b, _, _] = ranges[0];
    let mut sensitivity = Vec::new();
    for width in &widths {
        for distance in &distances {
            let mut candidate = base.clone();
            for asset in candidate.instruments.values_mut() {
                asset.strategy.grid.spacing_mode = SpacingMode::Atr;
                asset.strategy.grid.atr_multiplier = *width;
                asset.strategy.grid.minimum_reset_distance = *distance;
            }
            let report = replay(a, b, &candidate, PortfolioBenchmark::Dynamic)?;
            sensitivity.push(PortfolioSensitivityCell {
                atr_multiplier: *width,
                reset_distance: *distance,
                metrics: report.portfolio.metrics,
                potential_overfitting: false,
            });
        }
    }
    let values: Vec<_> = sensitivity.iter().map(|c| c.metrics.total_return).collect();
    for (i, cell) in sensitivity.iter_mut().enumerate() {
        let row = i / distances.len();
        let col = i % distances.len();
        let mut neighbors = Vec::new();
        if row > 0 {
            neighbors.push(i - distances.len());
        }
        if row + 1 < widths.len() {
            neighbors.push(i + distances.len());
        }
        if col > 0 {
            neighbors.push(i - 1);
        }
        if col + 1 < distances.len() {
            neighbors.push(i + 1);
        }
        cell.potential_overfitting = neighbors.len() >= 2
            && neighbors
                .iter()
                .all(|j| values[i] - values[*j] > config.parameter_cliff_threshold);
    }
    Ok(PortfolioResearchReport {
        folds, sensitivity,
        trade_shuffle: monte_carlo(&trades, config.simulations, config.random_seed, config.ruin_equity_fraction, false, false)?,
        return_shuffle: monte_carlo(&returns, config.simulations, config.random_seed, config.ruin_equity_fraction, true, false)?,
        return_bootstrap: monte_carlo(&returns, config.simulations, config.random_seed, config.ruin_equity_fraction, true, true)?,
        notes: vec![
            "Each partition starts flat with independent causal warm-up; no inventory is carried across folds".into(),
            "Sensitivity uses the first training window only; clamped ATR widths may produce identical cells without demonstrating robustness".into(),
            "Trade shuffle excludes open inventory; marked daily returns include inventory but resampling loses serial dependence".into(),
            "Permutation does not change terminal compound return; bootstrap is statistical stress, not a price forecast".into(),
            "Short histories and small trade counts cannot establish out-of-sample alpha".into(),
            "Per-fold fee, slippage and return perturbations hold historical fills fixed; they do not simulate order-policy feedback".into(),
        ],
    })
}
