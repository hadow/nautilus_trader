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

"""Offline checks for symbol-isolated, resumable market-data caches."""

import gzip
import io
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from intraday_alpaca_download import download


def response(payload):
    stream = io.BytesIO(gzip.compress(json.dumps(payload).encode()))
    stream.headers = {"Content-Encoding": "gzip"}
    return stream


class DownloadTest(unittest.TestCase):
    def test_resume_and_cached_rerun_without_network(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch("shutil.disk_usage", return_value=SimpleNamespace(free=10 * 1024**3)),
        ):
            root = Path(directory)
            notebook = root / "reference.ipynb"
            notebook.write_text(
                json.dumps(
                    {
                        "cells": [
                            {},
                            {},
                            {},
                            {
                                "source": [
                                    "API_KEY = 'test-key'\nSECRET_KEY = 'test-secret'\n"
                                ]
                            },
                        ]
                    }
                )
            )
            output = root / "QQQ"
            page = {
                "symbol": "QQQ",
                "bars": [{"t": "2016-01-04T14:30:00Z"}],
                "next_page_token": "next",
            }
            with (
                patch(
                    "urllib.request.urlopen",
                    side_effect=[
                        response(page),
                        RuntimeError("interrupted"),
                    ],
                ),
                patch("time.sleep"),
                patch("builtins.print"),
                self.assertRaisesRegex(RuntimeError, "interrupted"),
            ):
                download(notebook, output, "QQQ")
            self.assertFalse(
                json.loads((output / "manifest.json").read_text())["complete"]
            )
            self.assertTrue((output / "page-0000.json.gz").exists())
            page["next_page_token"] = None
            with (
                patch(
                    "urllib.request.urlopen",
                    side_effect=[
                        response(page),
                        response([{"date": "2016-01-04"}]),
                    ],
                ) as fetch,
                patch("time.sleep"),
                patch("builtins.print"),
            ):
                download(notebook, output, "QQQ")
                self.assertEqual(fetch.call_count, 2)
                self.assertIn(
                    "page_token=next", fetch.call_args_list[0].args[0].full_url
                )
                self.assertIn("/QQQ/bars", fetch.call_args_list[0].args[0].full_url)
            with (
                patch(
                    "urllib.request.urlopen", side_effect=AssertionError("cache missed")
                ),
                patch("builtins.print"),
            ):
                download(notebook, output, "QQQ")
                with self.assertRaisesRegex(ValueError, "Cache request mismatch"):
                    download(notebook, output, "IWM")
                with self.assertRaisesRegex(ValueError, "Cache request mismatch"):
                    download(
                        notebook, output, "QQQ", start="2021-01-04", end="2026-01-01"
                    )
                with gzip.open(output / "page-0000.json.gz", "wt") as stream:
                    json.dump({"symbol": "IWM", "bars": []}, stream)
                with self.assertRaisesRegex(
                    RuntimeError, "Cached page symbol mismatch"
                ):
                    download(notebook, output, "QQQ")
            manifest = (output / "manifest.json").read_text()
            self.assertNotIn("test-key", manifest)
            self.assertNotIn("test-secret", manifest)
            with (
                patch("shutil.disk_usage", return_value=SimpleNamespace(free=0)),
                patch(
                    "urllib.request.urlopen",
                    side_effect=AssertionError("disk guard missed"),
                ),
                self.assertRaisesRegex(RuntimeError, "disk free below"),
            ):
                download(notebook, root / "DIA", "DIA")
            with (
                patch(
                    "urllib.request.urlopen",
                    side_effect=[
                        response(
                            {"symbol": "AAPL", "bars": [], "next_page_token": None}
                        ),
                        response([{"date": "2021-01-04"}]),
                    ],
                ) as fetch,
                patch("time.sleep"),
                patch("builtins.print"),
            ):
                download(
                    notebook,
                    root / "AAPL",
                    "AAPL",
                    start="2021-01-04",
                    end="2026-01-01",
                )
                self.assertIn(
                    "start=2021-01-04T00%3A00%3A00Z",
                    fetch.call_args_list[0].args[0].full_url,
                )
                self.assertIn(
                    "end=2026-01-01", fetch.call_args_list[1].args[0].full_url
                )
                with self.assertRaisesRegex(ValueError, "Start date"):
                    download(
                        notebook, root / "invalid", start="2026-01-01", end="2021-01-04"
                    )


if __name__ == "__main__":
    unittest.main()
