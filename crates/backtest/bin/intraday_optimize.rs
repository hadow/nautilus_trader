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

//! Chronological, train-only parameter selection with a single final OOS replay.
mod intraday;

use std::{collections::BTreeSet, fs};

use rust_decimal::Decimal;

fn main() -> anyhow::Result<()> {
    let intraday::Arguments {
        input,
        output,
        config,
        start,
        end,
        audit_only,
    } = intraday::arguments()?;
    let mut bars = intraday::load(&input)?;
    bars.retain(|(s, _)| {
        intraday::date(s.open).is_ok_and(|d| {
            start.as_ref().is_none_or(|v| &d >= v) && end.as_ref().is_none_or(|v| &d <= v)
        })
    });
    if audit_only {
        return intraday::audit_existing(&bars, &config, &output);
    }
    let days = bars
        .iter()
        .map(|(s, _)| s.open)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let split = days.len() * 8 / 10;
    anyhow::ensure!(
        split > 14 && days.len() - split > 14,
        "need at least 15 sessions on each side of chronological 80/20 split"
    );
    let boundary = days[split];
    let split_bar = bars.partition_point(|(s, _)| s.open < boundary);
    let train = &bars[..split_bar];
    let test = &bars[split_bar..];
    fs::create_dir_all(&output)?;
    // Create-new prevents an accidental second OOS selection/evaluation in the same experiment.
    let _experiment = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output.join("experiment.lock"))?;
    let mut rows = Vec::new();
    let mut best = None;
    let mut best_sharpe = f64::NEG_INFINITY;
    for lookback in [14, 30, 90] {
        if lookback >= split {
            continue;
        }
        for vm in [
            Decimal::new(10, 1),
            Decimal::new(12, 1),
            Decimal::new(15, 1),
        ] {
            for target in [0.01, 0.02, 0.03] {
                for rvol in [10, 12, 15, 18, 20, 25] {
                    let mut candidate = config.clone();
                    candidate.model.lookback = lookback;
                    candidate.model.volatility_multiplier = vm;
                    candidate.model.target_volatility = target;
                    candidate.model.relative_volume_threshold = Some(Decimal::new(rvol, 1));
                    let result = intraday::run(train, &candidate)?;
                    let sharpe = result.metrics["sharpe"].as_f64();
                    rows.push(serde_json::json!({"config":candidate,"train":result.metrics}));
                    if let Some(sharpe) = sharpe
                        && sharpe > best_sharpe
                    {
                        best_sharpe = sharpe;
                        best = Some(candidate);
                    }
                    fs::write(
                        output.join("train_sweep.json"),
                        serde_json::to_vec_pretty(&rows)?,
                    )?;
                }
            }
        }
    }
    let best =
        best.ok_or_else(|| anyhow::anyhow!("no finite training score; OOS not evaluated"))?;
    fs::write(
        output.join("selected_config.json"),
        serde_json::to_vec_pretty(&best)?,
    )?;
    let result = intraday::run(test, &best)?;
    intraday::write(&output.join("oos"), &result)?;
    fs::write(
        output.join("split.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"train_sessions":split,"test_sessions":days.len()-split,"first_test_session":intraday::date(boundary)?,"oos_evaluations":1,"training_evaluations":rows.len(),"state_shared_between_parameters":false,"lookbacks_without_sufficient_history_are_skipped":true}),
        )?,
    )?;
    println!(
        "Train-only sweep completed; one OOS evaluation saved to {}",
        output.display()
    );
    Ok(())
}
