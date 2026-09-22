# Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
# Licensed under the GNU Lesser General Public License Version 3.0.
"""Prepare local Alpaca observations and run the existing native SLC BacktestEngine.

Python performs ingestion and report aggregation only. All signals, risk, orders,
fills and account state run in the Rust slc-momentum example.
"""

import argparse
import csv
import gzip
import hashlib
import importlib.util
import json
import shutil
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from copy import deepcopy
from datetime import UTC, datetime
from decimal import ROUND_CEILING, ROUND_FLOOR, Decimal
from functools import partial
from pathlib import Path
from zoneinfo import ZoneInfo

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "slc_research", ROOT / "examples/backtest/slc_momentum_research.py"
)
RESEARCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RESEARCH)
MINUTE = 60_000_000_000
UNIVERSE = {
    "AAPL": "technology",
    "MSFT": "technology",
    "NVDA": "technology",
    "GOOGL": "communication_services",
    "META": "communication_services",
    "DIS": "communication_services",
    "AMZN": "consumer_discretionary",
    "TSLA": "consumer_discretionary",
    "F": "consumer_discretionary",
    "WMT": "consumer_staples",
    "KO": "consumer_staples",
    "MO": "consumer_staples",
    "JPM": "financials",
    "BAC": "financials",
    "C": "financials",
    "MRK": "healthcare",
    "JNJ": "healthcare",
    "PFE": "healthcare",
    "BA": "industrials",
    "UBER": "industrials",
    "AAL": "industrials",
    "XOM": "energy",
    "CVX": "energy",
    "OXY": "energy",
    "FCX": "materials",
    "NEM": "materials",
    "O": "real_estate",
    "AMD": "technology",
    "NEE": "utilities",
    "PLTR": "technology",
}
STOCKS = list(UNIVERSE)
VARIANTS = {
    "slc_htf_v1": ("SWEEP_RECLAIM", "STOCHASTIC_REENTRY"),
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def sources():
    root = ROOT / "test_data/local/intraday_momentum"
    return {
        **{s: root / "slc_htf_30" / s / "bars.csv.gz" for s in STOCKS},
        "SPY": root / "spy_notebook_full.csv",
        **{s: root / "multi_asset" / s / "bars.csv" for s in ["QQQ", "IWM"]},
    }


def local_date(timestamp):
    return (
        datetime.fromtimestamp(timestamp / 1e9, UTC)
        .astimezone(ZoneInfo("America/New_York"))
        .date()
        .isoformat()
    )


def load_quarter(paths, year, quarter):
    start_month = quarter * 3 - 2
    start = int(datetime(year, start_month, 1, tzinfo=UTC).timestamp()) * 10**9
    end = (
        int(
            datetime(
                year + (quarter == 4), (start_month + 2) % 12 + 1, 1, tzinfo=UTC
            ).timestamp()
        )
        * 10**9
    )
    # At least 21 prior completed sessions for ranking/correlation and 5 for RVOL
    warm = start - 70 * 1440 * MINUTE
    data = {}
    excluded = {}
    for symbol, path in paths.items():
        days = {}
        with gzip.open(path, "rt") if path.suffix == ".gz" else path.open() as stream:
            for row in csv.DictReader(stream):
                at = int(row["timestamp_ns"])
                if not warm <= at < end:
                    continue
                opened, closed = int(row["session_open"]), int(row["session_close"])
                days.setdefault((opened, closed), []).append(row)
        valid = {}
        for session, rows in days.items():
            times = [int(r["timestamp_ns"]) for r in rows]
            expected = list(range(session[0] + MINUTE, session[1] + 1, MINUTE))
            if times == expected:
                valid[session] = rows
        data[symbol] = valid
        excluded[symbol] = [local_date(s[0]) for s in days if s not in valid]
    calendar = sorted(data["SPY"])
    trading = [s for s in calendar if s[0] >= start]
    if not trading or len([s for s in calendar if s[0] < start]) < 21:
        raise ValueError("Insufficient complete SPY warmup/trading sessions")
    coverage = {
        symbol: len(set(sessions) & set(calendar)) / len(calendar)
        for symbol, sessions in data.items()
    }
    # The universe is frozen at >=90% full-window coverage; this lower per-quarter
    # guard catches clustered outages without reselecting members each quarter.
    sparse = {
        symbol: ratio
        for symbol, ratio in coverage.items()
        if symbol in STOCKS and ratio < 0.8
    }
    if sparse:
        raise ValueError(
            f"Stock coverage below 80% of the quarterly SPY calendar: {sparse}"
        )
    return (
        data,
        calendar,
        trading,
        {
            "calendar": "SPY complete 390-minute sessions; symbols remain independently missing",
            "incomplete": excluded,
            "coverage": coverage,
            "missing_sessions": {
                symbol: [local_date(s[0]) for s in calendar if s not in sessions]
                for symbol, sessions in data.items()
            },
            "first": local_date(trading[0][0]),
            "last": local_date(trading[-1][0]),
            "sessions": len(trading),
        },
    )


def write_events(path, data, sessions, start, compact=False):
    def price(value):
        amount = Decimal(value)
        fixed = amount.quantize(Decimal("0.00000001"))
        if amount != fixed:
            raise ValueError("Price exceeds supported source precision")
        return str(fixed)

    def emit(stream, event):
        stream.write(json.dumps(event, separators=(",", ":")) + "\n")

    with path.open("w") as stream:
        for opened, closed in sessions:
            for symbol, days in data.items():
                sid = symbol + ".SIM"
                rows = days.get((opened, closed), [])
                if not rows:
                    continue
                for row in rows:
                    at = int(row["timestamp_ns"])
                    if opened >= start and (not compact or symbol in STOCKS):
                        # Fixed OHLC path is an explicit fill hypothesis, never real quotes.
                        # Diagnostic projections retain a second open observation.
                        path_points = [
                            (10_000_000, "open"),
                            (20_000_000_000, "low"),
                            (40_000_000_000, "high"),
                            (59_000_000_000, "close"),
                        ]
                        if not compact:
                            path_points.insert(1, (3_000_000_000, "open"))
                        for offset, field in path_points:
                            mid = Decimal(row[field])
                            half = (
                                mid
                                * Decimal("0.00005")
                                * (2 if at - opened <= 30 * MINUTE else 1)
                            )
                            tick = Decimal("0.01")
                            emit(
                                stream,
                                {
                                    "kind": "quote",
                                    "symbol": sid,
                                    "timestamp": at - MINUTE + offset,
                                    "available_at": at - MINUTE + offset,
                                    "bid": str(
                                        (mid - half).quantize(
                                            tick, rounding=ROUND_FLOOR
                                        )
                                    ),
                                    "ask": str(
                                        (mid + half).quantize(
                                            tick, rounding=ROUND_CEILING
                                        )
                                    ),
                                    "bid_size": "1000",
                                    "ask_size": "1000",
                                },
                            )
                    emit(
                        stream,
                        {
                            "kind": "bar",
                            "symbol": sid,
                            "timestamp": at,
                            "available_at": at,
                            "daily": False,
                            **{
                                k: price(row[k])
                                for k in ["open", "high", "low", "close"]
                            },
                            "volume": row["volume"],
                        },
                    )
                emit(
                    stream,
                    {
                        "kind": "bar",
                        "symbol": sid,
                        "timestamp": closed,
                        "available_at": closed,
                        "daily": True,
                        "open": price(rows[0]["open"]),
                        "close": price(rows[-1]["close"]),
                        "high": price(max(Decimal(r["high"]) for r in rows)),
                        "low": price(min(Decimal(r["low"]) for r in rows)),
                        "volume": str(sum(Decimal(r["volume"]) for r in rows)),
                    },
                )


def configuration(sessions, trading, event_path, profile):
    quote_path = "O-L-H-C" if profile == "operational" else "O-O-L-H-C"
    config = {
        "synthetic": True,
        "provenance": f"Actual cached Alpaca raw OHLC; SPY complete-session calendar with independently missing symbol sessions; synthetic {quote_path} quotes on candidates only, 1bp full spread (2bp opening), outward penny rounding, fixed depth1000, one-tick native slippage, 1bp commission each fill; shortability assumed, no borrow cost. Fixed 30-stock research universe; no cross-sectional selection or regime gate.",
        "events_path": str(event_path.resolve()),
        "starting_equity": "100000",
        "price_increments": {s + ".SIM": "0.01" for s in sources()},
        "slippage_probability": 1.0,
        "strategy": {
            "universe": [
                {
                    "instrument_id": s + ".SIM",
                    "sector": sector,
                    "sector_etf": "SPY.SIM",
                    # Positive placeholder satisfies the security-master invariant;
                    # the zero minimum below disables this unavailable factor.
                    "market_cap": "1",
                    "known_at": 0,
                    "effective_from": 0,
                    "effective_until": 2**64 - 1,
                }
                for s, sector in UNIVERSE.items()
            ],
            "benchmarks": [s + ".SIM" for s in ["SPY", "QQQ", "IWM"]],
            "sessions": [{"open": o, "close": c} for o, c in sessions],
            "trading_start": trading[0][0],
            "trading_end": trading[-1][1],
            "dry_run": False,
            "ablation": "SLC_ONLY",
            "directions": ["LONG", "SHORT"],
            "trading_windows": [[5, 120], [270, 375]],
            "momentum": {
                "minimum_market_cap": "0",
                "sector_weight": 0.0,
                "spy_weight": 1.0,
                "qqq_weight": 1.0,
                "min_percentile": 90.0,
                "minimum_universe_size": 2,
            },
            "slc": {
                "htf_minutes": 30,
                "structure_mode": "SWEEP_RECLAIM",
                "confirmation_mode": "STOCHASTIC_REENTRY",
                "require_htf_ema": False,
                "require_intraday_trend": False,
            },
            "risk": {
                "risk_per_trade": "0.0025",
                "max_daily_loss": "0.01",
                "max_positions": 3,
            },
        },
    }
    if profile == "operational":
        # Fixed from 2024Q1 rejection frequencies, without inspecting trade PnL.
        config["strategy"]["slc"].update(
            require_htf_ema=False,
            confirmation_window_bars=8,
            max_level_age_bars=48,
            max_level_tests=2,
            impulse_atr=1.0,
            impulse_volume=1.0,
            min_level_score=5.0,
            max_level_distance_atr=1.5,
            minimum_confirmation_volume=1.0,
        )
    return config


def compress(path):
    destination = path.with_suffix(path.suffix + ".gz")
    with (
        path.open("rb") as source,
        gzip.open(destination, "wb", compresslevel=6) as target,
    ):
        shutil.copyfileobj(source, target)
    with gzip.open(destination, "rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != digest(path):
            raise ValueError("Compression verification failed")
    path.unlink()


def execute_variant(binary, base, directory, period, item):
    variant, (structure, confirmation) = item
    if (directory / f"{variant}.json.gz").exists():
        return
    config = deepcopy(base)
    config["strategy"]["slc"].update(
        structure_mode=structure, confirmation_mode=confirmation
    )
    print(f"Running {period} {variant}", flush=True)
    RESEARCH.run_native(binary, config, directory, variant)
    compress(directory / f"{variant}.json")
    compress(directory / f"{variant}.log")


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    paths = sources()
    plan = {
        "sources": {s: {"path": str(p), "sha256": digest(p)} for s, p in paths.items()},
        "variants": VARIANTS,
        "years": args.years,
        "quarters": args.quarters,
        "upstream_commit": "b3a09cd8bad7497ffe76df8886db927e3c7199ed",
        "binary_sha256": digest(args.binary),
        "generator_sha256": digest(Path(__file__)),
        "profile": args.profile,
        "parameters_selected_by_pnl": False,
        "calibration": (
            "2024Q1 rejection frequencies only"
            if args.profile == "operational"
            else None
        ),
        "parallel_variants": args.workers,
    }
    plan_path = args.output / "plan.json"
    if (
        plan_path.exists()
        and json.loads(plan_path.read_text()) != plan
        and any(args.output.glob("*Q*/*.json.gz"))
    ):
        raise ValueError("Frozen plan differs; use a separate output directory")
    plan_path.write_text(json.dumps(plan, indent=2))
    for year in args.years:
        for quarter in args.quarters:
            directory = args.output / f"{year}Q{quarter}"
            directory.mkdir(exist_ok=True)
            if all((directory / f"{v}.json.gz").exists() for v in VARIANTS):
                continue
            if shutil.disk_usage(args.output).free < 900_000_000:
                raise RuntimeError(
                    "Insufficient free disk; raw caches remain untouched"
                )
            print(f"Preparing {year}Q{quarter}", flush=True)
            data, sessions, trading, coverage = load_quarter(paths, year, quarter)
            events = directory / "events.jsonl"
            if not events.exists():
                write_events(
                    events,
                    data,
                    sessions,
                    trading[0][0],
                    compact=args.profile == "operational",
                )
            del data
            coverage["events_sha256"] = digest(events)
            coverage_path = directory / "coverage.json"
            if (
                coverage_path.exists()
                and json.loads(coverage_path.read_text()) != coverage
            ):
                raise ValueError("Cached projection does not match current coverage")
            coverage_path.write_text(json.dumps(coverage, indent=2))
            base = configuration(sessions, trading, events, args.profile)

            execute = partial(
                execute_variant,
                args.binary,
                base,
                directory,
                f"{year}Q{quarter}",
            )
            with ThreadPoolExecutor(max_workers=args.workers) as executor:
                list(executor.map(execute, VARIANTS.items()))
            events.unlink()  # Only this reproducible temporary event projection is removed
    summarize(args.output)


def summarize(root):
    summaries = {}
    for variant in VARIANTS:
        merged = None
        equity = Decimal(100000)
        rejected = Counter()
        entry_rejected = Counter()
        for path in sorted(root.glob(f"*Q*/{variant}.json.gz")):
            with gzip.open(path, "rt") as stream:
                result = json.load(stream)
            RESEARCH.validate_execution(result)
            report = result["report"]
            # Quarterly accounts reset: stitch additive realized PnL, never pretend compounding.
            offset = equity - Decimal(result["starting_equity"])
            for row in report["equity"]:
                row["equity"] = str(Decimal(row["equity"]) + offset)
            equity = Decimal(report["equity"][-1]["equity"])
            rejected.update(report["rejected"])
            entry_rejected.update(report["entry_rejected"])
            if merged is None:
                merged = result
                merged["configuration"]["sessions"] = [
                    s
                    for s in result["configuration"]["sessions"]
                    if s["open"] >= result["configuration"]["trading_start"]
                ]
            else:
                for field in ["trades", "equity"]:
                    merged["report"][field].extend(report[field])
                merged["report"]["turnover"] = str(
                    Decimal(merged["report"]["turnover"]) + Decimal(report["turnover"])
                )
                merged["configuration"]["sessions"].extend(
                    s
                    for s in result["configuration"]["sessions"]
                    if s["open"] >= result["configuration"]["trading_start"]
                )
                merged["configuration"]["trading_end"] = result["configuration"][
                    "trading_end"
                ]
        if merged is None:
            continue
        summary = RESEARCH.metrics(merged)
        summary.update(
            final_equity=str(equity),
            rejected=dict(rejected),
            entry_rejected=dict(entry_rejected),
            account_model="Independent quarterly 100k accounts; additive diagnostic equity",
        )
        summaries[variant] = summary
    (root / "summary.json").write_text(json.dumps(summaries, indent=2, allow_nan=False))
    print(
        json.dumps(
            {
                v: {k: m[k] for k in ["trades", "net_pnl", "sharpe", "max_drawdown"]}
                for v, m in summaries.items()
            },
            indent=2,
        ),
        flush=True,
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary", type=Path, default=ROOT / "target/debug/slc-momentum-backtest"
    )
    parser.add_argument("--output", type=Path, default=ROOT / "reports/slc_htf")
    parser.add_argument("--years", nargs="+", type=int, default=[2024, 2025])
    parser.add_argument(
        "--quarters", nargs="+", type=int, choices=[1, 2, 3, 4], default=[1, 2, 3, 4]
    )
    parser.add_argument(
        "--profile", choices=["diagnostic", "operational"], default="diagnostic"
    )
    parser.add_argument("--workers", type=int, choices=[1, 2], default=2)
    parser.add_argument("--summary-only", action="store_true")
    args = parser.parse_args()
    if args.summary_only:
        summarize(args.output)
    else:
        run(args)
