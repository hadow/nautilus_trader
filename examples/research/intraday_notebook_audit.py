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

"""Offline data normalization and exact original Notebook train/test returns."""

import argparse
import ast
import csv
import gzip
import json
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

import numpy as np
import pandas as pd


def normalize(cache, output):
    manifest = json.loads((cache / "manifest.json").read_text())
    if not manifest["complete"]:
        raise ValueError("Incomplete history download")
    calendar = {d["date"]: d for d in json.loads((cache / "calendar.json").read_text())}
    timezone = ZoneInfo("America/New_York")
    output.parent.mkdir(parents=True, exist_ok=True)
    days = []
    current = None
    group = []
    skipped = []
    with output.open("w", newline="") as stream:
        writer = csv.writer(stream, lineterminator="\n")
        writer.writerow(
            [
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
        )

        def flush(date, rows):
            if date is None:
                return
            if len(rows) != 390:
                skipped.append({"date": date, "rows": len(rows)})
                return
            session = calendar[date]
            opening = (
                int(
                    datetime.fromisoformat(date + "T" + session["open"])
                    .replace(tzinfo=timezone)
                    .timestamp()
                )
                * 10**9
            )
            closing = (
                int(
                    datetime.fromisoformat(date + "T" + session["close"])
                    .replace(tzinfo=timezone)
                    .timestamp()
                )
                * 10**9
            )
            if closing - opening != 390 * 60 * 10**9:
                raise ValueError(f"390 rows disagree with exchange calendar {date}")
            if [r[0] for r in rows] != list(
                range(opening + 60 * 10**9, closing + 1, 60 * 10**9)
            ):
                raise ValueError(f"Incomplete chronology {date}")
            for r in rows:
                writer.writerow([r[0], opening, closing, *r[1:]])
            days.append(date)

        for path in sorted(cache.glob("page-*.json.gz")):
            with gzip.open(path, "rt") as stream:
                rows = json.load(stream)["bars"] or []
            for r in rows:
                timestamp = datetime.fromisoformat(
                    r["t"].replace("Z", "+00:00")
                ).astimezone(timezone)
                minute = timestamp.hour * 60 + timestamp.minute
                if not 570 <= minute < 960:
                    continue
                date = timestamp.date().isoformat()
                if date != current:
                    flush(current, group)
                    current = date
                    group = []
                group.append(
                    [
                        int(timestamp.timestamp()) * 10**9 + 60 * 10**9,
                        r["o"],
                        r["h"],
                        r["l"],
                        r["c"],
                        r["v"],
                        r["vw"],
                    ]
                )
        flush(current, group)
    split = int(len(days) * 0.8)
    metadata = {
        "sessions": len(days),
        "rows": len(days) * 390,
        "train_sessions": split,
        "test_sessions": len(days) - split,
        "train_start": days[0],
        "train_end": days[split - 1],
        "test_start": days[split],
        "test_end": days[-1],
        "excluded_days": skipped,
        "source_manifest": manifest,
        "calendar": "Alpaca historical trading calendar",
        "selection": "Notebook cell 13: exactly 390 regular-window rows per date",
    }
    output.with_suffix(".json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(
        {
            k: v
            for k, v in metadata.items()
            if k not in ("excluded_days", "source_manifest")
        },
        flush=True,
    )
    return metadata


def audit(notebook, data, output, metadata):
    namespace = {"pd": pd, "np": np}
    cells = json.loads(notebook.read_text())["cells"]
    for index in (18, 21, 25, 38, 56, 57):
        tree = ast.parse("".join(cells[index]["source"]))
        if any(not isinstance(n, ast.FunctionDef) for n in tree.body):
            raise ValueError("Only pure function definitions allowed")
        exec(compile(tree, f"cell-{index}", "exec"), namespace)  # noqa: S102 - audited user-supplied reference functions, offline only
    df = pd.read_csv(data)
    df["timestamp"] = pd.to_datetime(
        df.timestamp_ns - 60 * 10**9, utc=True
    ).dt.tz_convert("America/New_York")
    df["date"] = df.timestamp.dt.date
    df["minute_index"] = df.groupby("date").cumcount()
    df["price_volume"] = df.vwap * df.volume
    df["vwap_anchored"] = (
        df.groupby("date").price_volume.cumsum() / df.groupby("date").volume.cumsum()
    )
    days = sorted(df.date.unique())
    split = int(len(days) * 0.8)
    results = {}
    output.mkdir(parents=True, exist_ok=True)
    for label, selected in [("train", days[:split]), ("test", days[split:])]:
        raw = df[df.date.isin(selected)].copy()
        signals = namespace["generate_signals_rvol"](raw.copy(), 14, 1.0)
        trades = namespace["execute_strategy_rvol"](signals, rvol_threshold=1.0)
        returns = namespace["calculate_pnl"](trades, raw, target_vol=0.03)
        results[label] = namespace["calculate_metrics"](returns)
        results[label]["total_return"] = float((1 + returns).prod() - 1)
        results[label]["days"] = len(returns)
        returns.rename("return").to_csv(
            output / f"notebook_{label}_returns.csv", index_label="date"
        )
        print(label, results[label], flush=True)
    results["status"] = (
        "Literal Notebook behavior, NONCAUSAL; not deployable performance"
    )
    (output / "notebook_audit.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--notebook", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--data", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    metadata = normalize(args.cache, args.data)
    audit(args.notebook, args.data, args.output, metadata)
