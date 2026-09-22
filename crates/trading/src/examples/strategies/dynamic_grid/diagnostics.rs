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

//! Passive per-instrument research observations; never used for order or risk decisions.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    config::{GridConfig, TrendPolicy},
    engine::{GridEngine, spacing_components},
    regime::RegimeDetector,
};

/// Instrument diagnostics retained by the existing report/checkpoint, not a second trading ledger.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GridDiagnostics {
    /// Completed bar callbacks observed by the strategy.
    pub bar_events: u64,
    /// Valid quote callbacks observed by the strategy.
    pub quote_events: u64,
    /// Native order lifecycle callbacks, including fills and rejects.
    pub order_events: u64,
    /// Portfolio watchdog callbacks; one callback may inspect several instruments.
    pub timer_events: u64,
    /// One sample per initialized completed bar, even while entries are disabled.
    pub spacing: Vec<SpacingObservation>,
    /// Number of initialized bars for which diagnostic spacing could not be calculated.
    pub spacing_errors: u64,
    /// Immutable grid plans at creation, including prices and lot-rounded zero quantities.
    pub grids: Vec<GridEngine>,
    /// Completed resets only, after the cancellation reconciliation barrier.
    pub resets: Vec<ResetObservation>,
    /// Dispatched cancellation requests, not confirmed cancellations or hypothetical fills.
    pub cancellations: Vec<CancelObservation>,
    /// Actual rejection/denial observations keyed by stable client order identity.
    pub rejections: BTreeMap<String, RejectionObservation>,
    /// Broker cancellation failures, distinct from order rejections.
    pub cancel_rejections: BTreeMap<String, RejectionObservation>,
    /// First observed timestamp per instrument hard-stop reason; legacy history is not invented.
    pub risk_stops: BTreeMap<String, u64>,
    /// Completed bars blocked by the first applicable entry filter, not elapsed time.
    pub blocked_bars: BTreeMap<String, ObservationCount>,
    /// Sizing/admission attempts reduced to zero, not broker rejections or unique signals.
    pub zero_admissions: BTreeMap<String, ObservationCount>,
}

/// Counts observations without pretending consecutive bars are independent trade opportunities.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationCount {
    /// Number of observations.
    pub count: u64,
    /// First observed timestamp.
    pub first_ns: u64,
    /// Last observed timestamp.
    pub last_ns: u64,
}

impl ObservationCount {
    pub(super) fn record(counts: &mut BTreeMap<String, Self>, reason: &str, now: u64) {
        let value = counts.entry(reason.to_string()).or_default();
        if value.count == 0 {
            value.first_ns = now;
        }
        value.count = value.count.saturating_add(1);
        value.last_ns = now;
    }
}

/// Prospective spacing and frozen active spacing are deliberately separate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpacingObservation {
    /// Completed signal bar timestamp.
    pub ts_ns: u64,
    /// ATR known at that timestamp, before any future bar.
    pub atr: Decimal,
    /// Completed bar close used for this diagnostic calculation.
    pub price: Decimal,
    /// Spacing before the min/max/cost clamps, including the configured trend multiplier.
    pub raw: Decimal,
    /// Prospective spacing after all clamps; this does not move an existing grid.
    pub effective: Decimal,
    /// Frozen spacing of the active grid after this bar; None if no grid exists.
    pub active: Option<Decimal>,
}

/// A reconciled reset and its original generation's actual acquisition history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetObservation {
    /// Completion timestamp, which may be later than the original reset signal.
    pub ts_ns: u64,
    /// Retired generation.
    pub grid_id: u64,
    /// Original selected trigger; priority remains up, down, regime, then volatility.
    pub reason: String,
    /// Fresh mark at reset completion.
    pub price: Decimal,
    /// Distinct buy orders with any actual fill, including partial fills and seed inventory.
    pub entry_orders_with_fills: usize,
    /// Subset on negative levels, excluding positive-level seed acquisitions.
    pub lower_entry_orders_with_fills: usize,
}

/// One dispatched cancellation, with causal (possibly stale) market context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelObservation {
    /// Dispatch timestamp.
    pub ts_ns: u64,
    /// Stable native client order identity.
    pub order_id: String,
    /// Generation owning the order, not necessarily the current grid.
    pub grid_id: u64,
    /// Signed level index.
    pub level: i32,
    /// Whether the order acquires inventory.
    pub buy: bool,
    /// Caller-provided cancellation cause, not inferred from later state.
    pub reason: String,
    /// Quantity already filled when requesting cancellation.
    pub filled: Decimal,
    /// Last available market mark.
    pub price: Option<Decimal>,
    /// Timestamp of that mark; do not treat an overnight watchdog mark as a fresh quote.
    pub mark_ns: u64,
    /// Highest-priced negative entry level, only for the matching active generation.
    pub first_buy_level: Option<Decimal>,
    /// (Mark - first buy level) / first buy level; negative means already below it.
    pub first_buy_distance_pct: Option<Decimal>,
}

/// The first real rejection observed for an order identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RejectionObservation {
    /// Local observation timestamp.
    pub ts_ns: u64,
    /// Native engine/broker reason, retaining the rejection-versus-denial prefix.
    pub reason: String,
}

impl GridDiagnostics {
    pub(super) fn observe_spacing(
        &mut self,
        config: &GridConfig,
        regime: &RegimeDetector,
        price: Decimal,
        grid: Option<&GridEngine>,
    ) {
        let signal = &regime.snapshot;
        if !signal.initialized {
            return;
        }
        let multiplier = if regime.policy(config) == TrendPolicy::WiderGrid {
            config.trend_spacing_multiplier
        } else {
            Decimal::ONE
        };
        // 与执行层共用计算函数；这里只观察已完成 K 线，不改变冻结网格或传播诊断错误。
        if let Some(atr) = Decimal::from_f64_retain(signal.atr)
            && let Ok((raw, effective)) = spacing_components(config, atr, price, multiplier)
        {
            self.spacing.push(SpacingObservation {
                ts_ns: signal.ts_ns,
                atr,
                price,
                raw,
                effective,
                active: grid.map(|g| g.spacing),
            });
        } else {
            self.spacing_errors = self.spacing_errors.saturating_add(1);
        }
    }

    pub(super) fn observe_stop(&mut self, reason: Option<&str>, now: u64) {
        if let Some(reason) = reason
            && !self.risk_stops.contains_key(reason)
        {
            self.risk_stops.insert(reason.to_string(), now);
        }
    }
}
