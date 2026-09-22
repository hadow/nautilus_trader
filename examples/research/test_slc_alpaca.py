# Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
# Licensed under the GNU Lesser General Public License Version 3.0.

import json
import tempfile
import unittest
from pathlib import Path

import slc_alpaca


class SlcAlpacaTest(unittest.TestCase):
    def test_operational_profile_and_compact_projection_are_frozen(self):
        opened = 1_704_205_800_000_000_000
        closed = opened + slc_alpaca.MINUTE
        row = {
            "timestamp_ns": str(closed),
            "session_open": str(opened),
            "session_close": str(closed),
            "open": "100",
            "high": "102",
            "low": "99",
            "close": "101",
            "volume": "1000",
        }
        data = {symbol: {(opened, closed): [row]} for symbol in slc_alpaca.sources()}
        config = slc_alpaca.configuration(
            [(opened, closed)], [(opened, closed)], Path("events.jsonl"), "operational"
        )

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            slc_alpaca.write_events(
                path, data, [(opened, closed)], opened, compact=True
            )
            events = [json.loads(line) for line in path.read_text().splitlines()]

        quotes = [event for event in events if event["kind"] == "quote"]
        self.assertEqual(len(slc_alpaca.STOCKS), 30)
        self.assertEqual(len(set(slc_alpaca.UNIVERSE.values())), 11)
        self.assertEqual(len(quotes), 4 * len(slc_alpaca.STOCKS))
        self.assertTrue(
            all(
                event["symbol"].removesuffix(".SIM") in slc_alpaca.STOCKS
                for event in quotes
            )
        )
        self.assertEqual(config["strategy"]["slc"]["impulse_atr"], 1.0)
        self.assertFalse(config["strategy"]["slc"]["require_htf_ema"])
        self.assertFalse(config["strategy"]["slc"]["require_intraday_trend"])
        self.assertEqual(config["strategy"]["ablation"], "SLC_ONLY")
        self.assertEqual(config["strategy"]["momentum"]["min_percentile"], 90.0)

    def test_compact_projection_keeps_independently_missing_sessions(self):
        opened = 1_704_205_800_000_000_000
        closed = opened + slc_alpaca.MINUTE
        row = {
            "timestamp_ns": str(closed),
            "session_open": str(opened),
            "session_close": str(closed),
            "open": "100",
            "high": "102",
            "low": "99",
            "close": "101",
            "volume": "1000",
        }
        data = {symbol: {(opened, closed): [row]} for symbol in slc_alpaca.sources()}
        missing = slc_alpaca.STOCKS[0]
        data[missing] = {}

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            slc_alpaca.write_events(
                path, data, [(opened, closed)], opened, compact=True
            )
            events = [json.loads(line) for line in path.read_text().splitlines()]

        self.assertTrue(all(event["symbol"] != f"{missing}.SIM" for event in events))


if __name__ == "__main__":
    unittest.main()
