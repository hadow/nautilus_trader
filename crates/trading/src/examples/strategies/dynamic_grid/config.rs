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

//! Validated configuration shared by every execution mode.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Position semantics used by the shared strategy and execution path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyMode {
    /// Original paper-style grid inventory, retained only as a research baseline.
    LegacyDgt,
    /// Stock-adapted target position with separate core and grid sleeves.
    StockAdaptive,
}

/// Grid spacing calculation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpacingMode {
    /// Fixed percentage between adjacent levels.
    Percentage,
    /// ATR divided by current price, clamped to configured bounds.
    Atr,
}

/// Native moving average used for slope and price confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegimeAverage {
    /// Retains the original rolling simple-average rule.
    Simple,
    /// Exponentially weighted average of completed closes.
    Exponential,
}

/// Capital weights for successive levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionSizing {
    /// Equal capital per level.
    Equal,
    /// Larger allocations further from the center.
    Progressive,
    /// Larger allocations near the center.
    Inverse,
    /// Equal level budgets, reduced by target ATR/price divided by observed ATR/price.
    VolatilityAdjusted,
}

/// Behavior while a directional trend is detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendPolicy {
    /// No new inventory; existing exits remain available.
    Disable,
    /// Reduce the number of active entry/exit levels in the trend direction.
    ReduceGrid,
    /// Expand spacing at the next permitted reset.
    WiderGrid,
    /// Accumulate long inventory without ordinary profit-taking in up trends.
    LongOnly,
    /// Continue the normal grid.
    Continue,
}

/// Action after a hard risk limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskPolicy {
    /// Cancel entries and retain inventory with profit-taking exits.
    Hold,
    /// Cancel everything, wait for confirmation, then liquidate filled inventory.
    Flatten,
}

/// Strategy parameters. Ratios use fractions (0.01 means one percent).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridConfig {
    /// Research baseline or stock-adapted production behavior.
    pub strategy_mode: StrategyMode,
    /// Number of levels on each side of center.
    pub grid_levels: usize,
    /// Percentage or ATR spacing.
    pub spacing_mode: SpacingMode,
    /// Fixed percentage spacing.
    pub spacing_pct: Decimal,
    /// Multiplier applied to ATR/price.
    pub atr_multiplier: Decimal,
    /// Minimum adjacent-spacing fraction.
    pub min_spacing_pct: Decimal,
    /// Maximum adjacent-spacing fraction.
    pub max_spacing_pct: Decimal,
    /// Initial total strategy capital, never an implicit deposit on reset.
    pub capital: Decimal,
    /// Fraction allocated to a grid before account and exposure limits.
    pub capital_allocation: Decimal,
    /// Fraction of grid capital used to seed upper sell levels.
    pub initial_inventory_fraction: Decimal,
    /// Long-lived fraction of the instrument allocation.
    pub core_target_pct: Decimal,
    /// Maximum tactical grid fraction of the instrument allocation.
    pub grid_max_pct: Decimal,
    /// Core target multiplier while an up trend is confirmed.
    pub trend_up_core_multiplier: Decimal,
    /// Grid target multiplier while an up trend is confirmed.
    pub trend_up_grid_multiplier: Decimal,
    /// Core target multiplier while a down trend is confirmed.
    pub trend_down_core_multiplier: Decimal,
    /// Grid target multiplier while a down trend is confirmed.
    pub trend_down_grid_multiplier: Decimal,
    /// Position multiplier during high volatility; entries remain disabled.
    pub high_volatility_position_multiplier: Decimal,
    /// Capital sizing weights.
    pub position_sizing: PositionSizing,
    /// ATR/price target for volatility sizing; the multiplier is capped at one.
    pub position_volatility_target: Decimal,
    /// Absolute maximum quantity, including unresolved buys.
    pub max_position: Decimal,
    /// Maximum marked inventory / strategy equity.
    pub max_position_pct: Decimal,
    /// Maximum marked inventory and pending notional.
    pub max_notional: Decimal,
    /// Maximum capital reserved by this grid and retained inventory.
    pub max_grid_exposure: Decimal,
    /// Maximum inventory / broker equity for this instrument.
    pub max_asset_ratio: Decimal,
    /// Maximum inventory and pending buy notional / strategy equity.
    pub max_capital_utilization: Decimal,
    /// Drawdown from the strategy equity high-water mark.
    pub max_drawdown: Decimal,
    /// Loss relative to equity at the start of a UTC day.
    pub max_daily_loss: Decimal,
    /// Maximum unrealized loss / initial capital.
    pub max_unrealized_loss: Decimal,
    /// Maximum resets without a completed profitable cycle.
    pub max_consecutive_resets: u32,
    /// Maximum completed resets per UTC day, independently of profitable cycles.
    pub maximum_resets_per_day: u32,
    /// Maximum outstanding orders including unknown outcomes.
    pub max_orders: usize,
    /// Maximum configured levels per side.
    pub max_grid_levels: usize,
    /// Reserve fraction unavailable to buys.
    pub reserve_capital: Decimal,
    /// Reset minimum distance from the previous center.
    pub minimum_reset_distance: Decimal,
    /// Breakout overshoot in current ATR units; in-range resets use distance from the anchor.
    pub minimum_reset_atr_multiple: Decimal,
    /// Completed closes required outside the grid before a stock-adaptive reset.
    pub breakout_confirmation_bars: u32,
    /// Reset minimum interval in seconds.
    pub minimum_reset_interval_secs: u64,
    /// Relative spacing change which requests a volatility reset.
    pub volatility_reset_ratio: Decimal,
    /// Whether boundary breaks reset instead of terminating.
    pub enable_dynamic_reset: bool,
    /// Whether trend policies apply.
    pub enable_trend_filter: bool,
    /// Whether volatility thresholds apply.
    pub enable_volatility_filter: bool,
    /// Up trend policy.
    pub trend_up_policy: TrendPolicy,
    /// Down trend policy.
    pub trend_down_policy: TrendPolicy,
    /// Remaining fraction of levels for `ReduceGrid`.
    pub trend_level_fraction: Decimal,
    /// Spacing multiplier for `WiderGrid`.
    pub trend_spacing_multiplier: Decimal,
    /// ATR and directional-movement period.
    pub atr_period: usize,
    /// ADX smoothing period after directional movement is warm.
    pub adx_period: usize,
    /// Bollinger and moving-average window.
    pub ma_period: usize,
    /// Native average implementation for directional classification.
    pub regime_average: RegimeAverage,
    /// Require price to be on the trend side of the selected moving average.
    pub require_price_ma_confirmation: bool,
    /// Minimum fractional price/average distance when confirmation is enabled.
    pub price_ma_confirmation_pct: f64,
    /// Number of completed bars for slope calculation.
    pub slope_period: usize,
    /// Realized log-return volatility window.
    pub volatility_period: usize,
    /// Bollinger standard deviation multiplier.
    pub bollinger_k: f64,
    /// Upper ADX bound for ranging markets.
    pub adx_range_max: f64,
    /// Lower ADX bound for trends.
    pub adx_trend_min: f64,
    /// Absolute normalized MA slope threshold per bar.
    pub ma_slope_threshold: f64,
    /// Minimum ATR/price needed to open a grid.
    pub atr_pct_min: f64,
    /// Maximum ATR/price.
    pub atr_pct_max: f64,
    /// Maximum Bollinger bandwidth / middle.
    pub bollinger_width_max: f64,
    /// Maximum log-return volatility per bar (not annualized).
    pub realized_volatility_max: f64,
    /// Restrict stock-adaptive entry execution to 09:30-16:00 `America/New_York`.
    pub regular_session_only: bool,
    /// Absolute overnight gap fraction which pauses new entries.
    pub max_gap_pct: Decimal,
    /// Absolute overnight gap in prior completed-bar ATR units which pauses new entries.
    pub max_gap_atr_multiple: Decimal,
    /// Number of completed regular-session bars paused after a large gap.
    pub gap_recovery_bars: u32,
    /// Rolling completed-bar window used by the dollar-volume gate.
    pub liquidity_lookback_bars: usize,
    /// Minimum rolling average `close * volume`; zero disables this gate.
    pub minimum_average_dollar_volume: Decimal,
    /// Maximum observed bid/ask spread in basis points; zero disables this gate.
    pub maximum_spread_bps: Decimal,
    /// Minimum executable stock price; zero disables this gate.
    pub minimum_price: Decimal,
    /// Conservative maker fee fraction for spacing and reservations.
    pub maker_fee: Decimal,
    /// Conservative taker fee fraction for spacing and reservations.
    pub taker_fee: Decimal,
    /// Additional commission fraction, added to maker/taker rate.
    pub commission: Decimal,
    /// Estimated one-way slippage fraction used before submission.
    pub slippage: Decimal,
    /// Minimum expected net cycle profit as a fraction of entry notional.
    pub minimum_profit_margin: Decimal,
    /// Hard-limit response.
    pub risk_policy: RiskPolicy,
    /// Maximum time for an unresolved submission or cancellation.
    pub order_timeout_secs: u64,
    /// Maximum age of a completed signal bar for tick trading.
    pub max_signal_age_secs: u64,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            strategy_mode: StrategyMode::LegacyDgt,
            grid_levels: 10,
            spacing_mode: SpacingMode::Atr,
            spacing_pct: Decimal::new(1, 2),
            atr_multiplier: Decimal::new(75, 2),
            min_spacing_pct: Decimal::new(5, 3),
            max_spacing_pct: Decimal::new(3, 2),
            capital: Decimal::from(100_000),
            capital_allocation: Decimal::new(2, 1),
            initial_inventory_fraction: Decimal::ZERO,
            core_target_pct: Decimal::new(4, 1),
            grid_max_pct: Decimal::new(6, 1),
            trend_up_core_multiplier: Decimal::new(125, 2),
            trend_up_grid_multiplier: Decimal::new(5, 1),
            trend_down_core_multiplier: Decimal::new(5, 1),
            trend_down_grid_multiplier: Decimal::ZERO,
            high_volatility_position_multiplier: Decimal::new(25, 2),
            position_sizing: PositionSizing::Equal,
            position_volatility_target: Decimal::new(1, 2),
            max_position: Decimal::from(1000),
            max_position_pct: Decimal::new(2, 1),
            max_notional: Decimal::from(20_000),
            max_grid_exposure: Decimal::from(20_000),
            max_asset_ratio: Decimal::new(2, 1),
            max_capital_utilization: Decimal::new(5, 1),
            max_drawdown: Decimal::new(1, 1),
            max_daily_loss: Decimal::new(3, 2),
            max_unrealized_loss: Decimal::new(8, 2),
            max_consecutive_resets: 5,
            maximum_resets_per_day: 100,
            max_orders: 40,
            max_grid_levels: 30,
            reserve_capital: Decimal::new(5, 1),
            minimum_reset_distance: Decimal::new(1, 2),
            minimum_reset_atr_multiple: Decimal::ZERO,
            breakout_confirmation_bars: 2,
            minimum_reset_interval_secs: 300,
            volatility_reset_ratio: Decimal::new(5, 1),
            enable_dynamic_reset: true,
            enable_trend_filter: true,
            enable_volatility_filter: true,
            trend_up_policy: TrendPolicy::ReduceGrid,
            trend_down_policy: TrendPolicy::Disable,
            trend_level_fraction: Decimal::new(5, 1),
            trend_spacing_multiplier: Decimal::from(2),
            atr_period: 14,
            adx_period: 14,
            ma_period: 20,
            regime_average: RegimeAverage::Simple,
            require_price_ma_confirmation: false,
            price_ma_confirmation_pct: 0.0,
            slope_period: 5,
            volatility_period: 20,
            bollinger_k: 2.0,
            adx_range_max: 20.0,
            adx_trend_min: 25.0,
            ma_slope_threshold: 0.001,
            atr_pct_min: 0.0001,
            atr_pct_max: 0.05,
            bollinger_width_max: 0.15,
            realized_volatility_max: 0.04,
            regular_session_only: true,
            max_gap_pct: Decimal::new(8, 2),
            max_gap_atr_multiple: Decimal::from(3),
            gap_recovery_bars: 5,
            liquidity_lookback_bars: 20,
            minimum_average_dollar_volume: Decimal::ZERO,
            maximum_spread_bps: Decimal::new(30, 0),
            minimum_price: Decimal::from(5),
            maker_fee: Decimal::new(8, 4),
            taker_fee: Decimal::new(10, 4),
            commission: Decimal::ZERO,
            slippage: Decimal::new(5, 4),
            minimum_profit_margin: Decimal::new(5, 4),
            risk_policy: RiskPolicy::Hold,
            order_timeout_secs: 30,
            max_signal_age_secs: 180,
        }
    }
}

impl GridConfig {
    /// Validates limits before creating indicators, grids or orders.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid periods, nonfinite signals or inconsistent risk/cost bounds.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.grid_levels > 0
                && self.grid_levels <= self.max_grid_levels
                && self.max_grid_levels <= 1000,
            "Invalid grid level limit"
        );
        anyhow::ensure!(
            self.max_orders > 0
                && self.max_consecutive_resets > 0
                && self.maximum_resets_per_day > 0
                && self.breakout_confirmation_bars > 0
                && self.liquidity_lookback_bars > 0,
            "Order/reset limits must be positive"
        );
        for value in [
            self.capital,
            self.max_position,
            self.max_notional,
            self.max_grid_exposure,
            self.atr_multiplier,
        ] {
            anyhow::ensure!(
                value > Decimal::ZERO && value <= Decimal::from(1_000_000_000_000_u64),
                "Invalid capital, quantity or multiplier"
            );
        }
        for ratio in [
            self.capital_allocation,
            self.position_volatility_target,
            self.max_position_pct,
            self.max_asset_ratio,
            self.max_capital_utilization,
            self.max_drawdown,
            self.max_daily_loss,
            self.max_unrealized_loss,
            self.trend_level_fraction,
            self.core_target_pct,
            self.grid_max_pct,
        ] {
            anyhow::ensure!(
                ratio > Decimal::ZERO && ratio <= Decimal::ONE,
                "Risk ratios must be in (0, 1]"
            );
        }
        for ratio in [
            self.initial_inventory_fraction,
            self.reserve_capital,
            self.maker_fee,
            self.taker_fee,
            self.commission,
            self.slippage,
            self.minimum_profit_margin,
            self.minimum_reset_distance,
            self.max_gap_pct,
            self.trend_up_grid_multiplier,
            self.trend_down_core_multiplier,
            self.trend_down_grid_multiplier,
            self.high_volatility_position_multiplier,
        ] {
            anyhow::ensure!(
                ratio >= Decimal::ZERO && ratio < Decimal::ONE,
                "Fractions must be in [0, 1)"
            );
        }
        anyhow::ensure!(
            self.trend_up_core_multiplier > Decimal::ZERO
                && self.trend_up_core_multiplier <= Decimal::from(4)
                && self.core_target_pct + self.grid_max_pct <= Decimal::ONE,
            "Invalid stock target-position allocation"
        );
        anyhow::ensure!(
            self.min_spacing_pct > Decimal::ZERO
                && self.min_spacing_pct <= self.max_spacing_pct
                && self.max_spacing_pct < Decimal::ONE,
            "Invalid spacing bounds"
        );
        anyhow::ensure!(
            self.spacing_pct >= self.min_spacing_pct && self.spacing_pct <= self.max_spacing_pct,
            "Fixed spacing outside bounds"
        );
        anyhow::ensure!(
            self.minimum_reset_atr_multiple >= Decimal::ZERO
                && self.minimum_reset_atr_multiple <= Decimal::from(100)
                && self.max_gap_atr_multiple >= Decimal::ZERO
                && self.max_gap_atr_multiple <= Decimal::from(100)
                && self.minimum_average_dollar_volume >= Decimal::ZERO
                && self.maximum_spread_bps >= Decimal::ZERO
                && self.minimum_price >= Decimal::ZERO,
            "Reset ATR buffer must be in [0, 100]"
        );
        anyhow::ensure!(
            self.volatility_reset_ratio > Decimal::ZERO
                && self.trend_spacing_multiplier >= Decimal::ONE
                && self.trend_spacing_multiplier <= Decimal::from(100),
            "Invalid reset or trend multiplier"
        );
        anyhow::ensure!(
            self.order_timeout_secs > 0
                && self.max_signal_age_secs > 0
                && self.order_timeout_secs <= u64::MAX / 1_000_000_000
                && self.max_signal_age_secs <= u64::MAX / 1_000_000_000,
            "Timeouts must be positive and representable as nanoseconds"
        );
        for period in [
            self.atr_period,
            self.adx_period,
            self.ma_period,
            self.slope_period,
            self.volatility_period,
        ] {
            anyhow::ensure!((2..=1024).contains(&period), "Periods must be in [2, 1024]");
        }
        for value in [
            self.bollinger_k,
            self.adx_range_max,
            self.adx_trend_min,
            self.ma_slope_threshold,
            self.price_ma_confirmation_pct,
            self.atr_pct_min,
            self.atr_pct_max,
            self.bollinger_width_max,
            self.realized_volatility_max,
        ] {
            anyhow::ensure!(
                value.is_finite() && value >= 0.0,
                "Signal thresholds must be finite and nonnegative"
            );
        }
        anyhow::ensure!(
            self.bollinger_k > 0.0
                && self.adx_range_max < self.adx_trend_min
                && self.adx_trend_min <= 100.0
                && self.atr_pct_min < self.atr_pct_max
                && self.realized_volatility_max > 0.0,
            "Inconsistent regime thresholds"
        );
        anyhow::ensure!(
            self.price_ma_confirmation_pct < 1.0,
            "Price/MA confirmation must be below one"
        );
        anyhow::ensure!(
            self.cost_floor() <= self.max_spacing_pct,
            "Maximum grid spacing cannot cover costs"
        );
        Ok(())
    }

    /// Conservative round-trip cost and profit-margin floor, including exit notional fees.
    #[must_use]
    pub fn cost_floor(&self) -> Decimal {
        let cost = self.maker_fee.max(self.taker_fee) + self.commission + self.slippage;
        if cost >= Decimal::ONE {
            return Decimal::MAX;
        }
        (Decimal::from(2) * cost + self.minimum_profit_margin) / (Decimal::ONE - cost)
    }
}
