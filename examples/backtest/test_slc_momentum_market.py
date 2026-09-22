# Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
# Licensed under the GNU Lesser General Public License Version 3.0.
"""Native engine check with invented global selection packets; no market-return evidence."""

import json
import shutil
import subprocess
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from examples.backtest.slc_momentum_research import synthetic_fixture

MINUTE = 60_000_000_000
ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / "target/debug/examples/slc-momentum"


def market_fixture(directory: Path) -> Path:
    source = synthetic_fixture(directory)
    app = json.loads(source.read_text())
    config = app["strategy"]
    session = config["sessions"][31]
    opening, closing = session["open"], session["close"]
    original = {m["instrument_id"] for m in config["universe"]}
    prototype = config["universe"][0]
    for index in range(3000 - len(original)):
        member = dict(prototype, instrument_id=f"Z{index:04}.SIM")
        config["universe"].append(member)
        app["price_increments"][member["instrument_id"]] = "0.01"
    ids = [m["instrument_id"] for m in config["universe"]]
    config["market"] = {
        "universe_size": 3000,
        "top_fraction": 0.10,
        "max_candidates": 300,
        "max_snapshot_age_minutes": 390,
    }
    config["momentum"]["min_percentile"] = 90
    config["momentum"]["minimum_universe_size"] = 500
    config["trading_start"], config["trading_end"] = opening, closing

    # Each packet is deliberately invented. The Rust ranker tests independently verify ranking.
    def selection(at: int, retained: bool) -> dict:
        ordered = [i for i in ids if i != "EEE.SIM"]
        ordered.insert(len(ordered) if retained else 0, "EEE.SIM")
        ranks = {}
        for index, symbol in enumerate(ordered):
            percentile = 100 * index / (len(ordered) - 1)
            ranks[symbol] = {
                "symbol": symbol,
                "timestamp": at,
                "daily_cutoff": config["sessions"][30]["close"],
                "rank": len(ordered) - index,
                "percentile": percentile,
                "score": percentile,
                "returns": [0.01, 0.05, 0.1, 0.2],
                "relative_strength": [0.1, 0.1, 0.1],
                "relative_volume": 2.0,
                "average_dollar_volume": "100000000",
                "average_volume": "1000000",
                "atr_fraction": 0.02,
                "gap": 0.0,
            }
        return {
            "session_open": opening,
            "available_at": at,
            "valid_until": closing,
            "universe_size": 3000,
            "observed_size": 3000,
            "ranking": {"timestamp": at, "ranks": ranks, "excluded": {}},
            "candidates": [i for i, r in ranks.items() if r["percentile"] >= 90],
        }

    def native_bar(event: dict) -> dict:
        return {
            "bar_type": f"{event['symbol']}-1-{'DAY' if event['daily'] else 'MINUTE'}-LAST-EXTERNAL",
            **{k: event[k] for k in ["open", "high", "low", "close", "volume"]},
            "ts_event": event["timestamp"],
            "ts_init": event["available_at"],
        }

    warmup, history, events, bars = [], [], [], {}
    active = set(config["benchmarks"]) | {"EEE.SIM"}
    with (directory / app["events_path"]).open() as stream:
        for line in stream:
            event = json.loads(line)
            at = event["available_at"]
            if at < opening and event["kind"] == "bar":
                if event["daily"] or event["symbol"] in config["benchmarks"]:
                    history.append(event)
                elif event["symbol"] == "EEE.SIM":
                    warmup.append(native_bar(event))
            elif opening <= at <= closing:
                if event["kind"] == "quote":
                    events.append(event)
                elif event["symbol"] in active and not event["daily"]:
                    bars.setdefault(at, []).append(native_bar(event))
    first = selection(opening + 1, True)
    events.append(
        {
            "kind": "market",
            "update": {
                "timestamp": opening + 1,
                "selection": first,
                "warmup_symbols": first["candidates"],
                "bars": warmup,
            },
        }
    )
    # EEE is still held when its ranking drops out: existing protective orders must survive.
    rotation = opening + 120 * MINUTE + 1
    events.append(
        {
            "kind": "market",
            "update": {
                "timestamp": rotation,
                "selection": selection(rotation, False),
                "warmup_symbols": [],
                "bars": [],
            },
        }
    )
    for at, batch in bars.items():
        events.append(
            {
                "kind": "market",
                "update": {
                    "timestamp": at,
                    "selection": None,
                    "warmup_symbols": [],
                    "bars": batch,
                },
            }
        )
    events.sort(
        key=lambda e: (
            e["update"]["timestamp"] if e["kind"] == "market" else e["available_at"]
        )
    )
    path = directory / "market-events.jsonl"
    with path.open("w") as output:
        for event in history + events:
            output.write(json.dumps(event, separators=(",", ":")) + "\n")
    app["events_path"] = path.name
    app["provenance"] = (
        "Invented 3000-member selection and candle fixture; verifies native routing/rotation, not alpha or broker fills."
    )
    source = directory / "market.json"
    source.write_text(json.dumps(app))
    return source


@unittest.skipUnless(BINARY.exists(), "build the native slc-momentum example first")
class FullMarketNativeTest(unittest.TestCase):
    def test_rotation_keeps_protection_and_global_membership(self):
        with tempfile.TemporaryDirectory(prefix="slc-market-") as temp:
            directory = Path(temp)
            source = market_fixture(directory)
            output = directory / "result.json"
            result = subprocess.run(
                [
                    str(BINARY),
                    str(source),
                    str(output),
                    "--start",
                    "2025-02-18",
                    "--end",
                    "2025-02-18",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr[-4000:])
            report = json.loads(output.read_text())
            self.assertEqual(report["remaining_positions"], 0)
            self.assertEqual(report["remaining_orders"], 0)
            self.assertEqual(report["report"]["errors"], [])
            snapshots = report["report"]["selections"]
            self.assertEqual(len(snapshots), 2)
            self.assertTrue(all(len(s["ranking"]["ranks"]) == 3000 for s in snapshots))
            self.assertTrue(all(len(s["candidates"]) == 300 for s in snapshots))
            self.assertNotIn("EEE.SIM", snapshots[1]["candidates"])
            trades = report["report"]["trades"]
            self.assertEqual(len(trades), 1)
            self.assertEqual(trades[0]["signal"]["symbol"], "EEE.SIM")
            self.assertLess(trades[0]["opened_at"], snapshots[1]["available_at"])
            self.assertGreater(trades[0]["closed_at"], snapshots[1]["available_at"])
            self.assertEqual(trades[0]["exit_reason"], "PROTECTIVE_STOP")
            self.assertTrue(
                all(
                    s["momentum"]["percentile"] >= 90
                    for s in report["report"]["signals"]
                )
            )
            # Expiring the selection before the setup must prevent the otherwise identical trade.
            lines = (directory / "market-events.jsonl").read_text().splitlines()
            modified = []
            for line in lines:
                event = json.loads(line)
                if event["kind"] == "market" and event["update"]["selection"]:
                    event["update"]["selection"]["valid_until"] = (
                        event["update"]["timestamp"] + MINUTE
                    )
                modified.append(json.dumps(event, separators=(",", ":")))
            (directory / "market-events.jsonl").write_text("\n".join(modified) + "\n")
            result = subprocess.run(
                [str(BINARY), str(source), str(output)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr[-4000:])
            expired = json.loads(output.read_text())
            self.assertEqual(expired["report"]["signals"], [])
            self.assertEqual(expired["report"]["trades"], [])

    def test_pending_entry_is_canceled_on_rotation_without_another_quote(self):
        with tempfile.TemporaryDirectory(prefix="slc-market-pending-") as temp:
            directory = Path(temp)
            source = market_fixture(directory)
            app = json.loads(source.read_text())
            # Use the native stop entry to leave an untriggered order pending. The example's
            # synthetic depth replenishes on matching passes, so ask_size cannot force a partial.
            app["strategy"]["entry_mode"] = "STOP"
            source.write_text(json.dumps(app))
            config = app["strategy"]
            first_quote = config["trading_start"] + 110 * MINUTE + 20_000_000_000
            rotation = first_quote + 10_000_000_000
            path = directory / "market-events.jsonl"
            events = [json.loads(line) for line in path.read_text().splitlines()]
            signal_close = next(
                Decimal(bar["close"])
                for event in events
                if event["kind"] == "market"
                for bar in event["update"]["bars"]
                if bar["bar_type"] == "EEE.SIM-1-MINUTE-LAST-EXTERNAL"
                and bar["ts_event"] == config["trading_start"] + 110 * MINUTE
            )
            modified = []
            for event in events:
                if event["kind"] == "quote" and event["symbol"] == "EEE.SIM":
                    if event["timestamp"] == first_quote:
                        event["ask"] = str(signal_close - Decimal("0.02"))
                        event["bid"] = str(signal_close - Decimal("0.04"))
                    elif first_quote < event["timestamp"] < rotation + MINUTE:
                        continue
                if event["kind"] == "market" and event["update"]["selection"]:
                    selection = event["update"]["selection"]
                    if "EEE.SIM" not in selection["candidates"]:
                        event["update"]["timestamp"] = rotation
                        selection["available_at"] = rotation
                        selection["ranking"]["timestamp"] = rotation
                        for rank in selection["ranking"]["ranks"].values():
                            rank["timestamp"] = rotation
                modified.append(event)
            modified.sort(
                key=lambda e: (
                    e["update"]["timestamp"]
                    if e["kind"] == "market"
                    else e["available_at"]
                )
            )
            path.write_text(
                "\n".join(json.dumps(e, separators=(",", ":")) for e in modified) + "\n"
            )
            output = directory / "result.json"
            result = subprocess.run(
                [str(BINARY), str(source), str(output)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr[-4000:])
            report = json.loads(output.read_text())
            self.assertEqual(report["report"]["errors"], [])
            self.assertEqual(report["remaining_positions"], 0)
            self.assertEqual(report["remaining_orders"], 0)
            self.assertEqual(len(report["report"]["signals"]), 1)
            self.assertEqual(report["report"]["trades"], [])
            # With identical quotes but no candidate removal, the pending stop must fill.
            unchanged = [
                event
                for event in modified
                if not (
                    event["kind"] == "market"
                    and event["update"]["selection"]
                    and "EEE.SIM" not in event["update"]["selection"]["candidates"]
                )
            ]
            path.write_text("\n".join(json.dumps(e) for e in unchanged) + "\n")
            result = subprocess.run(
                [str(BINARY), str(source), str(output)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr[-4000:])
            self.assertEqual(len(json.loads(output.read_text())["report"]["trades"]), 1)


if __name__ == "__main__":
    unittest.main()


def mirror_market_fixture(directory: Path) -> Path:
    """Reflect prices and ranks, preserving event time, to exercise the short lifecycle."""
    source = market_fixture(directory)
    app = json.loads(source.read_text())
    app["strategy"]["directions"] = ["SHORT"]
    source.write_text(json.dumps(app))

    def reflect_bar(bar):
        opening, high, low, close = (Decimal(bar[k]) for k in ("open", "high", "low", "close"))
        for key, value in zip(("open", "high", "low", "close"), (opening, low, high, close)):
            bar[key] = str(Decimal(300) - value)

    path = directory / app["events_path"]
    reflected = []
    for line in path.read_text().splitlines():
        event = json.loads(line)
        if event["kind"] == "bar":
            reflect_bar(event)
        elif event["kind"] == "quote":
            bid, ask = Decimal(event["bid"]), Decimal(event["ask"])
            event["bid"], event["ask"] = str(300 - ask), str(300 - bid)
        else:
            update = event["update"]
            for bar in update["bars"]:
                reflect_bar(bar)
            if update["selection"]:
                for rank in update["selection"]["ranking"]["ranks"].values():
                    rank["percentile"] = 100 - rank["percentile"]
                    rank["score"] = 100 - rank["score"]
                    rank["rank"] = 3001 - rank["rank"]
                    for field in ("returns", "relative_strength"):
                        rank[field] = [-v for v in rank[field]]
        reflected.append(json.dumps(event, separators=(",", ":")))
    path.write_text("\n".join(reflected) + "\n")
    return source


@unittest.skipUnless(BINARY.exists(), "build the native slc-momentum example first")
class ShortMarketNativeTest(unittest.TestCase):
    def test_short_native_entry_protection_and_cash_for_all_entry_modes(self):
        from examples.backtest.slc_momentum_research import validate_execution

        with tempfile.TemporaryDirectory(prefix="slc-short-native-") as temp:
            directory = Path(temp)
            source = mirror_market_fixture(directory)
            app = json.loads(source.read_text())
            path = directory / app["events_path"]
            original = path.read_text()
            for mode in ("MARKET", "LIMIT", "STOP", "STOP_LIMIT"):
                with self.subTest(mode=mode):
                    app["strategy"]["entry_mode"] = mode
                    path.write_text(original)
                    if mode in ("STOP", "STOP_LIMIT"):
                        events = [json.loads(line) for line in original.splitlines()]
                        close_at = app["strategy"]["trading_start"] + 110 * MINUTE
                        trigger = next(Decimal(bar["close"]) for event in events if event["kind"] == "market" for bar in event["update"]["bars"] if bar["bar_type"].startswith("EEE.SIM-") and bar["ts_event"] == close_at)
                        # Place the sell stop above its trigger; the following quote crosses down.
                        for event in events:
                            if event["kind"] == "quote" and event["symbol"] == "EEE.SIM" and event["timestamp"] == close_at + 20_000_000_000:
                                event["bid"], event["ask"] = str(trigger + Decimal("0.02")), str(trigger + Decimal("0.04"))
                        path.write_text("".join(json.dumps(e, separators=(",", ":")) + "\n" for e in events))
                    # Exact-touch fills isolate lifecycle from the separately tested impact model.
                    app["slippage_probability"] = 0.0
                    source.write_text(json.dumps(app))
                    output = directory / "result.json"
                    result = subprocess.run([str(BINARY), str(source), str(output)], capture_output=True, text=True, check=False)
                    self.assertEqual(result.returncode, 0, result.stderr[-4000:])
                    report = json.loads(output.read_text())
                    validate_execution(report)
                    trades = report["report"]["trades"]
                    self.assertEqual(len(trades), 1)
                    trade = trades[0]
                    self.assertEqual(trade["signal"]["side"], "SHORT")
                    self.assertEqual(trade["signal"]["level_type"], "SUPPLY")
                    self.assertLessEqual(trade["signal"]["momentum"]["percentile"], 10)
                    self.assertGreater(Decimal(trade["allocation"]["stop"]), Decimal(trade["allocation"]["entry"]))
                    self.assertLess(Decimal(trade["allocation"]["target"]), Decimal(trade["allocation"]["entry"]))
                    self.assertEqual(Decimal(trade["pnl"]), Decimal(trade["entry_value"]) - Decimal(trade["exit_value"]) - Decimal(trade["fees"]))

    def test_pending_short_cancels_on_side_change_inside_candidate_union(self):
        with tempfile.TemporaryDirectory(prefix="slc-short-rotation-") as temp:
            directory = Path(temp)
            source = mirror_market_fixture(directory)
            app = json.loads(source.read_text())
            c = app["strategy"]
            c["directions"] = ["LONG", "SHORT"]
            c["market"]["max_candidates"] = 600
            c["entry_mode"] = "STOP"
            app["slippage_probability"] = 0.0
            source.write_text(json.dumps(app))
            path = directory / app["events_path"]
            events = [json.loads(line) for line in path.read_text().splitlines()]
            at = c["trading_start"] + 110 * MINUTE
            rotation = at + 30_000_000_000
            trigger = next(Decimal(b["close"]) for e in events if e["kind"] == "market" for b in e["update"]["bars"] if b["bar_type"].startswith("EEE.SIM-") and b["ts_event"] == at)
            for event in events:
                if event["kind"] == "quote" and event["symbol"] == "EEE.SIM" and event["timestamp"] == at + 20_000_000_000:
                    event["bid"], event["ask"] = str(trigger + Decimal("0.02")), str(trigger + Decimal("0.04"))
                if event["kind"] != "market" or not event["update"]["selection"]:
                    continue
                update = event["update"]
                selection = update["selection"]
                selection["candidates"] = [symbol for symbol, rank in selection["ranking"]["ranks"].items() if rank["percentile"] <= 10 or rank["percentile"] >= 90]
                self.assertIn("EEE.SIM", selection["candidates"])
                if selection["available_at"] > c["trading_start"] + MINUTE:
                    update["timestamp"] = selection["available_at"] = selection["ranking"]["timestamp"] = rotation
                    for rank in selection["ranking"]["ranks"].values():
                        rank["timestamp"] = rotation
            for keep_rotation, count in ((True, 0), (False, 1)):
                chosen = [e for e in events if keep_rotation or not (e["kind"] == "market" and e["update"]["timestamp"] == rotation)]
                path.write_text("".join(json.dumps(e, separators=(",", ":")) + "\n" for e in chosen))
                output = directory / "result.json"
                result = subprocess.run([str(BINARY), str(source), str(output)], capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, 0, result.stderr[-3000:])
                report = json.loads(output.read_text())
                self.assertEqual(report["remaining_positions"], 0)
                self.assertEqual(report["remaining_orders"], 0)
                self.assertEqual(report["report"]["errors"], [])
                self.assertEqual(len(report["report"]["trades"]), count)


@unittest.skipUnless(BINARY.exists(), "build the native slc-momentum example first")
class HistoryReplayNativeTest(unittest.TestCase):
    def test_native_history_path_and_missing_candidate_guard(self):
        from collections import defaultdict
        from examples.backtest.slc_momentum_research import validate_execution

        with tempfile.TemporaryDirectory(prefix="slc-history-native-") as temp:
            root = Path(temp)
            source = mirror_market_fixture(root)
            app = json.loads(source.read_text())
            c = app["strategy"]
            ids = {f"{name}.SIM" for name in ("AAA", "BBB", "CCC", "DDD", "EEE")}
            c["universe"] = [m for m in c["universe"] if m["instrument_id"] in ids]
            c["market"]["universe_size"] = 5
            c["market"]["max_candidates"] = 2
            c["momentum"]["minimum_universe_size"] = 5
            daily, minute, selection = defaultdict(list), defaultdict(dict), None

            def store_bar(b):
                symbol = b["bar_type"].split("-1-")[0]
                if "-DAY-" in b["bar_type"]:
                    daily[symbol].append(b)
                else:
                    minute[symbol][b["ts_event"]] = b

            for line in (root / app["events_path"]).read_text().splitlines():
                event = json.loads(line)
                if event["kind"] == "bar":
                    store_bar({"bar_type": f"{event['symbol']}-1-{'DAY' if event['daily'] else 'MINUTE'}-LAST-EXTERNAL", **{k: event[k] for k in ("open", "high", "low", "close", "volume")}, "ts_event": event["timestamp"], "ts_init": event["available_at"]})
                elif event["kind"] == "market":
                    update = event["update"]
                    if selection is None and update["selection"]:
                        selection = update["selection"]
                    for b in update["bars"]:
                        store_bar(b)
            ranks = {symbol: rank for symbol, rank in selection["ranking"]["ranks"].items() if symbol in ids}
            for index, (_, rank) in enumerate(sorted(ranks.items(), key=lambda pair: pair[1]["score"])):
                rank["percentile"], rank["rank"] = 25.0 * index, 5 - index
            selection["ranking"]["ranks"] = ranks
            selection["candidates"] = [symbol for symbol, r in ranks.items() if r["percentile"] <= 10]
            selection["universe_size"] = selection["observed_size"] = 5
            (root / "daily").mkdir()
            for symbol, bars in daily.items():
                (root / "daily" / f"{symbol}.json").write_text(json.dumps(sorted(bars, key=lambda b: b["ts_event"])))
            active = set(selection["candidates"]) | set(c["benchmarks"])
            ranges = {symbol: {"open": c["sessions"][19]["open"], "close": c["trading_end"]} for symbol in active}
            for symbol in active:
                directory = root / "minute" / symbol
                directory.mkdir(parents=True)
                (directory / "complete.json").write_text(json.dumps(ranges[symbol]))
                with (directory / "page.csv").open("w") as stream:
                    for b in minute[symbol].values():
                        stream.write(",".join([str((b["ts_event"] - MINUTE) // 1_000_000_000), *(b[k] for k in ("open", "high", "low", "close", "volume"))]) + "\n")
            plan = {"provenance": "Invented short fixture; no historical alpha evidence", "daily_cache_dir": str(root / "daily"), "strategy": c, "price_increments": app["price_increments"], "selections": [selection], "ranges": ranges, "starting_equity": "100000", "spread_bps": "4", "opening_spread_multiplier": "2", "quote_depth": "100", "slippage_probability": 0.0}
            (root / "plan.json").write_text(json.dumps(plan))
            output = root / "history-result.json"
            result = subprocess.run([str(BINARY), "--history", str(root), str(output)], capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr[-3000:])
            report = json.loads(output.read_text())
            self.assertTrue(report["complete_period"])
            validate_execution(report)
            self.assertEqual(len(report["daily"]), 1)
            self.assertEqual(len(report["report"]["trades"]), 1)
            self.assertEqual(report["report"]["trades"][0]["signal"]["side"], "SHORT")
            # An explicit two-index research variant must work with no QQQ minute files
            c["regime_benchmarks"] = [c["benchmarks"][0], c["benchmarks"][2]]
            del ranges[c["benchmarks"][1]]
            shutil.rmtree(root / "minute" / c["benchmarks"][1])
            (root / "plan.json").write_text(json.dumps(plan))
            result = subprocess.run([str(BINARY), "--history", str(root), str(output)], capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr[-3000:])
            report = json.loads(output.read_text())
            validate_execution(report)
            self.assertEqual(len(report["report"]["trades"]), 1)
            (root / "minute" / "EEE.SIM" / "complete.json").unlink()
            result = subprocess.run([str(BINARY), "--history", str(root), str(output)], capture_output=True, text=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("candidate history incomplete", result.stderr)
