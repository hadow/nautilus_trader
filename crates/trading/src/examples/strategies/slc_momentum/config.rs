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
//  See the License for the specific language governing permissions and limitations under the License.
// -------------------------------------------------------------------------------------------------

//! SLC Momentum 的配置契约。
//!
//! 所有窗口、确认模式、风险上限和会话日历都在策略启动前统一校验；价格、资金与仓位
//! 使用 `Decimal` 或 Nautilus 领域类型，避免交易计算因二进制浮点产生舍入漂移。

use nautilus_core::UnixNanos;
use nautilus_model::identifiers::InstrumentId;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::strategy::StrategyConfig;

/// An exchange-provided regular session, expressed in UTC nanoseconds.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub open: UnixNanos,
    pub close: UnixNanos,
}

/// Point-in-time security-master record; intervals are half-open.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SymbolMetadata {
    pub instrument_id: InstrumentId,
    pub sector: String,
    pub sector_etf: InstrumentId,
    pub market_cap: Decimal,
    pub effective_from: UnixNanos,
    pub effective_until: UnixNanos,
    pub known_at: UnixNanos,
}

/// Direction of the complete entry/protection/exit lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradeSide {
    Long,
    Short,
}

impl TradeSide {
    pub(super) fn sign(self) -> Decimal {
        match self {
            Self::Long => Decimal::ONE,
            Self::Short => -Decimal::ONE,
        }
    }
    pub(super) fn strength(self, percentile: f64) -> f64 {
        match self {
            Self::Long => percentile,
            Self::Short => 100.0 - percentile,
        }
    }
    pub(super) fn aligned(self, change: f64) -> bool {
        match self {
            Self::Long => change > 0.0,
            Self::Short => change < 0.0,
        }
    }
    pub(super) fn structure(self) -> super::Structure {
        match self {
            Self::Long => super::Structure::Bullish,
            Self::Short => super::Structure::Bearish,
        }
    }
    pub(super) fn entry_side(self) -> nautilus_model::enums::OrderSide {
        match self {
            Self::Long => nautilus_model::enums::OrderSide::Buy,
            Self::Short => nautilus_model::enums::OrderSide::Sell,
        }
    }
    pub(super) fn exit_side(self) -> nautilus_model::enums::OrderSide {
        match self {
            Self::Long => nautilus_model::enums::OrderSide::Sell,
            Self::Short => nautilus_model::enums::OrderSide::Buy,
        }
    }
    pub(super) fn tighter(
        self,
        a: nautilus_model::types::Price,
        b: nautilus_model::types::Price,
    ) -> nautilus_model::types::Price {
        match self {
            Self::Long => a.max(b),
            Self::Short => a.min(b),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryMode {
    Market,
    #[default]
    Limit,
    Stop,
    StopLimit,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConfirmationMode {
    /// 触区后等待随机指标离开极值区，是最严格的传统确认路径。
    #[default]
    StochasticReentry,
    /// 用触碰后的价格反应代替随机指标重返。
    PriceResponse,
    /// 将结构、区域、随机指标和量能作为加权证据，不再要求它们全部同时成立。
    WeightedEvidence,
    /// Completed close crosses the opening extreme of the preceding opposite candle run.
    Cisd,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StructureMode {
    #[default]
    Swing,
    /// C2 sweeps C1 and reclaims its boundary; context lasts through C3/C4.
    SweepReclaim,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TargetMode {
    #[default]
    FixedR,
    Atr,
    Structure,
}

/// Predeclared research variants; F is the production signal policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Ablation {
    A,
    B,
    C,
    D,
    E,
    #[default]
    F,
    /// SLC-HTF v1: structure, level and confirmation without auxiliary hard gates.
    SlcOnly,
    WithoutStochastic,
    WithoutRegime,
}

impl Ablation {
    pub(super) fn slc(self) -> bool {
        self != Self::A
    }
    pub(super) fn momentum(self) -> bool {
        self != Self::SlcOnly
    }
    pub(super) fn vwap(self) -> bool {
        !matches!(self, Self::A | Self::B | Self::D | Self::SlcOnly)
    }
    pub(super) fn volume(self) -> bool {
        !matches!(self, Self::A | Self::B | Self::C | Self::SlcOnly)
    }
    pub(super) fn regime(self) -> bool {
        matches!(self, Self::F | Self::WithoutStochastic)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MomentumConfig {
    pub lookbacks: Vec<usize>,
    pub return_weights: Vec<f64>,
    pub relative_strength_lookback: usize,
    pub spy_weight: f64,
    pub qqq_weight: f64,
    pub sector_weight: f64,
    pub relative_volume_weight: f64,
    pub intraday_weight: f64,
    pub min_percentile: f64,
    pub refresh_minutes: u64,
    pub daily_only: bool,
    pub minimum_price: Decimal,
    pub minimum_market_cap: Decimal,
    pub minimum_average_dollar_volume: Decimal,
    pub minimum_atr_fraction: f64,
    pub maximum_atr_fraction: f64,
    pub minimum_relative_volume: f64,
    pub maximum_gap_fraction: f64,
    pub minimum_universe_size: usize,
    pub minimum_coverage: f64,
}

impl Default for MomentumConfig {
    fn default() -> Self {
        Self {
            lookbacks: vec![1, 5, 10, 20],
            return_weights: vec![0.0, 1.0, 1.0, 1.0],
            relative_strength_lookback: 5,
            spy_weight: 0.0,
            qqq_weight: 0.0,
            sector_weight: 1.0,
            relative_volume_weight: 1.0,
            intraday_weight: 1.0,
            min_percentile: 80.0,
            refresh_minutes: 5,
            daily_only: false,
            minimum_price: Decimal::from(5),
            minimum_market_cap: Decimal::from(1_000_000_000_u64),
            minimum_average_dollar_volume: Decimal::from(20_000_000),
            minimum_atr_fraction: 0.01,
            maximum_atr_fraction: 0.15,
            minimum_relative_volume: 0.5,
            maximum_gap_fraction: 0.15,
            minimum_universe_size: 5,
            minimum_coverage: 0.95,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlcConfig {
    pub htf_minutes: u64,
    pub structure_mode: StructureMode,
    pub ltf_minutes: u64,
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub require_htf_ema: bool,
    pub require_intraday_trend: bool,
    pub minimum_intraday_return: f64,
    pub atr_period: usize,
    pub stochastic_k: usize,
    pub stochastic_d: usize,
    pub stochastic_smoothing: usize,
    pub oversold: f64,
    pub confirmation_mode: ConfirmationMode,
    pub confirmation_window_bars: usize,
    pub max_level_age_bars: usize,
    pub max_level_tests: usize,
    pub max_level_breaks: usize,
    pub reclaim_impulse_atr: f64,
    pub impulse_atr: f64,
    pub impulse_volume: f64,
    pub min_level_score: f64,
    pub max_level_distance_atr: f64,
    pub confirmation_weights: [f64; 8],
    pub confirmation_threshold: f64,
    pub neutral_extra_score: f64,
    pub minimum_confirmation_volume: f64,
    pub opening_range_minutes: u64,
    pub regime_votes: usize,
    pub regime_momentum_minutes: usize,
    pub breadth_threshold: Option<f64>,
}

impl Default for SlcConfig {
    fn default() -> Self {
        Self {
            htf_minutes: 60,
            structure_mode: StructureMode::Swing,
            ltf_minutes: 5,
            ema_fast: 20,
            ema_slow: 50,
            require_htf_ema: true,
            require_intraday_trend: false,
            minimum_intraday_return: 0.0,
            atr_period: 14,
            stochastic_k: 5,
            stochastic_d: 3,
            stochastic_smoothing: 3,
            oversold: 20.0,
            confirmation_mode: ConfirmationMode::StochasticReentry,
            confirmation_window_bars: 4,
            max_level_age_bars: 24,
            max_level_tests: 1,
            max_level_breaks: 1,
            reclaim_impulse_atr: 0.5,
            impulse_atr: 1.5,
            impulse_volume: 1.2,
            min_level_score: 6.0,
            max_level_distance_atr: 1.0,
            confirmation_weights: [2.0, 2.0, 2.0, 1.0, 1.0, 1.0, 1.0, 0.0],
            confirmation_threshold: 9.0,
            neutral_extra_score: 1.0,
            minimum_confirmation_volume: 1.2,
            opening_range_minutes: 15,
            regime_votes: 2,
            regime_momentum_minutes: 5,
            breadth_threshold: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskConfig {
    pub risk_per_trade: Decimal,
    pub neutral_multiplier: Decimal,
    pub max_daily_loss: Decimal,
    pub max_total_risk: Decimal,
    pub max_sector_risk: Decimal,
    pub max_position_value: Decimal,
    pub max_notional: Decimal,
    pub max_total_exposure: Decimal,
    pub max_sector_exposure: Decimal,
    pub max_positions: usize,
    pub max_sector_positions: usize,
    pub max_correlated_positions: usize,
    pub max_consecutive_losses: usize,
    pub correlation_lookback: usize,
    pub correlation_threshold: f64,
    pub correlation_penalty: Decimal,
    pub participation: Decimal,
    pub commission_rate: Decimal,
    pub stop_buffer_atr: Decimal,
    pub minimum_stop_distance: Decimal,
    pub short_margin_ratio: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            risk_per_trade: Decimal::new(5, 3),
            neutral_multiplier: Decimal::new(5, 1),
            max_daily_loss: Decimal::new(2, 2),
            max_total_risk: Decimal::new(2, 2),
            max_sector_risk: Decimal::new(1, 2),
            max_position_value: Decimal::from(25_000),
            max_notional: Decimal::from(100_000),
            max_total_exposure: Decimal::new(8, 1),
            max_sector_exposure: Decimal::new(3, 1),
            max_positions: 4,
            max_sector_positions: 2,
            max_correlated_positions: 2,
            max_consecutive_losses: 3,
            correlation_lookback: 20,
            correlation_threshold: 0.8,
            correlation_penalty: Decimal::new(5, 1),
            participation: Decimal::new(1, 2),
            commission_rate: Decimal::new(1, 4),
            stop_buffer_atr: Decimal::new(2, 1),
            minimum_stop_distance: Decimal::new(10, 2),
            short_margin_ratio: Decimal::new(15, 1),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExitConfig {
    pub partial_r: Decimal,
    pub partial_fraction: Decimal,
    pub target_r: Decimal,
    pub target_mode: TargetMode,
    pub atr_multiplier: Decimal,
    pub trailing_enabled: bool,
    pub trailing_atr: Decimal,
    pub chandelier: bool,
    pub vwap_loss: bool,
    pub momentum_failure: bool,
    pub momentum_drop: f64,
    pub selling_volume: f64,
}

impl Default for ExitConfig {
    fn default() -> Self {
        Self {
            partial_r: Decimal::ONE,
            partial_fraction: Decimal::new(5, 1),
            target_r: Decimal::from(2),
            target_mode: TargetMode::FixedR,
            atr_multiplier: Decimal::from(3),
            trailing_enabled: true,
            trailing_atr: Decimal::from(3),
            chandelier: true,
            vwap_loss: false,
            momentum_failure: true,
            momentum_drop: 25.0,
            selling_volume: 1.5,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlcMomentumConfig {
    pub base: StrategyConfig,
    pub universe: Vec<SymbolMetadata>,
    pub benchmarks: [InstrumentId; 3],
    /// Explicit regime subset; None requires all three benchmarks.
    pub regime_benchmarks: Option<Vec<InstrumentId>>,
    pub sessions: Vec<Session>,
    pub momentum: MomentumConfig,
    pub market: Option<super::market::MarketSelectionConfig>,
    pub slc: SlcConfig,
    pub risk: RiskConfig,
    pub exit: ExitConfig,
    pub ablation: Ablation,
    pub entry_mode: EntryMode,
    pub directions: Vec<TradeSide>,
    pub trading_windows: Vec<[u64; 2]>,
    pub flatten_before_close_minutes: u64,
    pub max_spread_bps: Decimal,
    pub max_slippage_bps: Decimal,
    pub max_quote_age_ms: u64,
    pub batch_delay_ms: u64,
    pub entry_timeout_minutes: u64,
    pub trading_start: UnixNanos,
    pub trading_end: UnixNanos,
    pub dry_run: bool,
    pub longbridge: bool,
}

impl Default for SlcMomentumConfig {
    fn default() -> Self {
        Self {
            base: StrategyConfig::default(),
            universe: vec![],
            sessions: vec![],
            benchmarks: [
                "SPY.US.SIM".into(),
                "QQQ.US.SIM".into(),
                "IWM.US.SIM".into(),
            ],
            regime_benchmarks: None,
            momentum: MomentumConfig::default(),
            market: None,
            slc: SlcConfig::default(),
            risk: RiskConfig::default(),
            exit: ExitConfig::default(),
            ablation: Ablation::F,
            entry_mode: EntryMode::Limit,
            directions: vec![TradeSide::Long],
            trading_windows: vec![[5, 120]],
            flatten_before_close_minutes: 5,
            max_spread_bps: Decimal::from(10),
            max_slippage_bps: Decimal::from(10),
            max_quote_age_ms: 5_000,
            batch_delay_ms: 2_000,
            entry_timeout_minutes: 2,
            trading_start: UnixNanos::default(),
            trading_end: UnixNanos::from(u64::MAX),
            dry_run: true,
            longbridge: false,
        }
    }
}

impl SlcMomentumConfig {
    pub(super) fn candidate(&self, percentile: f64) -> bool {
        self.directions
            .iter()
            .any(|side| side.strength(percentile) >= self.momentum.min_percentile)
    }

    /// Validates dimensions, timestamps, probability bounds and supported execution modes.
    ///
    /// # Errors
    /// Returns an error for inconsistent, missing or unsafe configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        let m = &self.momentum;
        let s = &self.slc;
        let r = &self.risk;
        let regime = self.regime_benchmarks();
        anyhow::ensure!(
            !regime.is_empty()
                && regime.iter().all(|id| self.benchmarks.contains(id))
                && regime
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == regime.len()
                && s.regime_votes <= regime.len(),
            "regime benchmarks must be a nonempty unique subset with enough votes"
        );
        anyhow::ensure!(
            !self.directions.is_empty()
                && self.directions.len() <= 2
                && (self.directions.len() == 1 || self.directions[0] != self.directions[1]),
            "directions must contain LONG and/or SHORT without duplicates"
        );
        anyhow::ensure!(
            s.minimum_intraday_return.is_finite()
                && (0.0..1.0).contains(&s.minimum_intraday_return)
                && (Decimal::ONE..=Decimal::from(10)).contains(&r.short_margin_ratio),
            "invalid intraday trend floor or short margin ratio"
        );
        if let Some(market) = &self.market {
            market.validate(self)?;
        }
        anyhow::ensure!(
            !self.universe.is_empty() && !self.sessions.is_empty(),
            "universe and sessions are required"
        );
        anyhow::ensure!(
            self.trading_start < self.trading_end,
            "trading interval is empty"
        );
        anyhow::ensure!(
            m.lookbacks.len() == m.return_weights.len() && !m.lookbacks.is_empty(),
            "lookback/weight dimensions differ"
        );
        anyhow::ensure!(
            m.lookbacks.iter().all(|n| (1..=252).contains(n))
                && (1..=252).contains(&m.relative_strength_lookback),
            "daily lookbacks must be in 1..=252"
        );
        let weights = m
            .return_weights
            .iter()
            .copied()
            .chain([
                m.spy_weight,
                m.qqq_weight,
                m.sector_weight,
                m.relative_volume_weight,
                m.intraday_weight,
            ])
            .collect::<Vec<_>>();
        let weight_sum = weights.iter().sum::<f64>();
        anyhow::ensure!(
            weights.iter().all(|w| w.is_finite() && *w >= 0.0)
                && weight_sum > 0.0
                && (weight_sum * 100.0).is_finite(),
            "ranking weights must be finite, nonnegative and nonzero"
        );
        anyhow::ensure!(
            (1..=390).contains(&m.refresh_minutes)
                && m.minimum_universe_size >= 2
                && (0.0..=100.0).contains(&m.min_percentile)
                && (0.0..=1.0).contains(&m.minimum_coverage)
                && m.minimum_coverage > 0.0,
            "invalid ranking frequency, size or percentile"
        );
        anyhow::ensure!(
            m.minimum_price > Decimal::ZERO
                && m.minimum_market_cap >= Decimal::ZERO
                && m.minimum_average_dollar_volume > Decimal::ZERO,
            "invalid universe floors"
        );
        anyhow::ensure!(
            m.minimum_atr_fraction.is_finite()
                && m.minimum_atr_fraction > 0.0
                && m.maximum_atr_fraction.is_finite()
                && m.maximum_atr_fraction >= m.minimum_atr_fraction
                && m.minimum_relative_volume.is_finite()
                && m.minimum_relative_volume >= 0.0
                && m.maximum_gap_fraction.is_finite()
                && m.maximum_gap_fraction >= 0.0,
            "invalid volatility, volume or gap filter"
        );
        anyhow::ensure!(
            [30, 60, 240].contains(&s.htf_minutes) && s.ltf_minutes == 5,
            "HTF must be 30/60/240 minutes and LTF 5 minutes"
        );
        anyhow::ensure!(
            s.ema_fast > 0
                && s.ema_fast < s.ema_slow
                && s.ema_slow <= 1_024
                && (1..=1_024).contains(&s.atr_period),
            "invalid indicator periods"
        );
        anyhow::ensure!(
            [s.stochastic_k, s.stochastic_d, s.stochastic_smoothing]
                .iter()
                .all(|n| (1..=1_024).contains(n)),
            "invalid stochastic periods"
        );
        anyhow::ensure!(
            (0.0..50.0).contains(&s.oversold)
                && s.confirmation_window_bars > 0
                && s.max_level_age_bars > 0
                && s.max_level_tests > 0,
            "invalid confirmation or level lifetime"
        );
        anyhow::ensure!(
            s.max_level_breaks <= 1
                && s.reclaim_impulse_atr.is_finite()
                && s.reclaim_impulse_atr > 0.0,
            "allow at most one level break and require a positive reclaim impulse"
        );
        anyhow::ensure!(
            s.confirmation_weights
                .iter()
                .all(|x| x.is_finite() && *x >= 0.0)
                && s.confirmation_weights.iter().sum::<f64>().is_finite()
                && s.confirmation_threshold.is_finite()
                && s.confirmation_threshold > 0.0
                && s.confirmation_threshold <= s.confirmation_weights.iter().sum::<f64>(),
            "invalid confirmation score"
        );
        anyhow::ensure!(
            [
                s.impulse_atr,
                s.impulse_volume,
                s.min_level_score,
                s.max_level_distance_atr,
                s.minimum_confirmation_volume
            ]
            .iter()
            .all(|x| x.is_finite() && *x > 0.0)
                && s.neutral_extra_score.is_finite()
                && s.neutral_extra_score >= 0.0,
            "invalid level/volume threshold"
        );
        anyhow::ensure!(
            (1..=3).contains(&s.regime_votes)
                && (1..=30).contains(&s.opening_range_minutes)
                && (1..=60).contains(&s.regime_momentum_minutes)
                && s.breadth_threshold.is_none_or(|x| (0.5..=1.0).contains(&x)),
            "invalid regime configuration"
        );
        anyhow::ensure!(
            [
                r.risk_per_trade,
                r.neutral_multiplier,
                r.max_daily_loss,
                r.max_total_risk,
                r.max_sector_risk,
                r.max_total_exposure,
                r.max_sector_exposure,
                r.correlation_penalty,
                r.participation
            ]
            .iter()
            .all(|x| *x > Decimal::ZERO && *x <= Decimal::ONE),
            "risk fractions must be in (0, 1]"
        );
        anyhow::ensure!(
            r.max_position_value > Decimal::ZERO
                && r.max_notional > Decimal::ZERO
                && r.minimum_stop_distance > Decimal::ZERO
                && r.stop_buffer_atr >= Decimal::ZERO
                && r.commission_rate >= Decimal::ZERO
                && r.commission_rate < Decimal::ONE,
            "invalid monetary limits"
        );
        anyhow::ensure!(
            [
                r.max_positions,
                r.max_sector_positions,
                r.max_correlated_positions,
                r.max_consecutive_losses
            ]
            .iter()
            .all(|n| *n > 0)
                && (2..=252).contains(&r.correlation_lookback)
                && (0.0..=1.0).contains(&r.correlation_threshold),
            "invalid portfolio limits"
        );
        anyhow::ensure!(
            self.exit.partial_fraction >= Decimal::ZERO
                && self.exit.partial_fraction < Decimal::ONE
                && self.exit.partial_r > Decimal::ZERO
                && self.exit.target_r > self.exit.partial_r
                && self.exit.atr_multiplier > Decimal::ZERO
                && self.exit.trailing_atr > Decimal::ZERO
                && (0.0..=100.0).contains(&self.exit.momentum_drop)
                && self.exit.selling_volume.is_finite()
                && self.exit.selling_volume > 0.0,
            "invalid exit policy"
        );
        anyhow::ensure!(
            !self.trading_windows.is_empty()
                && self
                    .trading_windows
                    .iter()
                    .all(|w| w[0] > 0 && w[0] < w[1] && w[1] <= 390)
                && self.flatten_before_close_minutes > 0
                && self.flatten_before_close_minutes < 210,
            "invalid session windows"
        );
        anyhow::ensure!(
            self.max_quote_age_ms > 0
                && self.max_quote_age_ms <= 60_000
                && self.entry_timeout_minutes > 0
                && self.entry_timeout_minutes <= 60
                && self.max_spread_bps >= Decimal::ZERO
                && self.max_spread_bps < Decimal::from(10_000)
                && self.max_slippage_bps >= Decimal::ZERO
                && self.max_slippage_bps < Decimal::from(10_000),
            "invalid execution limits"
        );
        anyhow::ensure!(
            !self.longbridge || matches!(self.entry_mode, EntryMode::Market | EntryMode::Limit),
            "Longbridge native stop entries are unsupported; use market/limit"
        );
        anyhow::ensure!(
            self.batch_delay_ms < 60_000,
            "batch delay must be less than one minute"
        );
        let mut previous_close = UnixNanos::default();
        for session in &self.sessions {
            anyhow::ensure!(
                session.open < session.close
                    && session.open >= previous_close
                    && session.open.as_u64().is_multiple_of(super::data::MINUTE)
                    && (session.close.as_u64() - session.open.as_u64()) % super::data::MINUTE == 0,
                "sessions overlap, are unsorted or have non-minute boundaries"
            );
            let open = session
                .open
                .to_datetime_utc()
                .to_zoned(nautilus_core::datetime::get_timezone("America/New_York")?);
            anyhow::ensure!(
                open.hour() == 9
                    && open.minute() == 30
                    && open.second() == 0
                    && (session.close.as_u64() - session.open.as_u64())
                        <= 390 * super::data::MINUTE,
                "session must open 09:30 ET and last at most 390 minutes"
            );
            previous_close = session.close;
        }
        for (i, meta) in self.universe.iter().enumerate() {
            anyhow::ensure!(
                !meta.sector.is_empty()
                    && meta.market_cap > Decimal::ZERO
                    && meta.effective_from < meta.effective_until,
                "invalid security-master record"
            );
            anyhow::ensure!(
                !self.benchmarks.contains(&meta.instrument_id),
                "benchmark cannot be a trading candidate"
            );
            anyhow::ensure!(
                !self.universe[..i]
                    .iter()
                    .any(|old| old.instrument_id == meta.instrument_id
                        && old.effective_from < meta.effective_until
                        && meta.effective_from < old.effective_until),
                "overlapping security-master records"
            );
        }
        Ok(())
    }

    /// Returns the explicitly required regime indices, defaulting to all benchmarks.
    #[must_use]
    pub fn regime_benchmarks(&self) -> &[InstrumentId] {
        self.regime_benchmarks
            .as_deref()
            .unwrap_or(&self.benchmarks)
    }

    #[must_use]
    pub fn instrument_ids(&self) -> Vec<InstrumentId> {
        let mut ids = self.benchmarks.to_vec();
        for meta in &self.universe {
            ids.extend([meta.instrument_id, meta.sector_etf]);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub(super) fn metadata(&self, id: InstrumentId, at: UnixNanos) -> Option<&SymbolMetadata> {
        self.universe.iter().find(|m| {
            m.instrument_id == id
                && m.known_at <= at
                && m.effective_from <= at
                && at < m.effective_until
        })
    }

    pub(super) fn session(&self, at: UnixNanos) -> Option<Session> {
        let index = self.sessions.partition_point(|s| s.open < at);
        index
            .checked_sub(1)
            .and_then(|i| self.sessions.get(i))
            .copied()
            .filter(|s| at <= s.close)
    }
}
