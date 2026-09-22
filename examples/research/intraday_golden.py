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
#  See the License for the specific language governing permissions and
#  limitations under the License.
# -------------------------------------------------------------------------------------------------

"""Offline-only Notebook oracle. Never imported or invoked by a trading executable."""

import argparse
import ast
import gzip
import hashlib
import json
from decimal import ROUND_FLOOR, Decimal
from pathlib import Path

import numpy as np
import pandas as pd


def generate(notebook, raw_directory, calendar, output, source="longbridge", days=30):
    notebook_bytes = notebook.read_bytes()
    cells = json.loads(notebook_bytes)["cells"]
    namespace = {"pd": pd, "np": np}
    # Only audited pure function definitions; never execute credential/download cells.
    for index in (25, 56, 57):
        tree = ast.parse("".join(cells[index]["source"]))
        if any(not isinstance(node, ast.FunctionDef) for node in tree.body):
            raise ValueError(f"Unexpected executable code in cell {index}")
        exec(compile(tree, f"notebook-cell-{index}", "exec"), namespace)  # noqa: S102 - audited user-supplied reference functions, offline only
    records = []
    paths = sorted(
        raw_directory.glob("page-*.json.gz" if source == "alpaca" else "*.json")
    )
    for path in paths:
        if source == "alpaca":
            with gzip.open(path, "rt") as stream:
                records.extend(json.load(stream)["bars"] or [])
            if len(records) > days * 1200:
                break
        else:
            data = json.loads(path.read_text())
            if not isinstance(data, list):
                raise ValueError(f"Invalid API response: {path}")
            records.extend(data)
    names = (
        {
            "t": "timestamp",
            "o": "open",
            "h": "high",
            "l": "low",
            "c": "close",
            "v": "volume",
            "vw": "vwap",
        }
        if source == "alpaca"
        else {"time": "timestamp"}
    )
    df = pd.DataFrame(records).rename(columns=names)
    df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True).dt.tz_convert(
        "America/New_York"
    )
    df = df.sort_values("timestamp").drop_duplicates("timestamp")
    df = df[
        (df.timestamp.dt.time >= pd.Timestamp("09:30").time())
        & (df.timestamp.dt.time <= pd.Timestamp("15:59").time())
    ].copy()
    for column in ("open", "high", "low", "close", "volume"):
        df[column] = pd.to_numeric(df[column])
    if source == "longbridge":
        df["turnover"] = pd.to_numeric(df.turnover)
        df["vwap"] = np.where(df.volume > 0, df.turnover / df.volume, df.close)
    df["date"] = df.timestamp.dt.date
    df["timestamp_ns"] = df.timestamp.dt.as_unit("ns").astype("int64") + 60_000_000_000
    sessions = json.loads(calendar.read_text())
    by_open = {s["open"]: s for s in sessions}
    kept = []
    used_sessions = []
    for date, group in df.groupby("date"):
        opening = int(pd.Timestamp(str(date) + " 09:30", tz="America/New_York").value)
        session = by_open.get(opening)
        if session is None and source == "alpaca":
            closing = int(
                pd.Timestamp(str(date) + " 16:00", tz="America/New_York").value
            )
            session = {"open": opening, "close": closing}
        if session is None:
            raise ValueError(f"Session missing from provider calendar: {date}")
        # Exact Notebook compatibility: only complete full sessions enter its history.
        if session["close"] - session["open"] != 390 * 60_000_000_000:
            continue
        expected = np.arange(
            opening + 60_000_000_000, session["close"] + 1, 60_000_000_000
        )
        if source == "alpaca" and len(group) != 390:
            continue
        if len(group) != 390 or not np.array_equal(group.timestamp_ns.values, expected):
            raise ValueError(f"Incomplete session: {date}, {len(group)} rows")
        group = group.copy()
        group["session_open"] = opening
        group["session_close"] = session["close"]
        kept.append(group)
        used_sessions.append(session)
        if len(used_sessions) == days:
            break
    df = pd.concat(kept).reset_index(drop=True)
    df["minute_index"] = df.groupby("date").cumcount()
    df["price_volume"] = df.vwap * df.volume
    df["vwap_anchored"] = (
        df.groupby("date").price_volume.cumsum() / df.groupby("date").volume.cumsum()
    )
    signals = namespace["generate_signals_rvol"](df.copy(), 14, 1.0)
    signals = namespace["execute_strategy_rvol"](signals, rvol_threshold=1.0)
    output.mkdir(parents=True, exist_ok=True)
    columns = [
        "timestamp_ns",
        "session_open",
        "session_close",
        "open",
        "high",
        "low",
        "close",
        "volume",
        "vwap",
    ]
    df[columns].to_csv(
        output / "spy_intraday_golden.csv", index=False, float_format="%.12g"
    )
    feature_columns = [
        "timestamp_ns",
        "sigma",
        "upper_bound",
        "lower_bound",
        "vwap_anchored",
        "rvol",
        "prev_close",
    ]
    signals[feature_columns].to_csv(
        output / "expected_features.csv", index=False, float_format="%.16g"
    )
    signals[["timestamp_ns", "raw_signal", "position"]].to_csv(
        output / "expected_signals.csv", index=False
    )
    # The literal Notebook return series intentionally preserves both documented leakage bugs.
    literal_returns = namespace["calculate_pnl"](
        signals.copy(), df.copy(), target_vol=0.03
    )
    literal_returns.rename("return").to_csv(
        output / "expected_notebook_returns.csv",
        index_label="date",
        float_format="%.16g",
    )
    # Corrected session-completion timing, identical Notebook entry/stop priority.
    production = namespace["generate_signals_rvol"](df.copy(), 14, 1.0)
    production["minute_index"] += 1
    production = namespace["execute_strategy_rvol"](production, rvol_threshold=1.0)
    # Production never opens at the closing boundary; Notebook still assigns raw_signal there.
    production.loc[production.minute_index == 390, "raw_signal"] = 0
    production[["timestamp_ns", "raw_signal", "position"]].to_csv(
        output / "expected_production_signals.csv", index=False
    )
    # Expected fills use next-minute open, including the explicit pre-close flatten.
    closes = df.groupby("date").close.last()
    historical_vol = closes.pct_change().rolling(14).std().shift(1)
    aum = Decimal(100000)
    fills = []
    daily = []
    for date, day in production.groupby("date"):
        vol = historical_vol.loc[date]
        lev = 1.0 if pd.isna(vol) else min(4.0, 0.03 / vol) if vol > 0 else 4.0
        qty = int(
            (
                aum * Decimal(str(lev)) / Decimal(str(day.iloc[0].open))
            ).to_integral_value(rounding=ROUND_FLOOR)
        )
        position = 0
        cash = aum
        rows = list(day.itertuples())
        for index, row in enumerate(rows[:-1]):
            target = 0 if index == len(rows) - 2 else int(row.position)
            if target == position:
                continue
            price = Decimal(str(rows[index + 1].open))
            # A reversal is two independent fills in the Nautilus netting strategy.
            for signed_quantity in ([-position * qty] if position else []) + (
                [target * qty] if target else []
            ):
                side = 1 if signed_quantity > 0 else -1
                commission = (
                    Decimal(abs(signed_quantity)) * Decimal("0.0045")
                ).quantize(Decimal("0.01"))
                cash -= signed_quantity * price + commission
                fills.append(
                    [
                        int(row.timestamp_ns),
                        side,
                        abs(signed_quantity),
                        price,
                        commission,
                        lev,
                    ]
                )
            position = target
        assert position == 0
        daily.append([str(date), cash / aum - 1, cash])
        aum = cash
    pd.DataFrame(
        fills,
        columns=["signal_timestamp", "side", "quantity", "price", "cost", "leverage"],
    ).to_csv(output / "expected_trades.csv", index=False, float_format="%.16g")
    pd.DataFrame(daily, columns=["date", "return", "equity"]).to_csv(
        output / "expected_production_returns.csv", index=False, float_format="%.16g"
    )
    (output / "sessions.json").write_text(json.dumps(used_sessions, indent=2) + "\n")
    metadata = {
        "source": "Alpaca SPY raw bars, provider minute VWAP"
        if source == "alpaca"
        else "Longbridge SPY.US raw bars; VWAP = turnover/volume",
        "same_download_settings_as_notebook": source == "alpaca",
        "original_csv_byte_identity_verified": False,
        "historical_download_may_include_provider_revisions": True,
        "calendar": "Notebook full-day 390-bar compatibility selection; not production holiday calendar"
        if source == "alpaca"
        else str(calendar),
        "notebook_sha256": hashlib.sha256(notebook_bytes).hexdigest(),
        "oracle_cells": [25, 56, 57],
        "sessions": len(used_sessions),
        "rows": len(df),
        "start": str(df.date.min()),
        "end": str(df.date.max()),
        "literal_notebook_returns_are_noncausal": True,
        "production_execution": "next bar open; last-minute open flatten; prior daily volatility; fixed shares",
        "files": {
            p.name: hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(output.glob("*.csv"))
        },
    }
    (output / "provenance.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--notebook", type=Path, required=True)
    parser.add_argument("--raw-directory", type=Path, required=True)
    parser.add_argument("--calendar", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source", choices=["alpaca", "longbridge"], default="alpaca")
    parser.add_argument("--days", type=int, default=30)
    args = parser.parse_args()
    generate(
        args.notebook,
        args.raw_directory,
        args.calendar,
        args.output,
        args.source,
        args.days,
    )
