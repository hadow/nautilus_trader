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

//! Notebook mathematics independent of Nautilus, network and brokerage APIs.

use std::collections::VecDeque;

use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

pub const MINUTE: u64 = 60_000_000_000;

/// UTC nanoseconds use the same representation as Nautilus `UnixNanos` at the boundary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Session {
    pub open: u64,
    pub close: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct MinuteBar {
    /// Completion time, never the provider's minute-start label.
    pub timestamp: u64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub vwap: Decimal,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Timing {
    /// Notebook index 30 means the 10:00-labelled bar, completed at 10:01.
    NotebookLabel,
    #[default]
    SessionClose,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionTarget {
    Long,
    Short,
    #[default]
    Flat,
}

impl PositionTarget {
    #[must_use]
    pub fn sign(self) -> i32 {
        match self {
            Self::Long => 1,
            Self::Short => -1,
            Self::Flat => 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub lookback: usize,
    pub volume_lookback: usize,
    pub volatility_lookback: usize,
    pub volatility_multiplier: Decimal,
    pub relative_volume_threshold: Option<Decimal>,
    pub target_volatility: f64,
    pub max_leverage: Decimal,
    pub interval_minutes: usize,
    pub timing: Timing,
    pub baseline_stop: bool,
    /// Uses the held model direction for exits; false preserves the Notebook stop OR.
    pub directional_stops: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            lookback: 14,
            volume_lookback: 14,
            volatility_lookback: 14,
            volatility_multiplier: Decimal::ONE,
            relative_volume_threshold: Some(Decimal::ONE),
            target_volatility: 0.03,
            max_leverage: Decimal::from(4),
            interval_minutes: 30,
            timing: Timing::SessionClose,
            baseline_stop: false,
            directional_stops: false,
        }
    }
}

impl ModelConfig {
    /// Checks the mathematical parameter bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid windows, thresholds or leverage.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (2..=1000).contains(&self.lookback),
            "invalid sigma lookback"
        );
        anyhow::ensure!(
            (2..=1000).contains(&self.volume_lookback),
            "invalid volume lookback"
        );
        anyhow::ensure!(
            (2..=1000).contains(&self.volatility_lookback),
            "invalid volatility lookback"
        );
        anyhow::ensure!(
            self.volatility_multiplier > Decimal::ZERO,
            "VM must be positive"
        );
        anyhow::ensure!(
            self.relative_volume_threshold
                .is_none_or(|v| v > Decimal::ZERO),
            "invalid RVOL threshold"
        );
        anyhow::ensure!(
            self.target_volatility.is_finite()
                && self.target_volatility > 0.0
                && self.target_volatility <= 1.0,
            "invalid target volatility"
        );
        anyhow::ensure!(
            self.max_leverage > Decimal::ZERO && self.max_leverage <= Decimal::from(4),
            "invalid leverage cap"
        );
        anyhow::ensure!(
            (1..=60).contains(&self.interval_minutes),
            "invalid decision interval"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct MinuteFeatures {
    pub timestamp: u64,
    pub session_open: u64,
    pub minute_index: usize,
    pub daily_open: Decimal,
    pub previous_close: Option<Decimal>,
    pub sigma: Option<Decimal>,
    pub upper_bound: Option<Decimal>,
    pub lower_bound: Option<Decimal>,
    pub anchored_vwap: Option<Decimal>,
    pub rvol: Option<Decimal>,
    /// IEEE positive infinity when positive current volume follows a zero historical mean.
    pub rvol_infinite: bool,
    pub daily_volatility: Option<f64>,
    pub leverage: Decimal,
    pub decision_time: bool,
    pub raw_signal: PositionTarget,
    pub long_stop: bool,
    pub short_stop: bool,
    pub target: PositionTarget,
}

#[derive(Clone, Debug)]
struct Profile {
    close: Decimal,
    moves: Vec<Decimal>,
    volumes: Vec<Decimal>,
}

#[derive(Clone, Debug)]
struct Current {
    session: Session,
    open: Decimal,
    price_volume: Decimal,
    volume: Decimal,
    profile: Profile,
}

/// Causal feature and explicit signal-state model, with no execution engine dependency.
#[derive(Clone, Debug)]
pub struct ReferenceModel {
    config: ModelConfig,
    history: VecDeque<Profile>,
    current: Option<Current>,
    last_timestamp: Option<u64>,
    target: PositionTarget,
}

impl ReferenceModel {
    /// Creates an empty model with validated parameters.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid mathematical parameters.
    pub fn new(config: ModelConfig) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            history: VecDeque::new(),
            current: None,
            last_timestamp: None,
            target: PositionTarget::Flat,
        })
    }

    #[must_use]
    pub fn warmed_sessions(&self) -> usize {
        self.history.len()
    }

    /// Processes one completed minute; malformed or duplicate data is rejected before mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid prices, session alignment, missing or repeated minutes.
    pub fn on_bar(&mut self, session: Session, bar: MinuteBar) -> anyhow::Result<MinuteFeatures> {
        anyhow::ensure!(
            session.close > session.open && (session.close - session.open).is_multiple_of(MINUTE),
            "invalid session"
        );
        let duration = (session.close - session.open) / MINUTE;
        anyhow::ensure!(duration <= 390, "session exceeds 390 minutes");
        anyhow::ensure!(
            bar.timestamp > session.open
                && bar.timestamp <= session.close
                && (bar.timestamp - session.open).is_multiple_of(MINUTE),
            "bar outside session or not minute aligned"
        );
        anyhow::ensure!(
            self.last_timestamp.is_none_or(|ts| ts < bar.timestamp),
            "duplicate or out-of-order bar"
        );
        anyhow::ensure!(
            bar.low > Decimal::ZERO
                && bar.high >= bar.open.max(bar.close)
                && bar.low <= bar.open.min(bar.close)
                && bar.volume >= Decimal::ZERO
                && bar.vwap > Decimal::ZERO,
            "invalid OHLCV/VWAP"
        );
        let index = usize::try_from((bar.timestamp - session.open) / MINUTE - 1)?;
        match &self.current {
            Some(day) => anyhow::ensure!(
                day.session == session && day.profile.moves.len() == index,
                "missing minute or incomplete preceding session"
            ),
            None => anyhow::ensure!(index == 0, "session must start at its first minute"),
        }
        if self.current.is_none() {
            self.target = PositionTarget::Flat;
            self.current = Some(Current {
                session,
                open: bar.open,
                price_volume: Decimal::ZERO,
                volume: Decimal::ZERO,
                profile: Profile {
                    close: bar.close,
                    moves: Vec::with_capacity(duration as usize),
                    volumes: Vec::with_capacity(duration as usize),
                },
            });
        }
        let previous_close = self.history.back().map(|p| p.close);
        let sigma = self.average(index, self.config.lookback, false);
        let volume_average = self.average(index, self.config.volume_lookback, true);
        let rvol = volume_average
            .filter(|v| *v > Decimal::ZERO)
            .map(|v| bar.volume / v);
        let rvol_infinite = volume_average == Some(Decimal::ZERO) && bar.volume > Decimal::ZERO;
        let daily_volatility = self.daily_volatility();
        let leverage = leverage(
            daily_volatility,
            self.config.target_volatility,
            self.config.max_leverage,
        )?;
        let day = self
            .current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("current session unavailable"))?;
        day.price_volume += bar.vwap * bar.volume;
        day.volume += bar.volume;
        day.profile.close = bar.close;
        day.profile
            .moves
            .push((bar.close / day.open - Decimal::ONE).abs());
        day.profile.volumes.push(bar.volume);
        let anchored_vwap = (day.volume > Decimal::ZERO).then(|| day.price_volume / day.volume);
        let upper_bound = sigma
            .zip(previous_close)
            .map(|(s, p)| day.open.max(p) * (Decimal::ONE + self.config.volatility_multiplier * s));
        let lower_bound = sigma
            .zip(previous_close)
            .map(|(s, p)| day.open.min(p) * (Decimal::ONE - self.config.volatility_multiplier * s));
        let elapsed = match self.config.timing {
            Timing::NotebookLabel => index,
            Timing::SessionClose => index + 1,
        };
        let decision_time = elapsed >= self.config.interval_minutes
            && elapsed.is_multiple_of(self.config.interval_minutes)
            && bar.timestamp < session.close
            && sigma.is_some()
            && previous_close.is_some();
        let mut raw_signal = PositionTarget::Flat;
        let mut long_stop = false;
        let mut short_stop = false;
        if let (Some(upper), Some(lower)) = (upper_bound, lower_bound) {
            let volume_ok = self
                .config
                .relative_volume_threshold
                .is_none_or(|t| rvol_infinite || rvol.unwrap_or(Decimal::ONE) >= t);
            if decision_time && volume_ok {
                if bar.close > upper {
                    raw_signal = PositionTarget::Long;
                } else if bar.close < lower {
                    raw_signal = PositionTarget::Short;
                }
            }
            let long_threshold = if self.config.baseline_stop {
                lower
            } else {
                upper.max(anchored_vwap.unwrap_or(upper))
            };
            let short_threshold = if self.config.baseline_stop {
                upper
            } else {
                lower.min(anchored_vwap.unwrap_or(lower))
            };
            long_stop = (self.config.baseline_stop || anchored_vwap.is_some())
                && bar.close < long_threshold;
            short_stop = (self.config.baseline_stop || anchored_vwap.is_some())
                && bar.close > short_threshold;
            if decision_time {
                // Entry priority is unchanged in both versions
                if raw_signal != PositionTarget::Flat {
                    self.target = raw_signal;
                } else if if self.config.directional_stops {
                    match self.target {
                        PositionTarget::Long => long_stop,
                        PositionTarget::Short => short_stop,
                        PositionTarget::Flat => false,
                    }
                } else {
                    long_stop || short_stop
                } {
                    self.target = PositionTarget::Flat;
                }
            }
        }
        let features = MinuteFeatures {
            timestamp: bar.timestamp,
            session_open: session.open,
            minute_index: index,
            daily_open: day.open,
            previous_close,
            sigma,
            upper_bound,
            lower_bound,
            anchored_vwap,
            rvol,
            rvol_infinite,
            daily_volatility,
            leverage,
            decision_time,
            raw_signal,
            long_stop,
            short_stop,
            target: self.target,
        };
        self.last_timestamp = Some(bar.timestamp);
        if bar.timestamp == session.close {
            let day = self
                .current
                .take()
                .ok_or_else(|| anyhow::anyhow!("closing session unavailable"))?;
            self.history.push_back(day.profile);
            let capacity = self
                .config
                .lookback
                .max(self.config.volume_lookback)
                .max(self.config.volatility_lookback + 1);
            while self.history.len() > capacity {
                self.history.pop_front();
            }
        }
        Ok(features)
    }

    fn average(&self, index: usize, count: usize, volume: bool) -> Option<Decimal> {
        if self.history.len() < count {
            return None;
        }
        let mut sum = Decimal::ZERO;
        for day in self.history.iter().rev().take(count) {
            sum += if volume {
                *day.volumes.get(index)?
            } else {
                *day.moves.get(index)?
            };
        }
        Some(sum / Decimal::from(count as u64))
    }

    fn daily_volatility(&self) -> Option<f64> {
        let count = self.config.volatility_lookback;
        if self.history.len() <= count {
            return None;
        }
        let mut returns = Vec::with_capacity(count);
        let mut days = self.history.iter().rev().take(count + 1);
        let mut newer = days.next()?.close;
        for day in days {
            returns.push((newer / day.close - Decimal::ONE).to_f64()?);
            newer = day.close;
        }
        sample_std(&returns)
    }
}

#[must_use]
pub fn sample_std(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance =
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (values.len() - 1) as f64;
    variance.is_finite().then(|| variance.sqrt())
}

/// Computes a capped volatility target using only a supplied historical estimate.
///
/// # Errors
///
/// Returns an error for invalid volatility or unrepresentable leverage.
pub fn leverage(volatility: Option<f64>, target: f64, maximum: Decimal) -> anyhow::Result<Decimal> {
    match volatility {
        None => Ok(Decimal::ONE.min(maximum)),
        Some(0.0) => Ok(maximum),
        Some(v) if v.is_finite() && v > 0.0 => Ok(Decimal::try_from(target / v)?.min(maximum)),
        _ => anyhow::bail!("invalid daily volatility"),
    }
}

/// Exact lot rounding with execution safety limits, independent of signal math.
///
/// # Errors
///
/// Returns an error for invalid capital, prices, limits or arithmetic overflow.
pub fn quantity(
    equity: Decimal,
    leverage: Decimal,
    open: Decimal,
    lot: Decimal,
    max_notional: Decimal,
    buying_power: Decimal,
) -> anyhow::Result<Decimal> {
    anyhow::ensure!(
        equity > Decimal::ZERO
            && leverage > Decimal::ZERO
            && open > Decimal::ZERO
            && lot > Decimal::ZERO
            && max_notional >= Decimal::ZERO
            && buying_power >= Decimal::ZERO,
        "invalid sizing inputs"
    );
    Ok(equity
        .checked_mul(leverage)
        .ok_or_else(|| anyhow::anyhow!("notional overflow"))?
        .min(max_notional)
        .min(buying_power)
        .checked_div(open)
        .and_then(|q| q.checked_div(lot))
        .ok_or_else(|| anyhow::anyhow!("quantity overflow"))?
        .floor()
        * lot)
}
