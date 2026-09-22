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

//! 市场环境、HTF 结构、SLC 关键区和确认信号状态机。
//!
//! 关键区只能由已完成的高低周期 bar 创建、触碰和失效。`WEIGHTED_EVIDENCE` 允许证据
//! 以分数互补，但仍保留数据完整性、方向、时效和极端超买/超卖等安全门槛。

use std::collections::{BTreeMap, VecDeque};

use nautilus_core::UnixNanos;
use nautilus_indicators::indicator::{Indicator, MovingAverage};
use nautilus_model::{data::Bar, identifiers::InstrumentId, types::Price};
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::{Deserialize, Serialize};

use super::{
    Ablation, ConfirmationMode, MomentumRank, SlcConfig, SlcMomentumConfig, TradeSide,
    data::SymbolFeatures,
};

fn confirmation_bitmap(flags: [bool; 8]) -> u8 {
    flags
        .iter()
        .enumerate()
        .fold(0, |bits, (index, flag)| bits | (u8::from(*flag) << index))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketRegime {
    Bullish,
    Neutral,
    Bearish,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Structure {
    Bullish,
    Bearish,
    #[default]
    Range,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SetupState {
    #[default]
    Scanning,
    Candidate,
    HtfValid,
    LevelFound,
    WaitingConfirmation,
    SignalReady,
    OrderSubmitted,
    PositionOpen,
    Managing,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NoTrade {
    Warmup,
    DataMissing,
    MetadataMissing,
    HtfRange,
    MarketBearish,
    MarketBullish,
    IntradayTrendMismatch,
    DirectionDisabled,
    MomentumWeak,
    LevelInvalid,
    LevelOvertested,
    RvolLow,
    SpreadTooWide,
    LiquidityTooLow,
    RiskTooHigh,
    MaxDailyLoss,
    MaxExposure,
    LateSession,
    VolatilityInvalid,
    ConfirmationMissing,
    DuplicateOrder,
    ShortDisabled,
    QuoteStale,
    CorrelationUnknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct SlcSignal {
    pub symbol: InstrumentId,
    pub timestamp: UnixNanos,
    pub available_at: UnixNanos,
    pub side: TradeSide,
    pub intraday_return: f64,
    pub sector: String,
    pub momentum: MomentumRank,
    pub market_regime: MarketRegime,
    pub structure: Structure,
    pub structure_score: f64,
    pub structure_confirmed_at: Option<UnixNanos>,
    pub cisd_confirmed_at: Option<UnixNanos>,
    pub cisd_level: Option<Price>,
    pub level_type: String,
    pub level_low: Price,
    pub level_high: Price,
    pub level_created: UnixNanos,
    pub level_tests: usize,
    pub level_breaks: usize,
    pub level_reclaimed_at: Option<UnixNanos>,
    pub level_score: f64,
    pub confirmation_score: f64,
    /// 实际满足的确认项位图；位序与 `SlcConfig::confirmation_weights` 一致。
    pub confirmation_flags: u8,
    /// 本次消融配置启用的确认项位图。
    pub confirmation_enabled: u8,
    pub stochastic_k: f64,
    pub stochastic_d: f64,
    pub vwap: f64,
    pub anchored_vwap: f64,
    pub relative_volume: f64,
    pub entry_price: Price,
    pub atr: Decimal,
    pub setup_type: String,
}

pub(super) fn market_regime(
    features: &BTreeMap<InstrumentId, SymbolFeatures>,
    ts: UnixNanos,
    c: &SlcMomentumConfig,
) -> Option<MarketRegime> {
    if c.market.is_some()
        && c.regime_benchmarks()
            .iter()
            .any(|id| features.get(id).is_none_or(|f| !f.complete_at(ts)))
    {
        return None;
    }
    let votes = c
        .regime_benchmarks()
        .iter()
        .map(|id| features.get(id)?.regime_vote(ts, &c.slc))
        .collect::<Option<Vec<_>>>()?;
    let mut bullish = votes.iter().filter(|v| **v > 0).count() >= c.slc.regime_votes;
    let bearish = votes.iter().filter(|v| **v < 0).count() >= c.slc.regime_votes;
    if let Some(threshold) = c.slc.breadth_threshold {
        let active = c
            .universe
            .iter()
            .filter(|m| m.known_at <= ts && m.effective_from <= ts && ts < m.effective_until)
            .map(|m| m.instrument_id)
            .collect::<std::collections::BTreeSet<_>>();
        let universe = features
            .iter()
            .filter(|(id, f)| active.contains(id) && f.last.is_some_and(|b| b.ts_event == ts))
            .map(|(_, f)| f)
            .collect::<Vec<_>>();
        if universe.len() < c.momentum.minimum_universe_size {
            return None;
        }
        let breadth = universe
            .iter()
            .filter(|f| f.last.is_some_and(|b| b.close.as_f64() > f.vwap.value))
            .count() as f64
            / universe.len() as f64;
        bullish &= breadth >= threshold;
    }
    Some(if bearish {
        MarketRegime::Bearish
    } else if bullish {
        MarketRegime::Bullish
    } else {
        MarketRegime::Neutral
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) enum LevelKind {
    Demand,
    Supply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum LevelPhase {
    Active,
    Broken,
    AwaitingRetest,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Level {
    pub(super) kind: LevelKind,
    pub(super) low: Price,
    pub(super) high: Price,
    pub(super) created: UnixNanos,
    pub(super) age: usize,
    pub(super) tests: usize,
    pub(super) breaks: usize,
    pub(super) impulse: f64,
    pub(super) volume: f64,
    inside: bool,
    armed: bool,
    reentry_at: Option<UnixNanos>,
    touch_at: Option<UnixNanos>,
    touch_high: Price,
    touch_low: Price,
    phase: LevelPhase,
    was_adverse: bool,
    reclaimed_at: Option<UnixNanos>,
    confirmation_age: usize,
    price_volume: f64,
    total_volume: f64,
    consumed: bool,
}

impl Level {
    pub(super) fn available(&self) -> bool {
        !self.consumed && self.phase != LevelPhase::Broken
    }

    pub(super) fn score(
        &self,
        c: &SlcConfig,
        bullish: bool,
        momentum: bool,
        close: Price,
        atr: f64,
    ) -> f64 {
        let distance = ((close.as_f64() - self.high.as_f64())
            .max(self.low.as_f64() - close.as_f64()))
        .max(0.0)
            / atr;
        self.impulse.min(3.0)
            + self.volume.min(2.0)
            + (1.0 - self.age as f64 / c.max_level_age_bars as f64).max(0.0)
            + f64::from(bullish)
            + f64::from(momentum)
            + (1.0 - distance).max(0.0)
    }

    fn reset_confirmation(&mut self) {
        self.inside = false;
        self.armed = false;
        self.reentry_at = None;
        self.touch_at = None;
        self.confirmation_age = 0;
    }

    pub(super) fn observe(&mut self, bar: Bar, c: &SlcConfig, k: Option<f64>, atr: f64) {
        self.age += 1;
        self.price_volume += bar.close.as_f64() * bar.volume.as_f64();
        self.total_volume += bar.volume.as_f64();
        self.consumed |= self.age > c.max_level_age_bars;
        if self.consumed {
            return;
        }
        let adverse = match self.kind {
            LevelKind::Demand => bar.close < self.low,
            LevelKind::Supply => bar.close > self.high,
        };
        if adverse {
            if !self.was_adverse {
                self.breaks += 1;
            }
            self.phase = LevelPhase::Broken;
            self.was_adverse = true;
            self.reset_confirmation();
            self.consumed |= self.breaks > c.max_level_breaks;
            return;
        }
        self.was_adverse = false;
        if self.phase == LevelPhase::Broken {
            let directional_close = match self.kind {
                LevelKind::Demand => bar.close > self.high && bar.close > bar.open,
                LevelKind::Supply => bar.close < self.low && bar.close < bar.open,
            };
            if directional_close
                && atr.is_finite()
                && atr > 0.0
                && (bar.close.as_f64() - bar.open.as_f64()).abs() >= c.reclaim_impulse_atr * atr
            {
                self.phase = LevelPhase::AwaitingRetest;
                self.reclaimed_at = Some(bar.ts_event);
            }
            // A completed reclaim candle cannot prove a subsequent intrabar retest
            return;
        }
        let touches = bar.low <= self.high && bar.high >= self.low;
        if touches && !self.inside {
            self.tests += 1;
            self.touch_at = Some(bar.ts_event);
            self.touch_high = bar.high;
            self.touch_low = bar.low;
            self.confirmation_age = 0;
            self.armed = false;
            self.reentry_at = None;
            self.phase = LevelPhase::Active;
        }
        self.inside = touches;
        if self.touch_at.is_some() {
            self.confirmation_age += 1;
            if let Some(k) = k {
                let extreme = match self.kind {
                    LevelKind::Demand => k < c.oversold,
                    LevelKind::Supply => k > 100.0 - c.oversold,
                };
                if extreme {
                    self.armed = true;
                    self.reentry_at = None;
                } else if self.armed && self.reentry_at.is_none() {
                    self.reentry_at = Some(bar.ts_event);
                }
            }
        }
        self.consumed |= self.tests > c.max_level_tests;
    }

    pub(super) fn confirmed(
        &self,
        bar: Bar,
        k: f64,
        previous_k: Option<f64>,
        c: &SlcConfig,
        ablation: Ablation,
    ) -> bool {
        let exits_zone = match self.kind {
            LevelKind::Demand => bar.close > self.high,
            LevelKind::Supply => bar.close < self.low,
        };
        if !self.available()
            || self.touch_at.is_none_or(|touch| {
                touch > bar.ts_event
                    || (bar.ts_event.as_u64() - touch.as_u64())
                        / super::data::MINUTE
                        / c.ltf_minutes
                        >= c.confirmation_window_bars as u64
            })
            || self.confirmation_age > c.confirmation_window_bars
            || !exits_zone
        {
            return false;
        }
        if ablation == Ablation::WithoutStochastic {
            return true;
        }
        match c.confirmation_mode {
            ConfirmationMode::PriceResponse => match self.kind {
                LevelKind::Demand => bar.close > self.touch_high && bar.close > bar.open,
                LevelKind::Supply => bar.close < self.touch_low && bar.close < bar.open,
            },
            ConfirmationMode::StochasticReentry => self.stochastic_confirmed(bar, k, previous_k, c),
            ConfirmationMode::WeightedEvidence | ConfirmationMode::Cisd => true,
        }
    }

    fn cisd_confirmed(&self, bar: Bar, f: &SymbolFeatures) -> bool {
        f.delivery.event.is_some_and(|(at, direction, price)| {
            self.touch_at.is_some_and(|touch| touch <= at)
                && at <= bar.ts_event
                && match self.kind {
                    LevelKind::Demand => direction == Structure::Bullish && bar.close > price,
                    LevelKind::Supply => direction == Structure::Bearish && bar.close < price,
                }
        })
    }

    fn stochastic_confirmed(
        &self,
        bar: Bar,
        k: f64,
        previous_k: Option<f64>,
        c: &SlcConfig,
    ) -> bool {
        let reentry_now = previous_k.is_some_and(|previous| match self.kind {
            LevelKind::Demand => previous < c.oversold && k >= c.oversold,
            LevelKind::Supply => previous > 100.0 - c.oversold && k <= 100.0 - c.oversold,
        });
        let outside_extreme = match self.kind {
            LevelKind::Demand => k >= c.oversold,
            LevelKind::Supply => k <= 100.0 - c.oversold,
        };
        // Retain the completed reentry while price and volume confirm within the touch window
        self.armed
            && outside_extreme
            && (reentry_now || self.reentry_at.is_some_and(|at| at <= bar.ts_event))
    }
}

#[derive(Debug, Default)]
pub(super) struct KeyLevelDetector {
    pub(super) levels: VecDeque<Level>,
    previous: Option<Bar>,
    last_level_failure: Option<NoTrade>,
    pub(super) state: SetupState,
}

impl KeyLevelDetector {
    pub(super) fn update(
        &mut self,
        bar: Bar,
        features: &SymbolFeatures,
        c: &SlcConfig,
        ablation: Ablation,
    ) {
        self.last_level_failure = None;
        let k = features
            .stochastic
            .initialized
            .then_some(features.stochastic.value_k);
        for level in &mut self.levels {
            level.observe(bar, c, k, features.atr.value);
            if level.tests > c.max_level_tests {
                self.last_level_failure = Some(NoTrade::LevelOvertested);
            }
        }
        self.levels.retain(|l| !l.consumed);
        if let Some(base) = self.previous
            && features.atr.initialized
        {
            let impulse = (bar.close.as_f64() - bar.open.as_f64()).abs() / features.atr.value;
            let volume = features.relative_ltf_volume(bar).unwrap_or(0.0);
            if impulse >= c.impulse_atr
                && (ablation == Ablation::SlcOnly || volume >= c.impulse_volume)
            {
                let kind = if bar.close > base.high && bar.close > bar.open {
                    Some(LevelKind::Demand)
                } else if bar.close < base.low && bar.close < bar.open {
                    Some(LevelKind::Supply)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    self.levels.push_back(Level {
                        kind,
                        low: base.low,
                        high: base.high,
                        created: bar.ts_event,
                        age: 0,
                        tests: 0,
                        breaks: 0,
                        impulse,
                        volume,
                        inside: false,
                        armed: false,
                        reentry_at: None,
                        touch_at: None,
                        touch_high: base.high,
                        touch_low: base.low,
                        phase: LevelPhase::Active,
                        was_adverse: false,
                        reclaimed_at: None,
                        confirmation_age: 0,
                        price_volume: bar.close.as_f64() * bar.volume.as_f64(),
                        total_volume: bar.volume.as_f64(),
                        consumed: false,
                    });
                }
            }
        }
        self.previous = Some(bar);
    }

    pub(super) fn evaluate(
        &mut self,
        bar: Bar,
        f: &SymbolFeatures,
        rank: &MomentumRank,
        regime: MarketRegime,
        c: &SlcMomentumConfig,
        available: UnixNanos,
    ) -> Result<SlcSignal, NoTrade> {
        self.state = SetupState::Candidate;
        if c.market.is_some() && !f.complete_at(bar.ts_event) {
            return Err(NoTrade::DataMissing);
        }
        let side = if c.ablation.momentum() {
            c.directions
                .iter()
                .copied()
                .max_by(|a, b| {
                    a.strength(rank.percentile)
                        .total_cmp(&b.strength(rank.percentile))
                })
                .ok_or(NoTrade::DirectionDisabled)?
        } else {
            match f.structure.structure {
                Structure::Bearish => TradeSide::Short,
                _ => TradeSide::Long,
            }
        };
        if !c.directions.contains(&side) {
            return Err(if side == TradeSide::Short {
                NoTrade::ShortDisabled
            } else {
                NoTrade::DirectionDisabled
            });
        }
        let momentum = side.strength(rank.percentile) >= c.momentum.min_percentile;
        if c.ablation.momentum() && !momentum {
            return Err(NoTrade::MomentumWeak);
        }
        let score_momentum = momentum || !c.ablation.momentum();
        if c.ablation.regime() {
            match (side, regime) {
                (TradeSide::Long, MarketRegime::Bearish) => return Err(NoTrade::MarketBearish),
                (TradeSide::Short, MarketRegime::Bullish) => return Err(NoTrade::MarketBullish),
                _ => {}
            }
        }
        if c.slc.require_intraday_trend && c.ablation != Ablation::SlcOnly {
            let open = f.open.ok_or(NoTrade::DataMissing)?.as_f64();
            let change = bar.close.as_f64() / open - 1.0;
            if !f.ema_slow.initialized()
                || !side.aligned(change)
                || change.abs() < c.slc.minimum_intraday_return
                || !side.aligned(f.ema_fast.value() - f.ema_slow.value())
                || !side.aligned(bar.close.as_f64() - f.vwap.value)
            {
                return Err(NoTrade::IntradayTrendMismatch);
            }
        }
        let aligned = f.structure.structure == side.structure();
        let kind = if side == TradeSide::Long {
            LevelKind::Demand
        } else {
            LevelKind::Supply
        };
        if let Some(session) = f.session {
            let interval = c.slc.htf_minutes * super::data::MINUTE;
            let expected = session.open.as_u64()
                + (bar.ts_event.as_u64() - session.open.as_u64()) / interval * interval;
            if expected > session.open.as_u64()
                && f.last_htf.is_none_or(|ts| ts.as_u64() < expected)
            {
                return Err(NoTrade::DataMissing);
            }
        }
        let weighted_evidence = c.slc.confirmation_mode == ConfirmationMode::WeightedEvidence;
        // 加权模式允许 RANGE 结构贡献 0 分后由其他证据补足；明确的反向结构仍然拒绝。
        if c.ablation.slc()
            && !aligned
            && !(weighted_evidence && f.structure.structure == Structure::Range)
        {
            return Err(NoTrade::HtfRange);
        }
        let needs_stochastic = c.ablation.slc()
            && c.ablation != Ablation::WithoutStochastic
            && c.slc.confirmation_mode == ConfirmationMode::StochasticReentry;
        if !f.atr.initialized || (needs_stochastic && !f.stochastic.initialized) {
            return Err(NoTrade::Warmup);
        }
        let exhaustion = c.slc.oversold / 2.0;
        // 随机指标不再是硬串联条件，但追涨杀跌的极端耗竭区仍是硬否决。
        if weighted_evidence
            && f.stochastic.initialized
            && match side {
                TradeSide::Long => f.stochastic.value_k >= 100.0 - exhaustion,
                TradeSide::Short => f.stochastic.value_k <= exhaustion,
            }
        {
            return Err(NoTrade::ConfirmationMissing);
        }
        self.state = SetupState::HtfValid;
        let rvol = match f.relative_ltf_volume(bar) {
            Some(value) => value,
            None if c.ablation == Ablation::SlcOnly => 0.0,
            None => return Err(NoTrade::Warmup),
        };
        let atr = f.atr.value;
        let eligible = self
            .levels
            .iter()
            .enumerate()
            .filter(|(_, l)| l.kind == kind && l.available() && l.created < bar.ts_event)
            .filter(|(_, l)| {
                weighted_evidence
                    || l.score(&c.slc, aligned, score_momentum, bar.close, atr)
                        >= c.slc.min_level_score
            })
            .filter(|(_, l)| {
                (if side == TradeSide::Long {
                    bar.close.as_f64() - l.high.as_f64()
                } else {
                    l.low.as_f64() - bar.close.as_f64()
                })
                .max(0.0)
                    / atr
                    <= c.slc.max_level_distance_atr
            });
        let has_level = eligible.clone().next().is_some();
        let chosen = eligible
            .filter(|(_, l)| {
                l.confirmed(bar, f.stochastic.value_k, f.previous_k, &c.slc, c.ablation)
                    && (c.slc.confirmation_mode != ConfirmationMode::Cisd
                        || l.cisd_confirmed(bar, f))
            })
            .max_by(|(_, a), (_, b)| {
                a.score(&c.slc, aligned, score_momentum, bar.close, atr)
                    .total_cmp(&b.score(&c.slc, aligned, score_momentum, bar.close, atr))
            });
        let (index, level) = if c.ablation.slc() {
            chosen.ok_or_else(|| {
                if has_level {
                    self.state = SetupState::WaitingConfirmation;
                    NoTrade::ConfirmationMissing
                } else {
                    self.last_level_failure.unwrap_or(NoTrade::LevelInvalid)
                }
            })?
        } else {
            // Momentum-only enters once after a positive completed candle; common sizing still applies
            if !side.aligned(bar.close.as_f64() - bar.open.as_f64()) {
                return Err(NoTrade::ConfirmationMissing);
            }
            let synthetic = Level {
                kind,
                low: bar.low,
                high: bar.high,
                created: bar.ts_event,
                age: 0,
                tests: 0,
                breaks: 0,
                impulse: 0.0,
                volume: 0.0,
                inside: false,
                armed: false,
                reentry_at: None,
                touch_at: None,
                touch_high: bar.high,
                touch_low: bar.low,
                phase: LevelPhase::Active,
                was_adverse: false,
                reclaimed_at: None,
                confirmation_age: 0,
                price_volume: 0.0,
                total_volume: 0.0,
                consumed: false,
            };
            return Self::make_signal(
                bar, f, rank, regime, c, available, &synthetic, 0.0, 0.0, 0, 0, rvol,
            );
        };
        self.state = SetupState::LevelFound;
        let level = level.clone();
        let level_score = level.score(&c.slc, aligned, score_momentum, bar.close, atr);
        let stochastic_confirmed =
            level.stochastic_confirmed(bar, f.stochastic.value_k, f.previous_k, &c.slc);
        self.state = SetupState::WaitingConfirmation;
        if c.ablation.vwap() && !side.aligned(bar.close.as_f64() - f.vwap.value) {
            return Err(NoTrade::ConfirmationMissing);
        }
        if c.ablation.volume() && !weighted_evidence && rvol < c.slc.minimum_confirmation_volume {
            return Err(NoTrade::RvolLow);
        }
        let flags = [
            // 顺序必须与 SlcConfig::confirmation_weights 的公开配置契约保持一致。
            aligned,
            momentum,
            level_score >= c.slc.min_level_score,
            stochastic_confirmed,
            side.aligned(bar.close.as_f64() - f.vwap.value),
            rvol >= c.slc.minimum_confirmation_volume,
            side.aligned(bar.close.as_f64() - bar.open.as_f64()),
            side.aligned(bar.close.as_f64() - f.ema_fast.value()),
        ];
        let enabled = [
            true,
            c.ablation.momentum(),
            true,
            c.ablation != Ablation::WithoutStochastic
                && matches!(
                    c.slc.confirmation_mode,
                    ConfirmationMode::StochasticReentry | ConfirmationMode::WeightedEvidence
                ),
            c.ablation.vwap(),
            c.ablation.volume(),
            true,
            true,
        ];
        let score = flags
            .iter()
            .zip(enabled)
            .zip(c.slc.confirmation_weights)
            .filter(|((flag, enabled), _)| **flag && *enabled)
            .map(|(_, w)| w)
            .sum::<f64>();
        let removed = enabled
            .iter()
            .zip(c.slc.confirmation_weights)
            .filter(|(enabled, _)| !**enabled)
            .map(|(_, w)| w)
            .sum::<f64>();
        let threshold = (c.slc.confirmation_threshold - removed).max(0.0)
            + if regime == MarketRegime::Neutral && c.ablation.regime() {
                c.slc.neutral_extra_score
            } else {
                0.0
            };
        if score < threshold {
            return Err(NoTrade::ConfirmationMissing);
        }
        let signal = Self::make_signal(
            bar,
            f,
            rank,
            regime,
            c,
            available,
            &level,
            score,
            level_score,
            confirmation_bitmap(flags),
            confirmation_bitmap(enabled),
            rvol,
        )?;
        if let Some(level) = self.levels.get_mut(index) {
            level.consumed = true;
        }
        self.state = SetupState::SignalReady;
        Ok(signal)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "immutable signal construction gathers model evidence"
    )]
    fn make_signal(
        bar: Bar,
        f: &SymbolFeatures,
        rank: &MomentumRank,
        regime: MarketRegime,
        c: &SlcMomentumConfig,
        available: UnixNanos,
        level: &Level,
        score: f64,
        level_score: f64,
        confirmation_flags: u8,
        confirmation_enabled: u8,
        rvol: f64,
    ) -> Result<SlcSignal, NoTrade> {
        let symbol = bar.bar_type.instrument_id();
        let sector = c
            .metadata(symbol, bar.ts_event)
            .ok_or(NoTrade::MetadataMissing)?
            .sector
            .clone();
        Ok(SlcSignal {
            symbol,
            timestamp: bar.ts_event,
            available_at: available,
            side: if level.kind == LevelKind::Demand {
                TradeSide::Long
            } else {
                TradeSide::Short
            },
            intraday_return: f
                .open
                .map_or(0.0, |p| bar.close.as_f64() / p.as_f64() - 1.0),
            sector,
            momentum: rank.clone(),
            market_regime: regime,
            structure: f.structure.structure,
            structure_score: if f.structure.structure == Structure::Range {
                0.0
            } else {
                2.0
            },
            structure_confirmed_at: f.structure.confirmed_at,
            cisd_confirmed_at: f.delivery.event.map(|e| e.0),
            cisd_level: f.delivery.event.map(|e| e.2),
            level_type: if c.ablation.slc() {
                if level.kind == LevelKind::Demand {
                    "DEMAND"
                } else {
                    "SUPPLY"
                }
            } else {
                if level.kind == LevelKind::Demand {
                    "MOMENTUM_BAR_LOW"
                } else {
                    "MOMENTUM_BAR_HIGH"
                }
            }
            .to_string(),
            level_low: level.low,
            level_high: level.high,
            level_created: level.created,
            level_tests: level.tests,
            level_breaks: level.breaks,
            level_reclaimed_at: level.reclaimed_at,
            level_score,
            confirmation_score: score,
            confirmation_flags,
            confirmation_enabled,
            stochastic_k: f.stochastic.value_k,
            stochastic_d: f.stochastic.value_d,
            vwap: f.vwap.value,
            anchored_vwap: if level.total_volume > 0.0 {
                level.price_volume / level.total_volume
            } else {
                f.vwap.value
            },
            relative_volume: rvol,
            entry_price: bar.close,
            atr: Decimal::from_f64(f.atr.value).ok_or(NoTrade::VolatilityInvalid)?,
            setup_type: if c.ablation.slc() && level.breaks == 1 {
                "MOMENTUM_SLC_BREAK_RETEST"
            } else if c.ablation.slc() {
                "MOMENTUM_SLC_PULLBACK"
            } else {
                "MOMENTUM_ONLY"
            }
            .to_string(),
        })
    }
}
