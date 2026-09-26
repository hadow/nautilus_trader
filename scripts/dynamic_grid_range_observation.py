"""Observe completed regime-bar ranges at grid creation; never generate orders or parameters."""

import argparse
import hashlib
import json
from collections import deque
from decimal import Decimal
from itertools import pairwise
from pathlib import Path


def observe_ranges(
    observations: list[dict], grids: list[dict], window: int
) -> list[dict]:
    """Use only native completed-bucket closes available at each grid's creation time."""
    if window < 2:
        raise ValueError("Range observation requires at least two closes")
    if any(a["ts_ns"] >= b["ts_ns"] for a, b in pairwise(observations)):
        raise ValueError("Observations must be strictly chronological")
    if any(a["created_ns"] > b["created_ns"] for a, b in pairwise(grids)):
        raise ValueError("Grids must be chronological")
    closes: deque[Decimal] = deque(maxlen=window)
    cursor = 0
    last_close_ns = None
    result = []
    for grid in grids:
        while (
            cursor < len(observations)
            and observations[cursor]["ts_ns"] <= grid["created_ns"]
        ):
            observation = observations[cursor]
            cursor += 1
            source = observation.get("regime_source")
            if source is None:
                continue
            if source["ts_ns"] != observation["ts_ns"]:
                raise ValueError("Regime bucket close does not match observation time")
            price = Decimal(observation["price"])
            if not price.is_finite() or price <= 0:
                raise ValueError("Invalid completed close")
            closes.append(price)
            last_close_ns = source["ts_ns"]
        center = Decimal(grid["center"])
        if not center.is_finite() or center <= 0:
            raise ValueError("Invalid grid center")
        result.append(
            {
                "grid_id": grid["grid_id"],
                "created_ns": grid["created_ns"],
                "last_completed_close_ns": last_close_ns,
                "observed_closes": len(closes),
                "window": window,
                "range": None
                if len(closes) < window
                else {
                    "lower": str(min(closes)),
                    "upper": str(max(closes)),
                    "width_pct": str((max(closes) - min(closes)) / center),
                    "contains_anchor": min(closes) <= center <= max(closes),
                },
                "active_grid_width_pct": str(
                    (Decimal(grid["upper_bound"]) - Decimal(grid["lower_bound"]))
                    / center,
                ),
            }
        )
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("candidate", nargs="?", default="Sequential")
    args = parser.parse_args()
    raw = args.report.read_bytes()
    candidate = json.loads(raw)["ablations"][args.candidate]
    output = {}
    for symbol, instrument in candidate["report"]["instruments"].items():
        config = candidate["effective_config"]["instruments"][symbol]["strategy"][
            "grid"
        ]
        if (
            config["strategy_mode"] != "StockAdaptive"
            or config["regime_bar_minutes"] != 15
        ):
            raise ValueError(
                "Expected the native StockAdaptive 15-minute regime stream"
            )
        diagnostics = instrument["diagnostics"]
        output[symbol] = observe_ranges(
            diagnostics["spacing"],
            diagnostics["grids"],
            config["ma_period"],
        )
    print(
        json.dumps(
            {
                "source": str(args.report),
                "sha256": hashlib.sha256(raw).hexdigest(),
                "candidate": args.candidate,
                "notes": "Passive close range using existing ma_period; includes overnight history. No fills or alpha claim.",
                "instruments": output,
            },
            ensure_ascii=False,
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
