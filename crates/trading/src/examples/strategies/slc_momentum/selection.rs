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

//! 横截面动量特征、百分位排名与相关性计算。
//!
//! 所有股票使用同一事件时间和前一交易日截止日线；快照发布后保持不可变，缺失覆盖率时
//! 返回空候选，而不是用幸存的少量股票生成有偏排名。

use std::collections::{BTreeMap, VecDeque};

use nautilus_core::UnixNanos;
use nautilus_model::{data::Bar, enums::BarAggregation, identifiers::InstrumentId};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use super::{
    SlcMomentumConfig,
    data::{MINUTE, SymbolFeatures, validate_bar},
    signal::NoTrade,
};

/// Immutable ranking evidence computed at one common event-time cutoff.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct MomentumRank {
    pub symbol: InstrumentId,
    pub timestamp: UnixNanos,
    pub daily_cutoff: UnixNanos,
    pub rank: usize,
    pub percentile: f64,
    pub score: f64,
    pub returns: Vec<f64>,
    pub relative_strength: [f64; 3],
    pub relative_volume: f64,
    pub average_dollar_volume: Decimal,
    pub average_volume: Decimal,
    pub atr_fraction: f64,
    pub gap: f64,
}

impl MomentumRank {
    pub(super) fn unranked(symbol: InstrumentId, timestamp: UnixNanos) -> Self {
        Self {
            symbol,
            timestamp,
            daily_cutoff: UnixNanos::default(),
            rank: 0,
            percentile: 50.0,
            score: 0.0,
            returns: Vec::new(),
            relative_strength: [0.0; 3],
            relative_volume: 0.0,
            average_dollar_volume: Decimal::ZERO,
            average_volume: Decimal::ZERO,
            atr_fraction: 0.0,
            gap: 0.0,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct RankSnapshot {
    pub timestamp: UnixNanos,
    pub ranks: BTreeMap<InstrumentId, MomentumRank>,
    pub excluded: BTreeMap<InstrumentId, NoTrade>,
}

#[derive(Debug, Default)]
pub struct CrossSectionalMomentumRanker {
    pub(super) daily: BTreeMap<InstrumentId, VecDeque<Bar>>,
    pub(super) snapshot: RankSnapshot,
    last_refresh: Option<UnixNanos>,
    last_session: Option<UnixNanos>,
}

impl CrossSectionalMomentumRanker {
    /// Accepts a final, close-stamped daily bar without revising past observations.
    ///
    /// # Errors
    /// Returns an error for unavailable, malformed, conflicting or out-of-order bars.
    pub fn update_daily(&mut self, bar: Bar, now: UnixNanos) -> anyhow::Result<()> {
        validate_bar(&bar, now)?;
        anyhow::ensure!(
            bar.bar_type.spec().aggregation == BarAggregation::Day
                && bar.bar_type.spec().step.get() == 1,
            "ranking requires daily bars"
        );
        let history = self.daily.entry(bar.bar_type.instrument_id()).or_default();
        if let Some(last) = history.back() {
            if *last == bar {
                return Ok(());
            }
            anyhow::ensure!(
                bar.ts_event > last.ts_event,
                "daily bars cannot amend past ranking inputs"
            );
        }
        history.push_back(bar);
        if history.len() > 300 {
            history.pop_front();
        }
        Ok(())
    }

    /// Returns the most recently frozen cross-sectional snapshot.
    #[must_use]
    pub fn snapshot(&self) -> &RankSnapshot {
        &self.snapshot
    }

    pub(super) fn refresh(
        &mut self,
        ts: UnixNanos,
        config: &SlcMomentumConfig,
        features: &BTreeMap<InstrumentId, SymbolFeatures>,
    ) {
        let Some(session) = config.session(ts) else {
            return;
        };
        let m = &config.momentum;
        if self.last_session == Some(session.open)
            && (m.daily_only
                || self
                    .last_refresh
                    .is_some_and(|last| ts.as_u64() < last.as_u64() + m.refresh_minutes * MINUTE))
        {
            return;
        }
        self.last_session = Some(session.open);
        self.last_refresh = Some(ts);
        let mut snapshot = RankSnapshot {
            timestamp: ts,
            ..RankSnapshot::default()
        };
        let metadata = config
            .universe
            .iter()
            .filter(|m| m.known_at <= ts && m.effective_from <= ts && ts < m.effective_until)
            .collect::<Vec<_>>();
        let expected_count = metadata.len();
        let observed = metadata
            .iter()
            .filter(|m| {
                features
                    .get(&m.instrument_id)
                    .is_some_and(|f| f.last.is_some_and(|b| b.ts_event == ts))
            })
            .count();
        let mut rows: Vec<(InstrumentId, Vec<f64>)> = Vec::new();
        for meta in metadata {
            let id = meta.instrument_id;
            let result = self.features(meta, ts, session.open, config, features, None);
            match result {
                Ok(rank) => {
                    let mut values = rank.returns.clone();
                    values.extend(rank.relative_strength);
                    values.push(rank.relative_volume);
                    let intraday_return = if m.daily_only {
                        0.0
                    } else {
                        features
                            .get(&id)
                            .and_then(|f| f.last.zip(f.open))
                            .map_or(0.0, |(b, p)| b.close.as_f64() / p.as_f64() - 1.0)
                    };
                    values.push(intraday_return);
                    rows.push((id, values));
                    snapshot.ranks.insert(id, rank);
                }
                Err(reason) => {
                    snapshot.excluded.insert(id, reason);
                }
            }
        }
        if rows.len() < m.minimum_universe_size
            || expected_count == 0
            || observed as f64 / (expected_count as f64) < m.minimum_coverage
        {
            for id in snapshot.ranks.keys() {
                snapshot.excluded.insert(*id, NoTrade::DataMissing);
            }
            snapshot.ranks.clear();
            self.snapshot = snapshot;
            return;
        }
        Self::score(&mut snapshot, &rows, config);
        self.snapshot = snapshot;
    }

    fn score(
        snapshot: &mut RankSnapshot,
        rows: &[(InstrumentId, Vec<f64>)],
        c: &SlcMomentumConfig,
    ) {
        let m = &c.momentum;
        let mut weights = m.return_weights.clone();
        weights.extend([
            m.spy_weight,
            m.qqq_weight,
            m.sector_weight,
            m.relative_volume_weight,
            if m.daily_only { 0.0 } else { m.intraday_weight },
        ]);
        let weight_sum = weights.iter().sum::<f64>();
        if weight_sum <= 0.0 {
            return;
        }
        for (column, weight) in weights.iter().enumerate().filter(|(_, w)| **w > 0.0) {
            let values = rows
                .iter()
                .map(|(id, v)| (*id, v[column]))
                .collect::<Vec<_>>();
            for (id, percentile) in percentiles(&values) {
                if let Some(rank) = snapshot.ranks.get_mut(&id) {
                    rank.score += weight * percentile / weight_sum;
                }
            }
        }
        // A weighted mean of percentiles is bounded; summing f64 weights can
        // otherwise produce 100.00000000000001 and invalidate a legitimate snapshot.
        for rank in snapshot.ranks.values_mut() {
            rank.score = rank.score.clamp(0.0, 100.0);
        }
        let scores = snapshot
            .ranks
            .iter()
            .map(|(id, r)| (*id, r.score))
            .collect::<Vec<_>>();
        let percentiles = percentiles(&scores);
        let mut ordered = scores;
        ordered.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        for (index, (id, _)) in ordered.into_iter().enumerate() {
            if let Some(rank) = snapshot.ranks.get_mut(&id) {
                rank.rank = index + 1;
                rank.percentile = percentiles[&id];
            }
        }
    }

    /// Ranks the complete point-in-time universe using observed quotes and prior final daily bars.
    /// No symbol is ranked against only the subscribed candidate pool.
    ///
    /// # Errors
    /// Returns an error for duplicate observations, missing metadata or an invalid publication.
    pub fn rank_market(
        &mut self,
        timestamp: UnixNanos,
        session_open: UnixNanos,
        c: &SlcMomentumConfig,
        observations: &[super::MarketObservation],
    ) -> anyhow::Result<super::UniverseSelection> {
        let market = c
            .market
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("market config required"))?;
        let session = c
            .sessions
            .iter()
            .find(|s| s.open == session_open)
            .ok_or_else(|| anyhow::anyhow!("unknown ranking session"))?;
        anyhow::ensure!(
            session.open <= timestamp && timestamp < session.close,
            "ranking outside regular session"
        );
        let by_symbol = observations
            .iter()
            .map(|o| (o.symbol, o))
            .collect::<BTreeMap<_, _>>();
        anyhow::ensure!(
            by_symbol.len() == observations.len(),
            "duplicate market observation"
        );
        let mut snapshot = RankSnapshot {
            timestamp,
            ..RankSnapshot::default()
        };
        let mut rows = Vec::new();
        let mut observed = 0;
        let empty = BTreeMap::new();
        for meta in c.universe.iter().filter(|m| {
            m.known_at <= timestamp
                && m.effective_from <= timestamp
                && timestamp < m.effective_until
        }) {
            let id = meta.instrument_id;
            let observation = by_symbol.get(&id).copied();
            let result = observation
                .ok_or(NoTrade::DataMissing)
                .and_then(|o| self.features(meta, timestamp, session.open, c, &empty, Some(o)));
            if !matches!(result, Err(NoTrade::DataMissing | NoTrade::Warmup)) {
                observed += 1;
            }
            match result {
                Ok(rank) => {
                    let mut values = rank.returns.clone();
                    values.extend(rank.relative_strength);
                    values.push(rank.relative_volume);
                    values
                        .push(observation.map_or(0.0, |o| o.last.as_f64() / o.open.as_f64() - 1.0));
                    rows.push((id, values));
                    snapshot.ranks.insert(id, rank);
                }
                Err(reason) => {
                    snapshot.excluded.insert(id, reason);
                }
            }
        }
        if rows.len() < c.momentum.minimum_universe_size
            || observed as f64 / (market.universe_size as f64) < c.momentum.minimum_coverage
        {
            for id in snapshot.ranks.keys() {
                snapshot.excluded.insert(*id, NoTrade::DataMissing);
            }
            snapshot.ranks.clear();
        } else {
            Self::score(&mut snapshot, &rows, c);
        }
        let selection = super::UniverseSelection {
            session_open,
            available_at: timestamp,
            valid_until: UnixNanos::from(
                (timestamp.as_u64() + market.max_snapshot_age_minutes * MINUTE)
                    .min(session.close.as_u64()),
            ),
            universe_size: market.universe_size,
            observed_size: observed,
            candidates: snapshot
                .ranks
                .iter()
                .filter(|(_, r)| c.candidate(r.percentile))
                .map(|(id, _)| *id)
                .collect(),
            ranking: snapshot,
        };
        selection.validate(c, timestamp)?;
        self.snapshot = selection.ranking.clone();
        Ok(selection)
    }

    fn features(
        &self,
        meta: &super::SymbolMetadata,
        ts: UnixNanos,
        open: UnixNanos,
        c: &SlcMomentumConfig,
        intraday: &BTreeMap<InstrumentId, SymbolFeatures>,
        observation: Option<&super::MarketObservation>,
    ) -> Result<MomentumRank, NoTrade> {
        let previous = c.sessions.iter().rfind(|s| s.close < open).map(|s| s.close);
        let id = meta.instrument_id;
        let m = &c.momentum;
        let history = self.history_at(id, open, ts);
        let maximum = *m.lookbacks.iter().max().ok_or(NoTrade::DataMissing)?;
        let needed = maximum
            .max(m.relative_strength_lookback)
            .max(c.risk.correlation_lookback)
            .max(20)
            + 1;
        if history.len() < needed {
            return Err(NoTrade::Warmup);
        }
        let last = history.last().ok_or(NoTrade::DataMissing)?;
        if Some(last.ts_event) != previous {
            return Err(NoTrade::DataMissing);
        }
        let expected = c
            .sessions
            .iter()
            .filter(|s| s.close < open)
            .rev()
            .take(needed)
            .map(|s| s.close)
            .collect::<Vec<_>>();
        if expected.len() != needed
            || history
                .iter()
                .rev()
                .take(needed)
                .map(|b| b.ts_event)
                .ne(expected)
        {
            return Err(NoTrade::DataMissing);
        }
        let state = intraday.get(&id);
        let (current_price, open_price) = if let Some(o) = observation {
            if o.symbol != id
                || o.timestamp > ts
                || o.available_at > ts
                || o.timestamp < open
                || ts.as_u64() - o.timestamp.as_u64()
                    > c.market
                        .as_ref()
                        .map_or(c.max_quote_age_ms, |m| m.max_observation_age_ms)
                        * 1_000_000
                || o.last.as_decimal() <= Decimal::ZERO
                || o.open.as_decimal() <= Decimal::ZERO
                || !o.relative_volume.is_finite()
                || o.relative_volume < 0.0
            {
                return Err(NoTrade::DataMissing);
            }
            (o.last, o.open)
        } else {
            let state = state.ok_or(NoTrade::DataMissing)?;
            if state.observed_minutes != (ts.as_u64() - open.as_u64()) / MINUTE {
                return Err(NoTrade::DataMissing);
            }
            (
                state
                    .last
                    .filter(|b| b.ts_event == ts)
                    .ok_or(NoTrade::DataMissing)?
                    .close,
                state.open.ok_or(NoTrade::DataMissing)?,
            )
        };
        let adv = history
            .iter()
            .rev()
            .take(20)
            .map(|b| b.close.as_decimal() * b.volume.as_decimal())
            .sum::<Decimal>()
            / Decimal::from(20);
        let volume = history
            .iter()
            .rev()
            .take(20)
            .map(|b| b.volume.as_decimal())
            .sum::<Decimal>()
            / Decimal::from(20);
        if current_price.as_decimal() < m.minimum_price
            || adv < m.minimum_average_dollar_volume
            || meta.market_cap < m.minimum_market_cap
            || volume <= Decimal::ZERO
        {
            return Err(NoTrade::LiquidityTooLow);
        }
        let atr = history
            .windows(2)
            .rev()
            .take(20)
            .map(|w| {
                (w[1].high.as_f64() - w[1].low.as_f64())
                    .max((w[1].high.as_f64() - w[0].close.as_f64()).abs())
                    .max((w[1].low.as_f64() - w[0].close.as_f64()).abs())
            })
            .sum::<f64>()
            / 20.0;
        let atr_fraction = atr / last.close.as_f64();
        let gap = open_price.as_f64() / last.close.as_f64() - 1.0;
        let relative_volume = if m.daily_only {
            last.volume.as_f64() / volume.to_f64().ok_or(NoTrade::DataMissing)?
        } else if let Some(o) = observation {
            o.relative_volume
        } else {
            let state = state.ok_or(NoTrade::DataMissing)?;
            let minute =
                usize::try_from(state.observed_minutes).map_err(|_| NoTrade::DataMissing)?;
            let samples = state
                .volume_profiles
                .iter()
                .filter_map(|p| p.get(minute.saturating_sub(1)))
                .copied()
                .collect::<Vec<_>>();
            if samples.len() < 5 {
                return Err(NoTrade::Warmup);
            }
            let expected = samples.iter().sum::<Decimal>() / Decimal::from(samples.len());
            if expected <= Decimal::ZERO {
                return Err(NoTrade::LiquidityTooLow);
            }
            (state.volume / expected)
                .to_f64()
                .ok_or(NoTrade::DataMissing)?
        };
        if atr_fraction < m.minimum_atr_fraction
            || atr_fraction > m.maximum_atr_fraction
            || gap.abs() > m.maximum_gap_fraction
        {
            return Err(NoTrade::VolatilityInvalid);
        }
        if relative_volume < m.minimum_relative_volume {
            return Err(NoTrade::RvolLow);
        }
        let returns = m
            .lookbacks
            .iter()
            .map(|n| last.close.as_f64() / history[history.len() - 1 - n].close.as_f64() - 1.0)
            .collect();
        let own = last.close.as_f64()
            / history[history.len() - 1 - m.relative_strength_lookback]
                .close
                .as_f64()
            - 1.0;
        let mut relative_strength = [0.0; 3];
        for (index, benchmark) in [c.benchmarks[0], c.benchmarks[1], meta.sector_etf]
            .iter()
            .enumerate()
        {
            let benchmark = self.history_at(*benchmark, open, ts);
            let n = m.relative_strength_lookback;
            if benchmark.len() <= n
                || benchmark.last().map(|b| b.ts_event) != previous
                || benchmark[benchmark.len() - 1 - n].ts_event
                    != history[history.len() - 1 - n].ts_event
            {
                return Err(NoTrade::DataMissing);
            }
            relative_strength[index] = own
                - (benchmark[benchmark.len() - 1].close.as_f64()
                    / benchmark[benchmark.len() - 1 - n].close.as_f64()
                    - 1.0);
        }
        Ok(MomentumRank {
            symbol: id,
            timestamp: ts,
            daily_cutoff: last.ts_event,
            rank: 0,
            percentile: 0.0,
            score: 0.0,
            returns,
            relative_strength,
            relative_volume,
            average_dollar_volume: adv,
            average_volume: volume,
            atr_fraction,
            gap,
        })
    }

    pub(super) fn history_at(
        &self,
        id: InstrumentId,
        before: UnixNanos,
        known: UnixNanos,
    ) -> Vec<Bar> {
        self.daily.get(&id).map_or_else(Vec::new, |h| {
            h.iter()
                .filter(|b| b.ts_event < before && b.ts_init <= known)
                .copied()
                .collect()
        })
    }

    pub(super) fn correlation(
        &self,
        a: InstrumentId,
        b: InstrumentId,
        before: UnixNanos,
        lookback: usize,
    ) -> Option<f64> {
        let a = self.history_at(a, before, before);
        let b = self.history_at(b, before, before);
        if a.len() <= lookback || b.len() <= lookback {
            return None;
        }
        let a = &a[a.len() - lookback - 1..];
        let b = &b[b.len() - lookback - 1..];
        if a.iter().zip(b).any(|(a, b)| a.ts_event != b.ts_event) {
            return None;
        }
        let x = a
            .windows(2)
            .map(|w| w[1].close.as_f64() / w[0].close.as_f64() - 1.0)
            .collect::<Vec<_>>();
        let y = b
            .windows(2)
            .map(|w| w[1].close.as_f64() / w[0].close.as_f64() - 1.0)
            .collect::<Vec<_>>();
        correlation(&x, &y)
    }
}

pub(super) fn percentiles(values: &[(InstrumentId, f64)]) -> BTreeMap<InstrumentId, f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    let mut result = BTreeMap::new();
    let mut start = 0;
    while start < sorted.len() {
        let mut end = start + 1;
        while end < sorted.len()
            && sorted[end].1.partial_cmp(&sorted[start].1) == Some(std::cmp::Ordering::Equal)
        {
            end += 1;
        }
        let percentile = if sorted.len() <= 1 {
            50.0
        } else {
            (start + end - 1) as f64 * 50.0 / (sorted.len() - 1) as f64
        };
        for (id, _) in &sorted[start..end] {
            result.insert(*id, percentile);
        }
        start = end;
    }
    result
}

pub(super) fn correlation(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() != y.len() || x.len() < 2 {
        return None;
    }
    let mx = x.iter().sum::<f64>() / x.len() as f64;
    let my = y.iter().sum::<f64>() / y.len() as f64;
    let covariance = x
        .iter()
        .zip(y)
        .map(|(x, y)| (x - mx) * (y - my))
        .sum::<f64>();
    let variance = (x.iter().map(|x| (x - mx).powi(2)).sum::<f64>()
        * y.iter().map(|y| (y - my).powi(2)).sum::<f64>())
    .sqrt();
    (variance > 0.0).then(|| (covariance / variance).clamp(-1.0, 1.0))
}
