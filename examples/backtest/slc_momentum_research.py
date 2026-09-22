# -------------------------------------------------------------------------------------------------
#  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
#  https://nautechsystems.io
#
#  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
#  You may not use this file except in compliance with the License.
#  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
#
#  Unless required by applicable law or agreed to in writing, software
#  distributed under the License is distributed on an "AS IS" BASIS,
#  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#  See the License for the specific language governing permissions and limitations under the License.
# -------------------------------------------------------------------------------------------------
"""Run native SLC experiments and summarize evidence without fitting on the test interval."""

import argparse
import copy
import hashlib
import json
import math
import shutil
import statistics
import subprocess
from collections import defaultdict
from datetime import UTC
from datetime import datetime
from datetime import timedelta
from decimal import Decimal
from itertools import pairwise
from pathlib import Path
from zoneinfo import ZoneInfo


MINUTE = 60_000_000_000
VARIANTS = [
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "SLC_ONLY",
    "WITHOUT_STOCHASTIC",
    "WITHOUT_REGIME",
]


def ratio(numerator: float, denominator: float) -> float | None:
    """Return null for an undefined ratio."""
    return numerator / denominator if denominator else None


def trade_metrics(trades: list[dict]) -> dict:
    """Summarize net trade outcomes; do not annualize irregular trade returns."""
    pnls = [float(t["pnl"]) for t in trades]
    rs = [float(t["r_multiple"]) for t in trades]
    winners = [p for p in pnls if p > 0]
    losers = [p for p in pnls if p < 0]
    return {
        "trades": len(trades),
        "win_rate": ratio(len(winners), len(trades)),
        "profit_factor": ratio(sum(winners), -sum(losers)),
        "expectancy": statistics.mean(pnls) if pnls else None,
        "average_r": statistics.mean(rs) if rs else None,
        "median_r": statistics.median(rs) if rs else None,
        "average_winner": statistics.mean(winners) if winners else None,
        "average_loser": statistics.mean(losers) if losers else None,
        "largest_winner": max(winners, default=None),
        "largest_loser": min(losers, default=None),
        "net_pnl": sum(pnls),
    }


def metrics(result: dict) -> dict:
    """Compute daily-equity risk metrics, retaining flat trading days."""
    config = result["configuration"]
    start, end = config["trading_start"], config["trading_end"]
    rows = [r for r in result["report"]["equity"] if start <= r["timestamp"] <= end]
    daily = {}
    for row in rows:
        daily[row["session_open"]] = float(row["equity"])
    sessions = [s for s in config["sessions"] if start <= s["open"] < end]
    equity = float(result["starting_equity"])
    values = [equity]
    returns = []
    missing = []
    for session in sessions:
        if session["open"] not in daily:
            missing.append(session["open"])
            continue
        current = daily[session["open"]]
        returns.append(current / equity - 1 if equity else 0.0)
        values.append(current)
        equity = current
    peak = values[0]
    drawdown = 0.0
    for value in [values[0], *[float(row["equity"]) for row in rows]]:
        peak = max(peak, value)
        drawdown = max(drawdown, 1 - value / peak)
    years = (end - start) / (365.25 * 24 * 60 * MINUTE)
    cagr = (values[-1] / values[0]) ** (1 / years) - 1 if years > 0 and values[-1] > 0 else None
    deviation = statistics.stdev(returns) if len(returns) > 1 else 0.0
    mean = statistics.mean(returns) if returns else 0.0
    downside = math.sqrt(statistics.mean([min(r, 0) ** 2 for r in returns])) if returns else 0.0
    observed_seconds = exposure_seconds = weighted_exposure = 0.0
    bounds = {s["open"]: s["close"] for s in config["sessions"]}
    for previous, current in pairwise(rows):
        if previous["session_open"] != current["session_open"]:
            continue
        # A final minute can be delivered by the next session's first watermark.
        # Exclude the overnight interval from intraday exposure denominators.
        seconds = (
            max(
                0,
                min(current["timestamp"], bounds[previous["session_open"]])
                - max(previous["timestamp"], previous["session_open"]),
            )
            / 1e9
        )
        observed_seconds += seconds
        exposure_seconds += seconds if float(previous["exposure"]) > 0 else 0
        weighted_exposure += seconds * float(previous["exposure"]) / float(previous["equity"])
    summary = {
        **trade_metrics(result["report"]["trades"]),
        "cagr": cagr if returns and not missing else None,
        "sharpe": ratio(mean * math.sqrt(252), deviation) if not missing else None,
        "sortino": ratio(mean * math.sqrt(252), downside) if not missing else None,
        "max_drawdown": drawdown,
        "calmar": ratio(cagr, drawdown) if cagr is not None and returns and not missing else None,
        "time_exposure": ratio(exposure_seconds, observed_seconds),
        "average_notional_exposure": ratio(weighted_exposure, observed_seconds),
        "turnover": float(result["report"]["turnover"]) / statistics.mean(values),
        "missing_equity_sessions": missing,
        "synthetic": result["synthetic"],
        "alpha_verified": False,
        "account_rounding_delta": str(
            Decimal(rows[-1]["equity"])
            - Decimal(result["starting_equity"])
            - sum((Decimal(t["pnl"]) for t in result["report"]["trades"]), Decimal(0))
        )
        if rows
        else None,
    }
    cohorts: dict[str, dict[str, list]] = defaultdict(lambda: defaultdict(list))
    for trade in result["report"]["trades"]:
        s = trade["signal"]
        local = datetime.fromtimestamp(trade["opened_at"] / 1e9, UTC).astimezone(
            ZoneInfo("America/New_York")
        )
        atr = s["momentum"]["atr_fraction"]
        labels = {
            "side": s.get("side", "LONG"),
            "market_regime": s["market_regime"],
            "setup_type": s["setup_type"],
            "time_of_day": f"{local.hour:02}:{local.minute // 30 * 30:02}",
            "symbol": s["symbol"],
            "sector": s["sector"],
            "momentum_percentile": str(int(s["momentum"]["percentile"] // 10) * 10),
            "confirmation_score": str(s["confirmation_score"]),
            "volatility": "HIGH" if atr >= 0.04 else "LOW" if atr < 0.02 else "NORMAL",
        }
        for dimension, label in labels.items():
            cohorts[dimension][label].append(trade)
    summary["cohorts"] = {
        dimension: {label: trade_metrics(ts) for label, ts in groups.items()}
        for dimension, groups in cohorts.items()
    }
    return summary


def run_native(binary: Path, config: dict, directory: Path, label: str) -> dict:
    """Run one immutable configuration through the native BacktestEngine."""
    source = directory / f"{label}.input.json"
    output = directory / f"{label}.json"
    source.write_text(json.dumps(config, indent=2))
    with (directory / f"{label}.log").open("w") as log:
        subprocess.run(
            [str(binary.resolve()), str(source), str(output)],
            check=True,
            stdout=log,
            stderr=log,
        )
    result = json.loads(output.read_text())
    validate_execution(result)
    summary = metrics(result)
    (directory / f"{label}.metrics.json").write_text(json.dumps(summary, indent=2, allow_nan=False))
    return summary


def validate_execution(result: dict) -> None:
    """Audit temporal ordering, exact cash reconciliation and completed order lifecycles."""
    if result["remaining_positions"] or result["remaining_orders"] or result["report"]["errors"]:
        raise ValueError("unresolved execution state")
    closed_by_symbol = {}
    net = Decimal(0)
    rounding_bound = Decimal(0)
    for trade in sorted(result["report"]["trades"], key=lambda t: t["opened_at"]):
        signal = trade["signal"]
        if (
            not signal["momentum"]["daily_cutoff"]
            < signal["momentum"]["timestamp"]
            <= signal["timestamp"]
            <= signal["available_at"]
            < trade["opened_at"]
            <= trade["closed_at"]
        ):
            raise ValueError("future data or same-event entry in trade record")
        symbol = signal["symbol"]
        if trade["opened_at"] <= closed_by_symbol.get(symbol, 0):
            raise ValueError("overlapping position for one symbol")
        closed_by_symbol[symbol] = trade["closed_at"]
        if not Decimal(0) < Decimal(trade["quantity"]) <= Decimal(trade["allocation"]["quantity"]):
            raise ValueError("filled quantity outside allocation")
        side = trade["signal"].get("side", "LONG")
        if side not in {"LONG", "SHORT"}:
            raise ValueError("unsupported trade direction")
        direction = Decimal(-1) if side == "SHORT" else Decimal(1)
        cash = direction * (Decimal(trade["exit_value"]) - Decimal(trade["entry_value"])) - Decimal(trade["fees"])
        if cash != Decimal(trade["pnl"]) or Decimal(trade["initial_risk"]) <= 0:
            raise ValueError("trade cash or initial risk does not reconcile")
        # Native margin/position accounting rounds Money at each reduction fill; the
        # executable-price cash ledger does not round an average opening price.
        bound = Decimal("0.01") * trade.get("exit_fill_count", trade.get("sell_fill_count", 0))
        if abs(Decimal(trade["native_booked_pnl"]) - cash) > bound:
            raise ValueError("native trade booking exceeds per-fill Money rounding bound")
        rounding_bound += bound
        net += cash
    rows = result["report"]["equity"]
    if (
        rows
        and abs(Decimal(rows[-1]["equity"]) - Decimal(result["starting_equity"]) - net)
        > rounding_bound
    ):
        raise ValueError("account equity does not reconcile to completed net trades")


def sensitivity_configs(config: dict) -> list[tuple[str, dict]]:
    """Return predeclared neighbors, changing one parameter family at a time."""
    dimensions = [
        ("momentum", "lookbacks", [[1, 4, 8, 16], [1, 6, 12, 24]]),
        ("momentum", "return_weights", [[0, 1, 0.8, 1.2], [0, 1, 1.2, 0.8]]),
        ("slc", "stochastic_k", [4, 6]),
        ("slc", "stochastic_d", [2, 4]),
        ("risk", "stop_buffer_atr", ["0.15", "0.25"]),
        ("risk", "risk_per_trade", ["0.004", "0.006"]),
        ("slc", "max_level_age_bars", [20, 28]),
        ("slc", "max_level_tests", [1, 2]),
        ("slc", "confirmation_threshold", [8.0, 10.0]),
        ("slc", "minimum_confirmation_volume", [1.0, 1.4]),
    ]
    neighbors = []
    for section, key, values in dimensions:
        for index, value in enumerate(values):
            neighbor = copy.deepcopy(config)
            neighbor["strategy"].setdefault(section, {})[key] = value
            neighbors.append((f"{key}_{index}", neighbor))
    for index, window in enumerate([[[5, 105]], [[10, 120]], [[5, 120], [270, 375]]]):
        neighbor = copy.deepcopy(config)
        neighbor["strategy"]["trading_windows"] = window
        neighbors.append((f"window_{index}", neighbor))
    return neighbors


def walk_forward(
    binary: Path, config: dict, output: Path, lengths: tuple[int, int, int]
) -> list[dict]:
    """Select on validation, then evaluate each disjoint sealed test interval once."""
    train, validation, test = lengths
    if min(lengths) <= 0:
        raise ValueError("walk-forward lengths must be positive")
    sessions = [
        s
        for s in config["strategy"]["sessions"]
        if s["open"] >= config["strategy"]["trading_start"]
        and s["close"] <= config["strategy"]["trading_end"]
    ]
    folds = []
    for start in range(0, len(sessions) - sum(lengths) + 1, test):
        fold = len(folds)
        candidates = [("base", copy.deepcopy(config))]
        candidates += sensitivity_configs(config)[:2]
        train_results, validation_results = {}, {}
        for label, candidate in candidates:
            candidate["strategy"]["trading_start"] = sessions[start]["open"]
            candidate["strategy"]["trading_end"] = sessions[start + train - 1]["close"]
            train_results[label] = run_native(binary, candidate, output, f"wf{fold}_train_{label}")
            candidate["strategy"]["trading_start"] = sessions[start + train]["open"]
            candidate["strategy"]["trading_end"] = sessions[start + train + validation - 1]["close"]
            validation_results[label] = run_native(
                binary, candidate, output, f"wf{fold}_validation_{label}"
            )
        eligible = [
            label
            for label, result in validation_results.items()
            if result["trades"] >= 5
            and result["sharpe"] is not None
            and not result["missing_equity_sessions"]
        ]
        # Insufficient validation evidence cannot justify tuning. Still test the predeclared
        # baseline out of sample, so the split remains observable without inventing a winner.
        winner = (
            max(eligible, key=lambda label: (validation_results[label]["sharpe"], label))
            if eligible
            else "base"
        )
        selected = copy.deepcopy(dict(candidates)[winner])
        selected["strategy"]["trading_start"] = sessions[start + train + validation]["open"]
        selected["strategy"]["trading_end"] = sessions[start + sum(lengths) - 1]["close"]
        result = run_native(binary, selected, output, f"wf{fold}_test")
        folds.append(
            {
                "fold": fold,
                "winner": winner,
                "status": "VALIDATION_SELECTED"
                if eligible
                else "BASELINE_ONLY_INSUFFICIENT_VALIDATION_TRADES",
                "train": train_results,
                "validation": validation_results,
                "test": result,
            }
        )
    return folds


def synthetic_candle(base: Decimal, index: int, minute: int) -> tuple:
    """Construct a known impulse/pullback path for execution regression checks."""
    phase = minute % 60
    wave = Decimal(phase if phase <= 15 else 30 - phase if phase <= 40 else phase - 50) / 25
    opening = base + Decimal(minute) / 500 + wave
    closing = opening + (Decimal("0.06") if phase < 15 or phase > 40 else Decimal("-0.04"))
    volume = 50000
    if index < 5 and 41 <= minute <= 45:
        # Deliberately constructed displacement, then a later pullback/reentry.
        # This tests the SLC path; it is not sampled market evidence.
        opening = base - Decimal("0.36") + Decimal(minute - 41) * Decimal("0.36")
        closing = opening + Decimal("0.36")
        volume = 150000
    if index < 5 and 96 <= minute <= 105:
        volume = 300000
    if index < 5 and 106 <= minute <= 110:
        volume = 150000
    if index < 5 and minute == 110:
        closing = base + Decimal("0.22")
    low, high = (
        min(opening, closing) - Decimal("0.03"),
        max(opening, closing) + Decimal("0.03"),
    )
    return opening, high, low, closing, volume


def synthetic_fixture(directory: Path) -> Path:
    """Create deterministic invented market data for engineering checks only."""
    symbols = [f"{s}.SIM" for s in ["AAA", "BBB", "CCC", "DDD", "EEE", "SPY", "QQQ", "IWM", "XLK"]]
    candidates = symbols[:5]
    sessions = []
    date = datetime(2025, 1, 2, 9, 30, tzinfo=ZoneInfo("America/New_York"))
    while len(sessions) < 40:
        if date.weekday() < 5 and date.date().isoformat() not in {
            "2025-01-20",
            "2025-02-17",
        }:
            opened = int(date.timestamp()) * 1_000_000_000
            sessions.append({"open": opened, "close": opened + 390 * MINUTE})
        date += timedelta(days=1)
    metadata = [
        {
            "instrument_id": symbol,
            "sector": "SYNTHETIC_TECH",
            "sector_etf": "XLK.SIM",
            "market_cap": "2000000000",
            "effective_from": 0,
            "effective_until": 2**64 - 1,
            "known_at": 0,
        }
        for symbol in candidates
    ]
    config = {
        "synthetic": True,
        "provenance": "Invented deterministic engineering fixture; no historical return or alpha evidence. OHLC-derived quotes use explicit adverse spread; native fill model adds impact/slippage.",
        "events_path": "events.jsonl",
        "starting_equity": "100000",
        "price_increments": dict.fromkeys(symbols, "0.01"),
        "strategy": {
            "universe": metadata,
            "benchmarks": symbols[5:8],
            "sessions": sessions,
            "trading_start": sessions[30]["open"],
            "trading_end": sessions[-1]["close"],
            "dry_run": False,
            "momentum": {
                "minimum_market_cap": "1000000000",
                "minimum_average_dollar_volume": "1000000",
                "minimum_atr_fraction": 0.001,
                "maximum_gap_fraction": 0.5,
                "min_percentile": 60.0,
            },
            "slc": {
                "htf_minutes": 30,
                "ema_fast": 3,
                "ema_slow": 5,
                "require_htf_ema": False,
            },
            "risk": {"participation": "0.02", "max_sector_positions": 2},
            "max_slippage_bps": "30",
            "max_spread_bps": "20",
        },
    }
    events = directory / "events.jsonl"
    with events.open("w") as stream:
        for day, session in enumerate(sessions):
            for index, symbol in enumerate(symbols):
                base = Decimal(50 + index * 10) + Decimal(day) * Decimal(index + 1) / 5
                daily_open, daily_high, daily_low, daily_close = base, base + 2, base - 1, base + 1
                daily_volume = 0 if day >= 25 else 10000000
                if day >= 25:
                    for minute in range(1, 391):
                        opening, high, low, closing, volume = synthetic_candle(base, index, minute)
                        if minute == 1:
                            daily_open, daily_high, daily_low = opening, high, low
                        daily_high, daily_low = max(daily_high, high), min(daily_low, low)
                        daily_close, daily_volume = closing, daily_volume + volume
                        ts = session["open"] + minute * MINUTE

                        def price(value: Decimal) -> str:
                            return str(value.quantize(Decimal("0.01")))

                        for offset, mid in [
                            (0, opening),
                            (20_000_000_000, low),
                            (40_000_000_000, high),
                            (59_000_000_000, closing),
                        ]:
                            quote_ts = ts - MINUTE + offset
                            # Opening spread stress is based solely on time, not future volume
                            half_spread = Decimal("0.03") if minute <= 30 else Decimal("0.01")
                            stream.write(
                                json.dumps(
                                    {
                                        "kind": "quote",
                                        "symbol": symbol,
                                        "timestamp": quote_ts,
                                        "available_at": quote_ts,
                                        "bid": price(mid - half_spread),
                                        "ask": price(mid + half_spread),
                                        "bid_size": "10000",
                                        "ask_size": "10000",
                                    }
                                )
                                + "\n"
                            )
                        stream.write(
                            json.dumps(
                                {
                                    "kind": "bar",
                                    "symbol": symbol,
                                    "timestamp": ts,
                                    "available_at": ts,
                                    "daily": False,
                                    "open": price(opening),
                                    "high": price(high),
                                    "low": price(low),
                                    "close": price(closing),
                                    "volume": str(volume),
                                }
                            )
                            + "\n"
                        )
                stream.write(
                    json.dumps(
                        {
                            "kind": "bar",
                            "symbol": symbol,
                            "timestamp": session["close"],
                            "available_at": session["close"],
                            "daily": True,
                            "open": str(daily_open.quantize(Decimal("0.01"))),
                            "high": str(daily_high.quantize(Decimal("0.01"))),
                            "low": str(daily_low.quantize(Decimal("0.01"))),
                            "close": str(daily_close.quantize(Decimal("0.01"))),
                            "volume": str(daily_volume),
                        }
                    )
                    + "\n"
                )
    path = directory / "fixture.json"
    path.write_text(json.dumps(config, indent=2))
    return path


def chart(result: dict, events: Path, destination: Path) -> None:
    """Render the Rust report with the repository's existing Plotly dependency."""
    import plotly.graph_objects as go
    from plotly.subplots import make_subplots

    report = result["report"]
    observations = report.get("observations", [])
    focus = (report["signals"] or observations or [None])[0]
    if focus is None:
        destination.write_text(
            "<html lang='en'><title>SLC evidence</title><p>No observations in this run.</p></html>"
        )
        return
    symbol = focus["symbol"]
    start, end = focus["timestamp"] - 90 * MINUTE, focus["timestamp"] + 120 * MINUTE
    with events.open() as stream:
        bars = [
            b
            for line in stream
            if (b := json.loads(line))["kind"] == "bar"
            and not b["daily"]
            and b["symbol"] == symbol
            and start <= b["timestamp"] <= end
        ]

    def x(ts: int) -> datetime:
        return (
            datetime.fromtimestamp(ts / 1e9, UTC)
            .astimezone(ZoneInfo("America/New_York"))
            .replace(tzinfo=None)
        )

    fig = make_subplots(
        rows=3,
        cols=1,
        shared_xaxes=True,
        row_heights=[0.65, 0.17, 0.18],
        vertical_spacing=0.06,
        subplot_titles=["Price and SLC levels", "Volume", "Momentum score / percentile"],
    )
    fig.add_trace(
        go.Candlestick(
            x=[x(b["timestamp"]) for b in bars],
            open=[float(b["open"]) for b in bars],
            high=[float(b["high"]) for b in bars],
            low=[float(b["low"]) for b in bars],
            close=[float(b["close"]) for b in bars],
            name="1m OHLC",
        ),
        row=1,
        col=1,
    )
    fig.add_trace(
        go.Bar(
            x=[x(b["timestamp"]) for b in bars],
            y=[float(b["volume"]) for b in bars],
            name="Volume",
            marker_color="#94a3b8",
        ),
        row=2,
        col=1,
    )
    visible = [o for o in observations if o["symbol"] == symbol and start <= o["timestamp"] <= end]
    fig.add_trace(
        go.Scatter(
            x=[x(o["timestamp"]) for o in visible],
            y=[o["vwap"] for o in visible],
            name="VWAP",
            line_color="#2563eb",
            customdata=[
                [o["regime"], o["structure"], o["stochastic_k"], o["stochastic_d"]] for o in visible
            ],
            hovertemplate="VWAP %{y:.2f}<br>Market %{customdata[0]}<br>HTF %{customdata[1]}<br>K/D %{customdata[2]:.1f}/%{customdata[3]:.1f}<extra></extra>",
        ),
        row=1,
        col=1,
    )
    for o in visible:
        for key, color in [("demand", "#16a34a"), ("supply", "#ea580c")]:
            for low, high in o[key]:
                fig.add_shape(
                    type="rect",
                    x0=x(o["timestamp"]),
                    x1=x(min(end, o["timestamp"] + 5 * MINUTE)),
                    y0=float(low),
                    y1=float(high),
                    fillcolor=color,
                    opacity=0.18,
                    line_width=0,
                    row=1,
                    col=1,
                )
    ranks = [
        r["ranks"][symbol]
        for r in report["rankings"]
        if symbol in r["ranks"] and start <= r["timestamp"] <= end
    ]
    for field in ["score", "percentile"]:
        fig.add_trace(
            go.Scatter(
                x=[x(r["timestamp"]) for r in ranks],
                y=[r[field] for r in ranks],
                name=f"Momentum {field}",
            ),
            row=3,
            col=1,
        )
    for trade in report["trades"]:
        if trade["signal"]["symbol"] != symbol or not start <= trade["opened_at"] <= end:
            continue
        entry = float(Decimal(trade["entry_value"]) / Decimal(trade["quantity"]))
        exit_price = float(Decimal(trade["exit_value"]) / Decimal(trade["quantity"]))
        for label, price, color in [
            ("Entry fill average", entry, "#0f172a"),
            ("Initial stop", float(trade["allocation"]["stop"]), "#dc2626"),
            ("Target", float(trade["allocation"]["target"]), "#16a34a"),
        ]:
            fig.add_trace(
                go.Scatter(
                    x=[x(trade["opened_at"]), x(trade["closed_at"])],
                    y=[price, price],
                    name=label,
                    mode="lines",
                    line={"color": color, "dash": "dash"},
                ),
                row=1,
                col=1,
            )
        fig.add_trace(
            go.Scatter(
                x=[x(trade["opened_at"]), x(trade["closed_at"])],
                y=[entry, exit_price],
                name="Entry / exit",
                mode="markers",
                marker={"symbol": ["triangle-up", "x"], "size": 12},
                text=[trade["signal"]["setup_type"], trade["exit_reason"]],
                hovertemplate="%{text}<br>%{y:.2f}<extra></extra>",
            ),
            row=1,
            col=1,
        )
    fig.update_layout(
        title=f"{symbol} — synthetic={result['synthetic']} — engineering evidence only",
        template="plotly_white",
        height=950,
        hovermode="x unified",
        legend={"orientation": "h"},
    )
    fig.update_xaxes(range=[x(start), x(end)], rangeslider_visible=False)
    fig.update_xaxes(title_text="America/New_York", row=3, col=1)
    fig.update_yaxes(range=[0, 100], row=3, col=1)
    fig.write_html(destination, include_plotlyjs=True, full_html=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path)
    parser.add_argument("--synthetic", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/examples/slc-momentum"))
    parser.add_argument("--ablation", action="store_true")
    parser.add_argument("--sensitivity", action="store_true")
    parser.add_argument("--walk-forward", action="store_true")
    parser.add_argument("--cost-stress", action="store_true")
    parser.add_argument("--fold-sessions", nargs=3, type=int, default=[60, 20, 20])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    frozen_binary = args.output / "slc-momentum"
    if args.binary.resolve() != frozen_binary.resolve():
        shutil.copy2(args.binary, frozen_binary)
    args.binary = frozen_binary
    source = synthetic_fixture(args.output) if args.synthetic else args.input
    if source is None:
        parser.error("provide --input or --synthetic")
    config = json.loads(source.read_text())
    config["events_path"] = str((source.parent / config["events_path"]).resolve())
    variants = VARIANTS if args.ablation else [config["strategy"].get("ablation", "F")]
    with args.binary.open("rb") as executable:
        comparison = {"binary_sha256": hashlib.file_digest(executable, "sha256").hexdigest()}
    for variant in variants:
        candidate = copy.deepcopy(config)
        candidate["strategy"]["ablation"] = variant
        comparison[variant] = run_native(args.binary, candidate, args.output, variant)
        print(
            variant,
            {
                k: comparison[variant][k]
                for k in [
                    "trades",
                    "sharpe",
                    "profit_factor",
                    "expectancy",
                    "max_drawdown",
                ]
            },
            flush=True,
        )
    if args.sensitivity:
        comparison["sensitivity"] = {
            label: run_native(args.binary, candidate, args.output, f"sensitivity_{label}")
            for label, candidate in sensitivity_configs(config)
        }
    if args.cost_stress:
        comparison["cost_stress"] = {}
        for multiplier in [2, 3]:
            stressed = copy.deepcopy(config)
            stressed["spread_multiplier"] = str(multiplier)
            risk = stressed["strategy"].setdefault("risk", {})
            risk["commission_rate"] = str(
                Decimal(str(risk.get("commission_rate", "0.0001"))) * multiplier
            )
            comparison["cost_stress"][str(multiplier)] = run_native(
                args.binary, stressed, args.output, f"cost_{multiplier}"
            )
    if args.walk_forward:
        comparison["walk_forward"] = walk_forward(
            args.binary, config, args.output, tuple(args.fold_sessions)
        )
    comparison["inference"] = (
        "Synthetic data validates engineering only. Historical attribution requires point-in-time data, cost stress, sealed out-of-sample tests and uncertainty estimates. No component alpha is established."
    )
    (args.output / "comparison.json").write_text(json.dumps(comparison, indent=2, allow_nan=False))
    result = json.loads(
        (args.output / f"{'F' if 'F' in variants else variants[0]}.json").read_text()
    )
    chart(result, Path(config["events_path"]), args.output / "chart.html")


if __name__ == "__main__":
    main()
