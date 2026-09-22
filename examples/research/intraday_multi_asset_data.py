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

"""Align cached CSVs for fixed-parameter Rust backtests; no strategy execution."""

import argparse
import hashlib
import json
import shutil
from collections import Counter
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo


def align(sources, output, boundary="2023-12-28", min_coverage=0.95, reference="SPY"):
    if reference not in sources or not 0 < min_coverage <= 1:
        raise ValueError("Reference symbol and coverage in (0, 1] required")
    sessions = {}
    for symbol, source in sources.items():
        with source.open("rb") as stream:
            next(stream)
            counts = Counter(int(row.split(b",", 3)[1]) for row in stream)
        if not counts or any(count != 390 for count in counts.values()):
            raise ValueError(f"Expected normalized complete sessions: {symbol}")
        sessions[symbol] = set(counts)
    coverage = {
        symbol: len(days & sessions[reference]) / len(sessions[reference])
        for symbol, days in sessions.items()
    }
    eligible = [symbol for symbol in sources if coverage[symbol] >= min_coverage]
    common = sorted(set.intersection(*(sessions[symbol] for symbol in eligible)))
    dates = {
        day: datetime.fromtimestamp(day // 10**9, ZoneInfo("America/New_York"))
        .date()
        .isoformat()
        for day in common
    }
    train = [day for day in common if dates[day] < boundary]
    test = [day for day in common if dates[day] >= boundary]
    if len(train) <= 14 or len(test) <= 14:
        raise ValueError("Both periods require more than 14 warmup sessions")
    output.mkdir(parents=True, exist_ok=True)
    files = {}
    for symbol, source in sources.items():
        chosen = set(common) if symbol in eligible else sessions[symbol]
        if shutil.disk_usage(output).free < 2 * 1024**3 + source.stat().st_size:
            raise RuntimeError("Insufficient disk for aligned CSV and 2 GiB reserve")
        target = output / f"{symbol}.csv"
        temporary = target.with_suffix(".tmp")
        source_hash = hashlib.sha256()
        output_hash = hashlib.sha256()
        count = 0
        with source.open("rb") as reader, temporary.open("wb") as writer:
            header = next(reader)
            writer.write(header)
            source_hash.update(header)
            output_hash.update(header)
            for line in reader:
                source_hash.update(line)
                if int(line.split(b",", 3)[1]) in chosen:
                    writer.write(line)
                    output_hash.update(line)
                    count += 1
        assert count == len(chosen) * 390
        temporary.replace(target)
        files[symbol] = {
            "source": str(source),
            "source_sha256": source_hash.hexdigest(),
            "source_sessions": len(sessions[symbol]),
            f"coverage_vs_{reference.lower()}": coverage[symbol],
            "comparison": "primary" if symbol in eligible else "sparse_diagnostic_only",
            "aligned": str(target),
            "aligned_sha256": output_hash.hexdigest(),
            "rows": count,
            "removed_by_intersection": len(sessions[symbol]) - len(chosen),
        }
    metadata = {
        "boundary": boundary,
        "reference_symbol": reference,
        "minimum_coverage": min_coverage,
        "primary_symbols": eligible,
        "sparse_symbols": [symbol for symbol in sources if symbol not in eligible],
        "train_start": dates[train[0]],
        "train_end": dates[train[-1]],
        "train_sessions": len(train),
        "train_evaluation_start": dates[train[14]],
        "test_start": dates[test[0]],
        "test_end": dates[test[-1]],
        "test_sessions": len(test),
        "test_evaluation_start": dates[test[14]],
        "warmup_per_period": 14,
        "sessions": list(dates.values()),
        "files": files,
        "limitation": "Ex-post intersection of complete 390-minute sessions, including warmup history; excludes half-days and incomplete days and is not a deployment calendar",
    }
    (output / "manifest.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps({k: v for k, v in metadata.items() if k != "sessions"}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--spy", type=Path)
    parser.add_argument("--reference", default="SPY")
    parser.add_argument("--boundary", default="2023-12-28")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--symbols", nargs="+", required=True)
    parser.add_argument("--min-coverage", type=float, default=0.95)
    args = parser.parse_args()
    sources = {"SPY": args.spy} if args.spy else {}
    sources.update(
        {symbol: args.cache / symbol / "bars.csv" for symbol in args.symbols}
    )
    align(sources, args.output, args.boundary, args.min_coverage, args.reference)
