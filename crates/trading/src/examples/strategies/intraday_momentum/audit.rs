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

//! Literal, noncausal Notebook accounting for offline parity audits only.
//!
//! This batch API deliberately requires the entire sample. The streaming strategy never calls it.
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Serialize;

use super::reference::{
    MinuteBar, ModelConfig, ReferenceModel, Session, Timing, leverage, sample_std,
};

#[derive(Clone, Debug, Serialize)]
pub struct NotebookDay {
    pub session_open: u64,
    pub gross_return: f64,
    pub net_return: f64,
    pub leverage: Decimal,
    pub quantity: Decimal,
    pub turnover: i32,
    pub cost: Decimal,
    pub equity: Decimal,
}

/// Reproduces cell 25 including current-day volatility and unshifted position returns.
///
/// # Errors
/// Returns an error for malformed input, invalid configuration or numeric conversion failure.
pub fn notebook_returns(
    bars: &[(Session, MinuteBar)],
    config: &ModelConfig,
    initial_equity: Decimal,
    cost_per_share: Decimal,
) -> anyhow::Result<Vec<NotebookDay>> {
    anyhow::ensure!(
        initial_equity > Decimal::ZERO && cost_per_share >= Decimal::ZERO,
        "invalid capital or cost"
    );
    anyhow::ensure!(
        !config.directional_stops,
        "directional exits are a research variant, not literal Notebook behavior"
    );
    let mut model_config = config.clone();
    model_config.timing = Timing::NotebookLabel;
    let mut model = ReferenceModel::new(model_config)?;
    let mut equity = initial_equity;
    let mut daily_returns = Vec::new();
    let mut previous_close = None;
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bars.len() {
        let session = bars[cursor].0;
        let end = cursor + bars[cursor..].partition_point(|(s, _)| s.open == session.open);
        let day = &bars[cursor..end];
        let close = day
            .last()
            .ok_or_else(|| anyhow::anyhow!("empty session"))?
            .1
            .close;
        if let Some(previous) = previous_close {
            let value: Decimal = close / previous - Decimal::ONE;
            daily_returns.push(
                value
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("return conversion"))?,
            );
        }
        previous_close = Some(close);
        let mut factor = 1.0;
        let mut prior_position = 0_i32;
        let mut turnover = 0_i32;
        let mut eligible = false;
        for (index, (session, bar)) in day.iter().enumerate() {
            let features = model.on_bar(*session, *bar)?;
            if features.sigma.is_none() {
                continue;
            }
            eligible = true;
            let position = features.target.sign();
            turnover += (position - prior_position).abs();
            prior_position = position;
            let next = day.get(index + 1).map_or(bar.close, |(_, b)| b.open);
            let interval = (next / bar.open - Decimal::ONE)
                .to_f64()
                .ok_or_else(|| anyhow::anyhow!("interval conversion"))?;
            factor *= 1.0 + f64::from(position) * interval;
        }
        if eligible {
            let count = config.volatility_lookback;
            let volatility = if daily_returns.len() >= count {
                sample_std(&daily_returns[daily_returns.len() - count..])
            } else {
                None
            };
            let leverage = leverage(volatility, config.target_volatility, config.max_leverage)?;
            let quantity = (equity * leverage / day[0].1.open).floor();
            let cost = Decimal::from(turnover) * quantity * cost_per_share;
            let gross_return = factor - 1.0;
            let net_decimal = Decimal::try_from(gross_return)? * leverage - cost / equity;
            equity *= Decimal::ONE + net_decimal;
            output.push(NotebookDay {
                session_open: session.open,
                gross_return,
                net_return: net_decimal
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("net return conversion"))?,
                leverage,
                quantity,
                turnover,
                cost,
                equity,
            });
        }
        cursor = end;
    }
    Ok(output)
}
