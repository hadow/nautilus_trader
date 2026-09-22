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

//! Native dynamic-grid backtest and benchmark comparison.

use std::{
    cell::Cell,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use nautilus_backtest::dynamic_grid::{
    Benchmark, GridBacktestConfig, load_bars, load_quotes,
    portfolio::{
        PortfolioBenchmark, load_portfolio_config, load_portfolio_data, portfolio_benchmark_config,
        run_portfolio_backtest_with_progress,
    },
    portfolio_research::{PortfolioResearchConfig, PortfolioVariant, portfolio_walk_forward},
    research::{WalkForwardConfig, walk_forward},
    run_grid_backtest,
};
use nautilus_core::UnixNanos;
use serde_json::json;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PortfolioResearchFile {
    settings: PortfolioResearchConfig,
    candidates: Vec<PortfolioVariant>,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| {
        matches!(
            a.as_str(),
            "--portfolio"
                | "--dynamic-only"
                | "--compare"
                | "--ablation"
                | "--portfolio-walk-forward"
        )
    }) {
        let walk_forward = args[0] == "--portfolio-walk-forward";
        let ablation = args[0] == "--ablation";
        let auxiliary = walk_forward || ablation;
        anyhow::ensure!(
            args.len() == if auxiliary { 4 } else { 3 },
            "Usage: dynamic-grid-backtest --portfolio|--dynamic-only|--compare CONFIG.json OUTPUT.json; --ablation|--portfolio-walk-forward CONFIG.json RESEARCH.json OUTPUT.json"
        );
        let (config, mut configuration_inputs) = load_portfolio_config(Path::new(&args[1]))?;
        if auxiliary {
            configuration_inputs.push(Path::new(&args[2]).canonicalize()?);
        }
        let output_arg = &args[if auxiliary { 3 } else { 2 }];
        let output = Path::new(output_arg);
        let canonical_output = output.canonicalize().ok();
        let overwrites_input = configuration_inputs
            .iter()
            .chain(
                config
                    .instruments
                    .values()
                    .flat_map(|c| std::iter::once(&c.bars_path).chain(c.quotes_path.iter())),
            )
            .any(|input| {
                input == output
                    || canonical_output.as_ref().is_some_and(|output| {
                        input.canonicalize().is_ok_and(|input| &input == output)
                    })
            });
        anyhow::ensure!(
            !overwrites_input,
            "Output must not overwrite portfolio inputs"
        );
        let started = Instant::now();
        eprintln!(
            "[BACKTEST] Loading portfolio data: {} instruments",
            config.instruments.len()
        );
        let (bars, quotes) = load_portfolio_data(&config)?;
        let load_elapsed = started.elapsed();
        eprintln!(
            "[BACKTEST] Loaded {} bars, {} quotes in {:.2}s",
            bars.len(),
            quotes.len(),
            started.elapsed().as_secs_f64()
        );
        if ablation {
            let research: PortfolioResearchFile = serde_json::from_reader(File::open(&args[2])?)?;
            let mut reports = serde_json::Map::new();
            for variant in &research.candidates {
                anyhow::ensure!(
                    !reports.contains_key(&variant.name),
                    "Duplicate ablation name {}",
                    variant.name
                );
                let effective = variant.resolve(&config)?;
                let report = run_portfolio_backtest_with_progress(
                    &bars,
                    &quotes,
                    &effective,
                    PortfolioBenchmark::Dynamic,
                    progress(variant.name.clone(), None),
                )?;
                println!(
                    "{}: {}",
                    variant.name,
                    serde_json::to_string(&report.portfolio.metrics)?
                );
                reports.insert(
                    variant.name.clone(),
                    json!({"effective_config": effective, "report": report}),
                );
            }
            write_report(
                output_arg,
                &json!({"source_config": config, "bars": bars.len(), "quotes": quotes.len(),
                "ablations": reports, "notes": [
                    "Ablation replays share data, capital, costs and execution assumptions",
                    "This command is an in-sample diagnostic; use portfolio walk-forward for OOS claims"
                ]}),
            )?;
            return Ok(());
        }
        if walk_forward {
            let research: PortfolioResearchFile = serde_json::from_reader(File::open(&args[2])?)?;
            let candidates = research
                .candidates
                .iter()
                .map(|variant| variant.resolve(&config))
                .collect::<anyhow::Result<Vec<_>>>()?;
            eprintln!(
                "[RESEARCH] {} candidates, train/validation/test={}/{}/{} observed days; native replay per partition",
                candidates.len(),
                research.settings.train_days,
                research.settings.validation_days,
                research.settings.test_days
            );
            let report = portfolio_walk_forward(&bars, &quotes, &candidates, &research.settings)?;
            write_report(
                output_arg,
                &json!({"source_config": config, "research": research,
                "bars": bars.len(), "quotes": quotes.len(), "report": report}),
            )?;
            eprintln!(
                "[RESEARCH] Complete in {:.2}s",
                started.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        if args[0] == "--compare" {
            let mut reports = serde_json::Map::new();
            for (name, benchmark) in [
                ("buy_and_hold", PortfolioBenchmark::EqualWeightBuyHold),
                ("fixed_grid", PortfolioBenchmark::Fixed),
                ("original_dgt", PortfolioBenchmark::LegacyDgt),
                ("sadg", PortfolioBenchmark::Sadg),
            ] {
                let effective = portfolio_benchmark_config(&config, benchmark)?;
                let report = run_portfolio_backtest_with_progress(
                    &bars,
                    &quotes,
                    &config,
                    benchmark,
                    progress(name, None),
                )?;
                println!(
                    "{name}: {}",
                    serde_json::to_string(&report.portfolio.metrics)?
                );
                reports.insert(
                    name.to_string(),
                    json!({"effective_config": effective, "report": report}),
                );
            }
            write_report(
                output_arg,
                &json!({"source_config": config, "bars": bars.len(),
                "quotes": quotes.len(), "first_ns": bars[0].ts_event, "last_ns": bars[bars.len()-1].ts_event,
                "benchmarks": reports, "notes": [
                    "Fixed and dynamic grids use the same geometric levels, capital, costs, filters, risk limits and native execution assumptions",
                    "Fixed grids stop new entries at the first boundary; inventory follows the same configured risk policy",
                    "Buy-and-hold is equal-weight without rebalancing; its exposure differs from risk-limited grids",
                    "Bar-only replay models intrabar order; no real spread or queue data is fabricated",
                    "No parameter selection is made by this comparison"
                ]}),
            )?;
            eprintln!(
                "[BACKTEST] Comparison complete in {:.2}s",
                started.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        let replay_started = Instant::now();
        let last_input_elapsed = Rc::new(Cell::new(None));
        let dynamic = run_portfolio_backtest_with_progress(
            &bars,
            &quotes,
            &config,
            PortfolioBenchmark::Dynamic,
            progress("dynamic_grid", Some(Rc::clone(&last_input_elapsed))),
        )?;
        let replay_and_finalize_elapsed = replay_started.elapsed();
        let event_replay_elapsed = last_input_elapsed
            .get()
            .unwrap_or(replay_and_finalize_elapsed);
        let finalize_elapsed = replay_and_finalize_elapsed.saturating_sub(event_replay_elapsed);
        println!(
            "dynamic_portfolio: {}",
            serde_json::to_string(&dynamic.portfolio.metrics)?
        );
        // 单策略研究保留原配置的几何结构与风险参数，不运行基准或自动搜索参数。
        if args[0] == "--dynamic-only" {
            let counts = &dynamic.portfolio.diagnostics;
            let report_started = Instant::now();
            write_report(
                output_arg,
                &json!({"config": config, "bars": bars.len(), "quotes": quotes.len(),
                "first_ns": bars[0].ts_event, "last_ns": bars[bars.len()-1].ts_event,
                "dynamic_multi_asset_grid": dynamic,
                "notes": [
                    "Only the configured Dynamic Grid is run; no benchmarks or parameter selection",
                    "One account and one strategy; instrument geometry and risk settings are unchanged",
                    "Bar-only replay models intrabar order; no real spread or queue data is fabricated",
                    "Same-time events are causally ordered by instrument; no cross-instrument future marks"
                ]}),
            )?;
            let report_elapsed = report_started.elapsed();
            eprintln!(
                "[BACKTEST] Dynamic-only complete in {:.2}s; report={output_arg}",
                started.elapsed().as_secs_f64()
            );
            eprintln!(
                "[BACKTEST] Phase timing load={:.2}s event_replay={:.2}s drain_finalize={:.2}s report_write={:.2}s",
                load_elapsed.as_secs_f64(),
                event_replay_elapsed.as_secs_f64(),
                finalize_elapsed.as_secs_f64(),
                report_elapsed.as_secs_f64(),
            );
            eprintln!(
                "[BACKTEST] Event counts bars={} quotes={} order_reports={} timers={}",
                counts.bar_events, counts.quote_events, counts.order_events, counts.timer_events,
            );
            return Ok(());
        }
        let benchmark = run_portfolio_backtest_with_progress(
            &bars,
            &quotes,
            &config,
            PortfolioBenchmark::EqualWeightBuyHold,
            progress("equal_weight_buy_hold", None),
        )?;
        println!(
            "equal_weight_buy_hold: {}",
            serde_json::to_string(&benchmark.portfolio.metrics)?
        );
        eprintln!("[BACKTEST] Replay finalized; writing report to {output_arg}");
        write_report(
            output_arg,
            &json!({"config":config,"bars":bars.len(),"quotes":quotes.len(),
            "dynamic_multi_asset_grid":dynamic,"equal_weight_buy_hold":benchmark,
            "notes":["One account and one strategy per run; no summation of independent backtests",
                "Buy and hold uses equal initial weights and no rebalancing; different exposure than risk-limited grids",
                "Same-time events are causally ordered by instrument; no cross-instrument future marks"]}),
        )?;
        eprintln!(
            "[BACKTEST] Complete in {:.2}s",
            started.elapsed().as_secs_f64()
        );
        return Ok(());
    }
    anyhow::ensure!(
        (3..=5).contains(&args.len()),
        "Usage: dynamic-grid-backtest CONFIG.json BARS.csv OUTPUT.json [QUOTES.csv | --walk-forward RESEARCH.json]"
    );
    let config: GridBacktestConfig = serde_json::from_reader(File::open(&args[0])?)?;
    anyhow::ensure!(
        args[2] != args[0]
            && args[2] != args[1]
            && args.get(3).is_none_or(|v| v != &args[2])
            && args.get(4).is_none_or(|v| v != &args[2]),
        "Output must not overwrite an input file"
    );
    let bars = load_bars(Path::new(&args[1]), &config)?;
    if args.get(3).is_some_and(|a| a == "--walk-forward") {
        anyhow::ensure!(
            args.len() == 5,
            "--walk-forward requires a research configuration"
        );
        let research: WalkForwardConfig = serde_json::from_reader(File::open(&args[4])?)?;
        let report = walk_forward(&bars, &config, &research)?;
        println!(
            "Completed {} out-of-sample folds and {} training sensitivity cells",
            report.folds.len(),
            report.sensitivity.len()
        );
        write_report(
            &args[2],
            &json!({"config":config,"research":research,"input":args[1],"report":report}),
        )?;
        return Ok(());
    }
    anyhow::ensure!(args.len() <= 4, "Unexpected arguments");
    let quotes = if args.len() == 4 {
        load_quotes(Path::new(&args[3]), &config)?
    } else {
        Vec::new()
    };
    let mut reports = serde_json::Map::new();
    for (name, benchmark) in [
        ("buy_and_hold", Benchmark::BuyHold),
        ("traditional_fixed_grid", Benchmark::Fixed),
        ("original_dgt", Benchmark::LegacyDgt),
        ("sadg", Benchmark::Sadg),
    ] {
        let report = run_grid_backtest(&bars, &quotes, &config, benchmark)?;
        println!("{name}: {}", serde_json::to_string(&report.metrics)?);
        reports.insert(name.to_string(), serde_json::to_value(report)?);
    }
    let result = json!({"config":config,"input":args[1],"bars":bars.len(),"quotes":quotes.len(),"first_ns":bars[0].ts_event,"last_ns":bars[bars.len()-1].ts_event,"execution":if quotes.is_empty() {"Nautilus OHLC matching; intrabar order is modeled"} else {"Nautilus actual quote replay"},"benchmarks":reports});
    write_report(&args[2], &result)?;
    Ok(())
}

fn progress(
    label: impl Into<String>,
    last_input_elapsed: Option<Rc<Cell<Option<Duration>>>>,
) -> impl FnMut(usize, usize, u64) {
    let label = label.into();
    let started = Instant::now();
    eprintln!("[BACKTEST] Starting {label}");
    move |received, total, ts_ns| {
        if received == total
            && let Some(elapsed) = &last_input_elapsed
        {
            elapsed.set(Some(started.elapsed()));
        }
        if received == 1 || received % 1000 == 0 || received == total {
            eprintln!(
                "[BACKTEST] {label} events={received}/{total} ({:.1}%) time={} elapsed={:.2}s",
                received as f64 / total as f64 * 100.0,
                UnixNanos::from(ts_ns).to_rfc3339(),
                started.elapsed().as_secs_f64()
            );
        }
    }
}

fn write_report(path: &str, report: &serde_json::Value) -> anyhow::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(&mut writer, report)?;
    writer.flush()?;
    Ok(())
}
