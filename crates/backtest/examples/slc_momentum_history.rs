// Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
// Licensed under the GNU Lesser General Public License Version 3.0.

//! Bounded-memory daily replays of preselected Longbridge history through the same native engine.
//!
//! 中文说明：逐日读取冻结候选与分钟 CSV，构造显式标注为 synthetic 的报价/成交假设，
//! 每日清仓后传递账户净值，避免把整月数据一次性装入内存。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::Bar,
    identifiers::InstrumentId,
    types::{Price, Quantity},
};
use nautilus_trading::examples::strategies::slc_momentum::{
    MarketUpdate, Session, SlcMomentumConfig, TradeSide, UniverseSelection,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::{DateRange, InputEvent, RunInput};

const MINUTE: u64 = 60_000_000_000;

#[derive(Deserialize)]
struct HistoryPlan {
    provenance: String,
    starting_equity: rust_decimal::Decimal,
    spread_bps: rust_decimal::Decimal,
    opening_spread_multiplier: rust_decimal::Decimal,
    quote_depth: nautilus_model::types::Quantity,
    slippage_probability: f64,
    daily_cache_dir: PathBuf,
    strategy: SlcMomentumConfig,
    price_increments: BTreeMap<InstrumentId, Price>,
    selections: Vec<UniverseSelection>,
    ranges: BTreeMap<InstrumentId, Session>,
}

fn minute_rows(
    root: &Path,
    id: InstrumentId,
    from: UnixNanos,
    to: UnixNanos,
    price_precision: u8,
    normalizations: &mut BTreeMap<InstrumentId, BTreeSet<UnixNanos>>,
    invalid_prices: &mut BTreeMap<InstrumentId, BTreeSet<UnixNanos>>,
) -> anyhow::Result<Vec<Bar>> {
    let directory = root.join("minute").join(id.to_string());
    let kind = format!("{id}-1-MINUTE-LAST-EXTERNAL").parse()?;
    let mut bars = BTreeMap::new();
    for file in
        fs::read_dir(&directory).with_context(|| format!("missing minute directory {id}"))?
    {
        let path = file?.path();
        if path.extension().is_none_or(|e| e != "csv") {
            continue;
        }
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            let f = line.split(',').collect::<Vec<_>>();
            anyhow::ensure!(f.len() == 6, "invalid minute CSV");
            let second: u64 = f[0].parse()?;
            let end = UnixNanos::from(
                second
                    .checked_mul(1_000_000_000)
                    .and_then(|n| n.checked_add(MINUTE))
                    .context("minute timestamp overflow")?,
            );
            if end <= from || end > to {
                continue;
            }
            let (open, source_high, source_low, close) = (
                f[1].parse::<Decimal>()?,
                f[2].parse::<Decimal>()?,
                f[3].parse::<Decimal>()?,
                f[4].parse::<Decimal>()?,
            );
            let volume = f[5].parse::<u64>()?;
            if [open, source_high, source_low, close]
                .iter()
                .any(|price| *price <= Decimal::ZERO)
            {
                // A reported volume with no valid price is a data gap, not a zero-price fill.
                invalid_prices.entry(id).or_default().insert(end);
                continue;
            }
            // Match Longbridge parse_bar_inner's observed-price envelope. The core
            // backtest cannot depend on the adapter; raw CSV values remain untouched.
            let high = source_high.max(open).max(close);
            let low = source_low.min(open).min(close);
            if high != source_high || low != source_low {
                normalizations.entry(id).or_default().insert(end);
            }
            let bar = Bar::new_checked(
                kind,
                Price::from_decimal_dp(open, price_precision)?,
                Price::from_decimal_dp(high, price_precision)?,
                Price::from_decimal_dp(low, price_precision)?,
                Price::from_decimal_dp(close, price_precision)?,
                Quantity::from(volume),
                end,
                end,
            )
            .with_context(|| format!("invalid minute bar for {id} at {end}: {line}"))?;
            if let Some(old) = bars.insert(end, bar) {
                anyhow::ensure!(old == bar, "conflicting history pages for {id}");
            }
        }
    }
    Ok(bars.into_values().collect())
}

fn write_event(writer: &mut impl Write, event: &InputEvent) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, event)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn quote(
    id: InstrumentId,
    at: u64,
    mid: Price,
    tick: Price,
    half_spread_fraction: Decimal,
    depth: Quantity,
) -> anyhow::Result<InputEvent> {
    let tick = tick.as_decimal();
    // Explicit OHLC-path model: fixed displayed depth, never future minute volume.
    let half = mid.as_decimal() * half_spread_fraction;
    let bid = ((mid.as_decimal() - half) / tick).floor() * tick;
    let ask = ((mid.as_decimal() + half) / tick).ceil() * tick;
    Ok(InputEvent::Quote {
        symbol: id,
        timestamp: at.into(),
        available_at: at.into(),
        bid: Price::from_decimal(bid)?,
        ask: Price::from_decimal(ask)?,
        bid_size: depth,
        ask_size: depth,
    })
}

/// The plan fixes dates and selection policy; each flat session carries forward realized equity.
pub(super) fn replay(
    root: &Path,
    destination: &Path,
    dates: Option<&DateRange>,
) -> anyhow::Result<()> {
    let mut plan: HistoryPlan = serde_json::from_slice(&fs::read(root.join("plan.json"))?)?;
    if let Some(dates) = dates {
        super::select_date_range(&mut plan.strategy, dates)?;
    }
    plan.strategy.validate()?;
    anyhow::ensure!(
        plan.starting_equity > Decimal::ZERO
            && plan.spread_bps > Decimal::ZERO
            && plan.spread_bps < Decimal::from(1000)
            && plan.opening_spread_multiplier >= Decimal::ONE
            && plan.opening_spread_multiplier <= Decimal::from(10)
            && plan.quote_depth.as_decimal() > Decimal::ZERO
            && (0.0..=1.0).contains(&plan.slippage_probability),
        "invalid history execution assumptions"
    );
    let selected = plan
        .selections
        .iter()
        .filter(|s| {
            plan.strategy.trading_start <= s.session_open
                && s.session_open < plan.strategy.trading_end
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !selected.is_empty(),
        "no ranking sessions in requested interval"
    );
    // No partial download can silently masquerade as the complete requested backtest.
    let required = selected
        .iter()
        .flat_map(|s| s.candidates.iter().copied())
        .chain(plan.strategy.regime_benchmarks().iter().copied())
        .collect::<BTreeSet<_>>();
    for id in &required {
        let marker = root
            .join("minute")
            .join(id.to_string())
            .join("complete.json");
        let covered: Session = serde_json::from_slice(
            &fs::read(&marker).with_context(|| format!("candidate history incomplete: {id}"))?,
        )?;
        anyhow::ensure!(
            plan.ranges.get(id) == Some(&covered),
            "history range differs from selection plan: {id}"
        );
    }
    let mut daily = Vec::new();
    for id in plan.strategy.instrument_ids() {
        for path in [
            plan.daily_cache_dir.join(format!("{id}.json")),
            root.join("ranking-gap").join(format!("{id}.json")),
        ] {
            if !path.exists() {
                continue;
            }
            let bars: Vec<Bar> = serde_json::from_slice(&fs::read(path)?)?;
            daily.extend(bars);
        }
    }
    daily.sort_by_key(|b| (b.ts_event, b.bar_type));
    let work = root.join(format!("replay-{}", std::process::id()));
    fs::create_dir(&work)?;
    let mut equity = plan.starting_equity;
    let mut combined = serde_json::json!({"trades": [], "signals": [], "observations": [], "rankings": [], "selections": [], "equity": [], "rejected": {}, "entry_rejected": {}, "errors": [], "turnover": "0"});
    let mut day_results = Vec::new();
    let mut missing = Vec::new();
    let mut coverage = Vec::new();
    let mut normalizations = BTreeMap::new();
    let mut invalid_prices = BTreeMap::new();
    for selection in selected {
        let index = plan
            .strategy
            .sessions
            .iter()
            .position(|s| s.open == selection.session_open)
            .context("unknown session")?;
        let session = plan.strategy.sessions[index];
        if selection.ranking.ranks.is_empty() {
            missing.push(session.open);
            continue;
        }
        let mut selection = selection.clone();
        selection.candidates.retain(|id| {
            let percentile = selection.ranking.ranks[id].percentile;
            plan.strategy.directions.iter().any(|side| match side {
                TradeSide::Long => percentile >= plan.strategy.momentum.min_percentile,
                TradeSide::Short => percentile <= 100.0 - plan.strategy.momentum.min_percentile,
            })
        });
        let active = selection
            .candidates
            .iter()
            .copied()
            .chain(plan.strategy.regime_benchmarks().iter().copied())
            .collect::<BTreeSet<_>>();
        let from = plan.strategy.sessions[index.saturating_sub(12)].open;
        let mut warmup = Vec::new();
        let mut intraday = BTreeMap::<UnixNanos, Vec<Bar>>::new();
        for id in &active {
            let precision = plan
                .price_increments
                .get(id)
                .with_context(|| format!("missing price increment for {id}"))?
                .precision;
            for bar in minute_rows(
                root,
                *id,
                from,
                session.close,
                precision,
                &mut normalizations,
                &mut invalid_prices,
            )? {
                if bar.ts_event <= session.open {
                    warmup.push(bar);
                } else {
                    intraday.entry(bar.ts_event).or_default().push(bar);
                }
            }
        }
        let mut counts = BTreeMap::<InstrumentId, usize>::new();
        for bar in intraday.values().flatten() {
            *counts.entry(bar.bar_type.instrument_id()).or_default() += 1;
        }
        for id in &active {
            let observed = counts.get(id).copied().unwrap_or(0);
            coverage.push(serde_json::json!({"symbol": id, "session_open": session.open, "expected_minutes": (session.close.as_u64() - session.open.as_u64()) / MINUTE, "observed_minutes": observed}));
            anyhow::ensure!(
                observed > 0,
                "no minute history for selected {id} on {}; history collection is incomplete",
                session.open
            );
        }
        let events = work.join("events.jsonl");
        let mut writer = std::io::BufWriter::new(fs::File::create(&events)?);
        let history_from = plan.strategy.sessions[index.saturating_sub(253)].open;
        for b in daily
            .iter()
            .filter(|b| history_from < b.ts_event && b.ts_event < session.open)
        {
            write_event(
                &mut writer,
                &InputEvent::Bar {
                    symbol: b.bar_type.instrument_id(),
                    timestamp: b.ts_event,
                    available_at: b.ts_event,
                    daily: true,
                    open: b.open,
                    high: b.high,
                    low: b.low,
                    close: b.close,
                    volume: b.volume,
                },
            )?;
        }
        write_event(
            &mut writer,
            &InputEvent::Market {
                update: MarketUpdate {
                    timestamp: selection.available_at,
                    selection: Some(selection),
                    warmup_symbols: active.clone(),
                    bars: warmup,
                },
            },
        )?;
        for (at, bars) in intraday {
            for bar in &bars {
                let id = bar.bar_type.instrument_id();
                let tick = plan.price_increments[&id];
                let start = at.as_u64() - MINUTE;
                let opening = start < session.open.as_u64() + 30 * MINUTE;
                // OHLC ordering is a simulation assumption, not historical ticks.
                for (offset, mid) in [
                    (10_000_000, bar.open),
                    (20_000_000_000, bar.low),
                    (40_000_000_000, bar.high),
                    (59_000_000_000, bar.close),
                ] {
                    write_event(
                        &mut writer,
                        &quote(
                            id,
                            start + offset,
                            mid,
                            tick,
                            plan.spread_bps
                                * if opening {
                                    plan.opening_spread_multiplier
                                } else {
                                    Decimal::ONE
                                }
                                / Decimal::from(20_000),
                            plan.quote_depth,
                        )?,
                    )?;
                }
            }
            write_event(
                &mut writer,
                &InputEvent::Market {
                    update: MarketUpdate {
                        timestamp: at,
                        selection: None,
                        warmup_symbols: BTreeSet::new(),
                        bars,
                    },
                },
            )?;
        }
        writer.flush()?;
        let mut c = plan.strategy.clone();
        c.trading_start = session.open;
        c.trading_end = session.close;
        let input = RunInput {
            synthetic: true,
            provenance: format!(
                "{} Actual Longbridge OHLC with modeled O-L-H-C quotes; configured spread (wider first 30 minutes), rounded outward to ticks, fixed configured depth, native probabilistic one-tick slippage and configured commissions; shortability assumed, intraday-flat borrow interest modeled as zero. Daily engines must end flat before carrying equity.",
                plan.provenance
            ),
            strategy: c,
            starting_equity: equity,
            price_increments: plan.price_increments.clone(),
            events_path: "events.jsonl".to_string(),
            spread_multiplier: Decimal::ONE,
            slippage_probability: Some(plan.slippage_probability),
        };
        let input_path = work.join("input.json");
        let output_path = work.join("output.json");
        fs::write(&input_path, serde_json::to_vec(&input)?)?;
        let log = fs::File::create(work.join("engine.log"))?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .arg(&input_path)
            .arg(&output_path)
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()?;
        anyhow::ensure!(
            status.success(),
            "native daily replay failed; inspect {}",
            work.join("engine.log").display()
        );
        let result: serde_json::Value = serde_json::from_slice(&fs::read(&output_path)?)?;
        let report = &result["report"];
        equity = report["equity"]
            .as_array()
            .and_then(|a| a.last())
            .and_then(|v| v["equity"].as_str())
            .context("missing final daily equity")?
            .parse()?;
        let turnover: Decimal = combined["turnover"]
            .as_str()
            .context("invalid turnover")?
            .parse()?;
        let daily_turnover: Decimal = report["turnover"]
            .as_str()
            .context("invalid daily turnover")?
            .parse()?;
        combined["turnover"] = serde_json::to_value(turnover + daily_turnover)?;
        for key in ["trades", "signals", "equity", "errors"] {
            combined[key]
                .as_array_mut()
                .context("invalid report array")?
                .extend(
                    report[key]
                        .as_array()
                        .context("missing native report array")?
                        .iter()
                        .cloned(),
                );
        }
        for key in ["rejected", "entry_rejected"] {
            for (reason, count) in report[key].as_object().context("missing rejections")? {
                let previous = combined[key][reason].as_u64().unwrap_or(0);
                combined[key][reason] =
                    (previous + count.as_u64().context("invalid rejection count")?).into();
            }
        }
        super::print_trade_logs(report)?;
        day_results.push(serde_json::json!({"session_open": session.open, "equity": equity, "net_pnl": result["net_pnl"], "trades": report["trades"].as_array().map_or(0, Vec::len)}));
        println!(
            "history session {} equity={} trades={}",
            session.open,
            equity,
            report["trades"].as_array().map_or(0, Vec::len)
        );
        // Only these files were created by this invocation; raw and cached history stays intact.
        for name in ["input.json", "output.json", "events.jsonl", "engine.log"] {
            fs::remove_file(work.join(name))?;
        }
    }
    fs::remove_dir(work)?;
    let complete_minute_prices = invalid_prices.is_empty()
        && coverage
            .iter()
            .all(|row| row["observed_minutes"] == row["expected_minutes"]);
    let output = serde_json::json!({"execution_assumptions": {"spread_bps": plan.spread_bps, "opening_spread_multiplier": plan.opening_spread_multiplier, "quote_depth": plan.quote_depth, "slippage_probability": plan.slippage_probability, "intraday_borrow_interest": "0", "shortability_verified": false, "path": "O-L-H-C"}, "ohlc_envelope_normalizations": normalizations, "discarded_nonpositive_ohlc": invalid_prices, "complete_minute_prices": complete_minute_prices, "minute_coverage": coverage, "configuration": plan.strategy, "starting_equity": plan.starting_equity, "synthetic": true, "provenance": plan.provenance, "report": combined, "daily": day_results, "missing_ranking_sessions": missing, "complete_period": missing.is_empty(), "remaining_positions": 0, "remaining_orders": 0, "net_pnl": equity - plan.starting_equity});
    fs::write(destination, serde_json::to_vec_pretty(&output)?)?;
    anyhow::ensure!(
        missing.is_empty(),
        "ranking data missing for {} sessions; report is partial",
        missing.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn test_minute_envelope_matches_longbridge_without_changing_source() {
        let root = tempfile::tempdir().unwrap();
        let id = InstrumentId::from("AAA.SIM");
        let directory = root.path().join("minute").join(id.to_string());
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("page.csv");
        let raw = "0,99.600,99.430,98.950,99.430,1859\n60,21.640,21.685,21.665,21.665,668\n120,10.00,11.00,9.00,12.00,5\n180,10.00,11.004,9.004,10.50,5\n240,0,0,0,0,70\n";
        fs::write(&path, raw).unwrap();
        let mut normalizations = BTreeMap::new();
        let mut invalid_prices = BTreeMap::new();
        let bars = minute_rows(
            root.path(),
            id,
            0.into(),
            (5 * MINUTE).into(),
            2,
            &mut normalizations,
            &mut invalid_prices,
        )
        .unwrap();
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[0].high, Price::from("99.600"));
        assert_eq!(bars[1].low, Price::from("21.640"));
        assert_eq!(bars[2].high, Price::from("12.00"));
        assert_eq!(bars[3].high, Price::from("11.00"));
        assert_eq!(bars[3].low, Price::from("9.00"));
        assert_eq!(bars[0].open.precision, 2);
        assert_eq!(
            invalid_prices[&id],
            [(5 * MINUTE).into()].into_iter().collect()
        );
        assert_eq!(
            normalizations[&id],
            [MINUTE.into(), (2 * MINUTE).into(), (3 * MINUTE).into()]
                .into_iter()
                .collect()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), raw);
    }
}
