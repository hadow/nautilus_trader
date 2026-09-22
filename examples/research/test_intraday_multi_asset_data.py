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

"""Checks date alignment without changing any cached prices or volumes."""

import csv
import json
import tempfile
import unittest
from datetime import datetime
from pathlib import Path
from unittest.mock import patch
from zoneinfo import ZoneInfo

from intraday_multi_asset_data import align


class AlignmentTest(unittest.TestCase):
    def test_shared_calendar_preserves_bars_and_rejects_incomplete_session(self):
        source = (
            Path(__file__).resolve().parents[2]
            / "tests/data/intraday_momentum/spy_intraday_golden.csv"
        )
        with source.open() as stream:
            days = sorted({int(row["session_open"]) for row in csv.DictReader(stream)})
        boundary = (
            datetime.fromtimestamp(days[15] // 10**9, ZoneInfo("America/New_York"))
            .date()
            .isoformat()
        )
        with tempfile.TemporaryDirectory() as directory, patch("builtins.print"):
            root = Path(directory)
            duplicate = root / "QQQ.csv"
            duplicate.write_bytes(source.read_bytes())
            sparse = root / "DIA.csv"
            lines = source.read_bytes().splitlines(keepends=True)
            sparse.write_bytes(lines[0] + b"".join(lines[1 + 2 * 390 :]))
            output = root / "aligned"
            align({"SPY": source, "QQQ": duplicate, "DIA": sparse}, output, boundary)
            for symbol in ("SPY", "QQQ"):
                self.assertEqual(
                    (output / f"{symbol}.csv").read_bytes(), source.read_bytes()
                )
            metadata = json.loads((output / "manifest.json").read_text())
            self.assertEqual(metadata["train_sessions"], 15)
            self.assertEqual(metadata["test_sessions"], 15)
            self.assertEqual(metadata["primary_symbols"], ["SPY", "QQQ"])
            self.assertEqual(metadata["sparse_symbols"], ["DIA"])
            self.assertEqual((output / "DIA.csv").read_bytes(), sparse.read_bytes())
            align(
                {"AAPL": source, "JPM": duplicate},
                root / "stocks",
                boundary,
                reference="AAPL",
            )
            stock_metadata = json.loads((root / "stocks/manifest.json").read_text())
            self.assertEqual(stock_metadata["reference_symbol"], "AAPL")
            self.assertEqual(stock_metadata["files"]["JPM"]["coverage_vs_aapl"], 1.0)
            duplicate.write_bytes(
                b"\n".join(duplicate.read_bytes().splitlines()[:-1]) + b"\n"
            )
            with self.assertRaisesRegex(ValueError, "complete sessions: QQQ"):
                align({"SPY": source, "QQQ": duplicate}, output, boundary)


if __name__ == "__main__":
    unittest.main()
