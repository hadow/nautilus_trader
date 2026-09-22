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

//! 全市场快照及候选集合的数据契约。
//!
//! 每次发布都必须覆盖当时有效的完整股票池，并证明排名时间、发布时间、有效期和候选成员
//! 相互一致；订阅数量不会反过来缩小排名母体。

use std::{any::Any, collections::BTreeSet, sync::Arc};

use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, CustomData, CustomDataTrait, Data, DataType, HasTsInit},
    identifiers::InstrumentId,
    types::Price,
};
use serde::{Deserialize, Serialize};

use super::{RankSnapshot, SlcMomentumConfig, data::MINUTE};

/// Full-universe selection contract, independent of broker subscriptions.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketSelectionConfig {
    pub universe_size: usize,
    pub top_fraction: f64,
    pub max_candidates: usize,
    pub max_snapshot_age_minutes: u64,
    pub max_observation_age_ms: u64,
    pub max_bar_delay_seconds: u64,
}

impl Default for MarketSelectionConfig {
    fn default() -> Self {
        Self {
            universe_size: 3_000,
            top_fraction: 0.10,
            max_candidates: 300,
            max_snapshot_age_minutes: 20,
            max_observation_age_ms: 60_000,
            max_bar_delay_seconds: 120,
        }
    }
}

impl MarketSelectionConfig {
    pub(super) fn validate(&self, c: &SlcMomentumConfig) -> anyhow::Result<()> {
        anyhow::ensure!(
            (2..=20_000).contains(&self.universe_size)
                && self.top_fraction.is_finite()
                && (0.05..=0.10).contains(&self.top_fraction)
                && self.max_candidates > 0
                && self.max_candidates <= 900
                && (1..=1_440).contains(&self.max_snapshot_age_minutes)
                && (1..=300_000).contains(&self.max_observation_age_ms)
                && (1..=300).contains(&self.max_bar_delay_seconds),
            "invalid full-market universe, top fraction, candidate capacity or snapshot expiry"
        );
        anyhow::ensure!(
            (c.momentum.min_percentile - (1.0 - self.top_fraction) * 100.0).abs() < 1e-8,
            "momentum min_percentile must match the full-market top_fraction"
        );
        anyhow::ensure!(
            !c.momentum.daily_only || self.max_snapshot_age_minutes >= 390,
            "daily-only market ranking requires a session-long snapshot lifetime"
        );
        let ids = c
            .universe
            .iter()
            .map(|m| m.instrument_id)
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            ids.len() >= self.universe_size,
            "security master cannot cover the configured full-market universe"
        );
        anyhow::ensure!(
            c.slc.breadth_threshold.is_none(),
            "full-market breadth requires full-universe VWAP observations"
        );
        anyhow::ensure!(
            (self.universe_size as f64 * self.top_fraction).ceil() as usize * c.directions.len()
                <= self.max_candidates,
            "candidate capacity is below the configured global top fraction"
        );
        anyhow::ensure!(
            c.ablation.momentum(),
            "full-market execution requires global momentum selection"
        );
        Ok(())
    }
}

/// An observed regular-session quote snapshot; RVOL is supplied with its provenance by the collector.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MarketObservation {
    pub symbol: InstrumentId,
    pub timestamp: UnixNanos,
    pub available_at: UnixNanos,
    pub open: Price,
    pub last: Price,
    pub relative_volume: f64,
}

/// Published global ranking and its executable candidate membership.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UniverseSelection {
    pub session_open: UnixNanos,
    pub available_at: UnixNanos,
    pub valid_until: UnixNanos,
    pub universe_size: usize,
    pub observed_size: usize,
    pub ranking: RankSnapshot,
    pub candidates: BTreeSet<InstrumentId>,
}

impl UniverseSelection {
    /// Checks coverage, causality and exact membership against the full ranking.
    ///
    /// # Errors
    /// Returns an error for partial, future, malformed or incorrectly ranked selections.
    pub fn validate(&self, c: &SlcMomentumConfig, now: UnixNanos) -> anyhow::Result<()> {
        let market = c
            .market
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("full-market mode is disabled"))?;
        let session = c
            .sessions
            .iter()
            .find(|s| s.open == self.session_open)
            .ok_or_else(|| anyhow::anyhow!("selection session is absent from calendar"))?;
        anyhow::ensure!(
            session.open <= self.ranking.timestamp
                && self.ranking.timestamp <= self.available_at
                && self.available_at <= now
                && self.available_at < self.valid_until
                && self.valid_until <= session.close
                && self.valid_until.as_u64() - self.available_at.as_u64()
                    <= market.max_snapshot_age_minutes * MINUTE,
            "invalid selection publication, expiry or future timestamp"
        );
        let universe = c
            .universe
            .iter()
            .filter(|m| {
                m.known_at <= self.ranking.timestamp
                    && m.effective_from <= self.ranking.timestamp
                    && self.ranking.timestamp < m.effective_until
            })
            .map(|m| m.instrument_id)
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            self.universe_size == market.universe_size
                && universe.len() == self.universe_size
                && self.observed_size <= self.universe_size,
            "selection universe does not match point-in-time membership"
        );
        let covered = self
            .ranking
            .ranks
            .keys()
            .chain(self.ranking.excluded.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            covered == universe
                && self
                    .ranking
                    .ranks
                    .keys()
                    .all(|id| !self.ranking.excluded.contains_key(id)),
            "selection must account for every universe member exactly once"
        );
        let enough = self.observed_size as f64 / self.universe_size as f64
            >= c.momentum.minimum_coverage
            && self.ranking.ranks.len() >= c.momentum.minimum_universe_size;
        anyhow::ensure!(
            enough || (self.ranking.ranks.is_empty() && self.candidates.is_empty()),
            "insufficient global coverage cannot authorize candidates"
        );
        let expected = self
            .ranking
            .ranks
            .iter()
            .filter(|(_, r)| c.candidate(r.percentile))
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            self.candidates == expected && self.candidates.len() <= market.max_candidates,
            "candidate membership or capacity differs from the global percentile threshold"
        );
        for (id, r) in &self.ranking.ranks {
            anyhow::ensure!(
                *id == r.symbol
                    && r.timestamp == self.ranking.timestamp
                    && Some(r.daily_cutoff)
                        == c.sessions
                            .iter()
                            .rfind(|s| s.close < session.open)
                            .map(|s| s.close)
                    && r.daily_cutoff <= r.timestamp
                    && r.score.is_finite()
                    && r.percentile.is_finite()
                    && (0.0..=100.0).contains(&r.score)
                    && (0.0..=100.0).contains(&r.percentile)
                    && r.rank > 0
                    && r.rank <= self.ranking.ranks.len(),
                "invalid global rank evidence for {id}: score={} percentile={} rank={} cutoff={} timestamp={}",
                r.score,
                r.percentile,
                r.rank,
                r.daily_cutoff,
                r.timestamp
            );
        }
        let scores = self
            .ranking
            .ranks
            .iter()
            .map(|(id, r)| (*id, r.score))
            .collect::<Vec<_>>();
        let percentiles = super::selection::percentiles(&scores);
        let mut ordered = scores;
        ordered.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        anyhow::ensure!(
            ordered
                .iter()
                .enumerate()
                .all(|(i, (id, _))| self.ranking.ranks[id].rank == i + 1),
            "rank order differs from global scores"
        );
        anyhow::ensure!(
            self.ranking
                .ranks
                .iter()
                .all(|(id, r)| (r.percentile - percentiles[id]).abs() < 1e-8),
            "percentiles were not computed across the complete eligible universe"
        );
        Ok(())
    }

    pub(super) fn permits_side(
        &self,
        id: InstrumentId,
        at: UnixNanos,
        side: super::TradeSide,
        c: &SlcMomentumConfig,
    ) -> bool {
        self.permits(id, at)
            && c.directions.contains(&side)
            && self
                .ranking
                .ranks
                .get(&id)
                .is_some_and(|r| side.strength(r.percentile) >= c.momentum.min_percentile)
    }

    #[must_use]
    pub fn permits(&self, id: InstrumentId, at: UnixNanos) -> bool {
        self.available_at <= at && at < self.valid_until && self.candidates.contains(&id)
    }
}

/// One atomic collector publication: global selection plus completed bars and new-symbol warmup.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MarketUpdate {
    pub timestamp: UnixNanos,
    pub selection: Option<UniverseSelection>,
    pub warmup_symbols: BTreeSet<InstrumentId>,
    pub bars: Vec<Bar>,
}

impl MarketUpdate {
    #[must_use]
    pub fn data_type() -> DataType {
        DataType::new("SlcMarketUpdate", None, None)
    }

    #[must_use]
    pub fn into_data(self) -> Data {
        Data::Custom(CustomData::new(Arc::new(self), Self::data_type()))
    }
}

impl HasTsInit for MarketUpdate {
    fn ts_init(&self) -> UnixNanos {
        self.timestamp
    }
}

impl CustomDataTrait for MarketUpdate {
    fn type_name(&self) -> &'static str {
        "SlcMarketUpdate"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn ts_event(&self) -> UnixNanos {
        self.timestamp
    }
    fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }
    fn clone_arc(&self) -> Arc<dyn CustomDataTrait> {
        Arc::new(self.clone())
    }
    fn eq_arc(&self, other: &dyn CustomDataTrait) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}
