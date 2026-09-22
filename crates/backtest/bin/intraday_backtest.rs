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

mod intraday;

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
    anyhow::ensure!(!bars.is_empty(), "requested range has no bars");
    let outcome = intraday::run(&bars, &config)?;
    intraday::write(&output, &outcome)?;
    println!("{}", serde_json::to_string_pretty(&outcome.metrics)?);
    println!("{}", serde_json::to_string_pretty(&outcome.parity)?);
    Ok(())
}
