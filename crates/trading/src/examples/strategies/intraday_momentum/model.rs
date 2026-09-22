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

use nautilus_core::UnixNanos;
use nautilus_model::{data::Bar, types::Price};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub use super::reference::PositionTarget;
use super::{
    IntradayMomentumConfig,
    reference::{MinuteBar, MinuteFeatures, ReferenceModel, Session},
};

/// One causal strategy decision from completed regular-session bars.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct IntradayMomentumDecision {
    pub timestamp: UnixNanos,
    pub session_open: UnixNanos,
    pub target: PositionTarget,
    pub open: Price,
    pub close: Price,
    pub vwap: Decimal,
    pub sigma_open: Decimal,
    pub upper_bound: Decimal,
    pub lower_bound: Decimal,
    pub relative_volume: Option<Decimal>,
    pub daily_volatility: f64,
    pub leverage: Decimal,
}

/// Nautilus value conversion around the independent reference model.
#[derive(Clone, Debug)]
pub struct IntradayMomentumModel {
    config: IntradayMomentumConfig,
    reference: ReferenceModel,
    latest: Option<MinuteFeatures>,
}

impl IntradayMomentumModel {
    /// Creates the Nautilus conversion boundary around the reference model.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid parameters or sessions.
    pub fn new(config: IntradayMomentumConfig) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            reference: ReferenceModel::new(config.model_config())?,
            config,
            latest: None,
        })
    }

    /// Feeds ordered OHLC-only history with explicit approximation opt-in.
    ///
    /// # Errors
    ///
    /// Returns an error for missing minutes, invalid prices or absent opt-in.
    pub fn warmup(&mut self, bars: Vec<Bar>) -> anyhow::Result<()> {
        for bar in bars {
            self.on_bar(bar)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn warmed_sessions(&self) -> usize {
        self.reference.warmed_sessions()
    }

    #[must_use]
    pub fn latest_features(&self) -> Option<MinuteFeatures> {
        self.latest
    }

    /// Supports the old OHLC-only examples only with an explicit approximation opt-in.
    ///
    /// # Errors
    ///
    /// Returns an error for absent opt-in or invalid input bars.
    pub fn on_bar(&mut self, bar: Bar) -> anyhow::Result<Option<IntradayMomentumDecision>> {
        anyhow::ensure!(
            self.config.allow_ohlc_vwap_approximation,
            "provider minute VWAP required; OHLC typical price is not Notebook parity"
        );
        let vwap = (bar.high.as_decimal() + bar.low.as_decimal() + bar.close.as_decimal())
            / Decimal::from(3);
        self.on_bar_with_vwap(bar, vwap)
    }

    /// Processes an already completed bar with its provider VWAP.
    ///
    /// # Errors
    ///
    /// Returns an error for an unexpected instrument, dividend adjustment or invalid chronology.
    pub fn on_bar_with_vwap(
        &mut self,
        bar: Bar,
        vwap: Decimal,
    ) -> anyhow::Result<Option<IntradayMomentumDecision>> {
        anyhow::ensure!(
            bar.instrument_id() == self.config.instrument_id,
            "unexpected instrument"
        );
        let Some(session) = self.config.session(bar.ts_event) else {
            return Ok(None);
        };
        anyhow::ensure!(
            session.dividend == Decimal::ZERO,
            "Notebook has no dividend adjustment; use consistently adjusted source data"
        );
        let f = self.reference.on_bar(
            Session {
                open: session.open.as_u64(),
                close: session.close.as_u64(),
            },
            MinuteBar {
                timestamp: bar.ts_event.as_u64(),
                open: bar.open.as_decimal(),
                high: bar.high.as_decimal(),
                low: bar.low.as_decimal(),
                close: bar.close.as_decimal(),
                volume: bar.volume.as_decimal(),
                vwap,
            },
        )?;
        self.latest = Some(f);
        if !f.decision_time {
            return Ok(None);
        }
        let (Some(vwap), Some(sigma_open), Some(upper_bound), Some(lower_bound)) =
            (f.anchored_vwap, f.sigma, f.upper_bound, f.lower_bound)
        else {
            return Ok(None);
        };
        Ok(Some(IntradayMomentumDecision {
            timestamp: bar.ts_event,
            session_open: session.open,
            target: f.target,
            open: Price::from_decimal(f.daily_open)?,
            close: bar.close,
            vwap,
            sigma_open,
            upper_bound,
            lower_bound,
            relative_volume: f.rvol,
            daily_volatility: f.daily_volatility.unwrap_or(0.0),
            leverage: f.leverage,
        }))
    }
}

pub(super) fn sized_quantity(
    equity: Decimal,
    capital_fraction: Decimal,
    leverage: Decimal,
    open: Price,
    lot_size: Decimal,
) -> anyhow::Result<Decimal> {
    anyhow::ensure!(equity > Decimal::ZERO, "account equity must be positive");
    anyhow::ensure!(
        capital_fraction > Decimal::ZERO && capital_fraction <= Decimal::ONE,
        "capital_fraction must be in (0, 1]",
    );
    anyhow::ensure!(leverage > Decimal::ZERO, "leverage must be positive");
    anyhow::ensure!(
        open.as_decimal() > Decimal::ZERO,
        "session open must be positive"
    );
    anyhow::ensure!(lot_size > Decimal::ZERO, "lot size must be positive");
    super::reference::quantity(
        equity * capital_fraction,
        leverage,
        open.as_decimal(),
        lot_size,
        Decimal::MAX,
        Decimal::MAX,
    )
}
