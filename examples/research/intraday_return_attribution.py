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

"""Offline counterfactual audit of Notebook accounting; never a trading runtime."""

import argparse
import ast
import hashlib
import json
from pathlib import Path

import numpy as np
import pandas as pd


def metrics(returns):
    total = (1 + returns).prod()
    return {
        "sessions": len(returns),
        "annualized_return": total ** (252 / len(returns)) - 1,
        "annualized_volatility": returns.std() * np.sqrt(252),
        "sharpe": returns.mean() / returns.std() * np.sqrt(252),
        "total_return": total - 1,
        "final_equity": total * 100_000,
    }


def audit(notebook, data, reports, output):
    cells = json.loads(notebook.read_text())["cells"]
    sources = {i: "".join(cells[i]["source"]) for i in (25, 56, 57)}
    namespace = {"pd": pd, "np": np}
    for index, source in sources.items():
        tree = ast.parse(source)
        if any(not isinstance(node, ast.FunctionDef) for node in tree.body):
            raise ValueError(
                f"Expected only audited function definitions: cell {index}"
            )
        exec(compile(tree, f"cell-{index}", "exec"), namespace)  # noqa: S102 - offline user-supplied reference functions

    df = pd.read_csv(data)
    df["date"] = (
        pd.to_datetime(df.timestamp_ns, utc=True)
        .dt.tz_convert("America/New_York")
        .dt.date
    )
    days = sorted(df.date.unique())
    df = df[df.date.isin(days[int(len(days) * 0.8) :])].copy()
    df["minute_index"] = df.groupby("date").cumcount()
    df["price_volume"] = df.vwap * df.volume
    df["vwap_anchored"] = (
        df.groupby("date").price_volume.cumsum() / df.groupby("date").volume.cumsum()
    )
    features = namespace["generate_signals_rvol"](df.copy(), 14, 1.0)
    notebook_signals = namespace["execute_strategy_rvol"](features, rvol_threshold=1.0)
    features["minute_index"] += 1
    production_signals = namespace["execute_strategy_rvol"](
        features, rvol_threshold=1.0
    )

    replacements = [
        (
            "trades['pos_exec'] = trades.groupby('date')['position'].ffill().fillna(0).astype(int)",
            "trades['pos_exec'] = trades.groupby('date')['position'].shift(1).fillna(0).astype(int)",
        ),
        (
            "daily_spy_rets.rolling(window=14).std()",
            "daily_spy_rets.rolling(window=14).std().shift(1)",
        ),
    ]
    for old, _ in replacements:
        if sources[25].count(old) != 1:
            raise ValueError(
                "Notebook changed: re-audit the accounting transformations"
            )

    daily = {}
    results = {}
    for name, shift_execution, shift_volatility, production_clock in [
        ("original_notebook", False, False, False),
        ("execution_only", True, False, False),
        ("volatility_only", False, True, False),
        ("execution_and_volatility", True, True, False),
        ("plus_production_clock", True, True, True),
    ]:
        source = sources[25]
        for enabled, (old, new) in zip(
            (shift_execution, shift_volatility), replacements
        ):
            if enabled:
                source = source.replace(old, new)
        scope = {"pd": pd, "np": np}
        exec(compile(source, name, "exec"), scope)  # noqa: S102 - explicit offline accounting counterfactual
        signals = production_signals if production_clock else notebook_signals
        returns = scope["calculate_pnl"](signals.copy(), df, target_vol=0.03)
        daily[name] = returns
        results[name] = metrics(returns)

    expected = pd.read_csv(
        reports / "notebook/notebook_test_returns.csv", index_col="date"
    )
    assert list(expected.index) == [
        str(date) for date in daily["original_notebook"].index
    ]
    np.testing.assert_allclose(
        daily["original_notebook"], expected["return"], atol=1e-14, rtol=0
    )
    native = pd.read_csv(reports / "test/daily_returns.csv", index_col="date")
    assert list(native.index) == list(expected.index)
    saved_metrics = json.loads((reports / "test/metrics.json").read_text())
    results["nautilus_existing_run"] = metrics(native["return"])
    for key in ("annualized_return", "annualized_volatility", "sharpe", "total_return"):
        np.testing.assert_allclose(
            results["nautilus_existing_run"][key],
            saved_metrics[key],
            atol=1e-10,
            rtol=0,
        )

    prior = notebook_signals.groupby("date").position.shift(1).fillna(0)
    entry = notebook_signals[(notebook_signals.position != 0) & (prior == 0)].iloc[0]
    fill = notebook_signals.loc[entry.name + 1]
    example = {
        "bar_start_et": str(
            pd.Timestamp(int(entry.timestamp_ns) - 60_000_000_000, tz="UTC").tz_convert(
                "America/New_York"
            )
        ),
        "signal_known_et": str(
            pd.Timestamp(int(entry.timestamp_ns), tz="UTC").tz_convert(
                "America/New_York"
            )
        ),
        "target": int(entry.position),
        "open": float(entry.open),
        "close": float(entry.close),
        "next_open": float(fill.open),
    }
    report = {
        "purpose": "Fixed-parameter accounting diagnosis on already observed OOS; not optimization or new OOS validation",
        "notebook_sha256": hashlib.sha256(notebook.read_bytes()).hexdigest(),
        "evaluation_start": str(daily["original_notebook"].index[0]),
        "evaluation_end": str(daily["original_notebook"].index[-1]),
        "parameters": {
            "lookback": 14,
            "vm": 1.0,
            "rvol": 1.0,
            "target_vol": 0.03,
            "max_leverage": 4,
        },
        "transformations": replacements,
        "results": results,
        "first_notebook_entry": example,
        "limits": [
            "Offline variants retain Notebook compounded percentage PnL and missing EOD exit cost.",
            "Only the existing Nautilus row uses native fills, fixed shares, EOD flatten and buying-power limits.",
            "Effects interact; sequential differences depend on order and are not independent alpha contributions.",
            "Original missing spy_data.csv byte identity cannot be verified; the downloaded-data original returns match the prior audit.",
        ],
        "checks": "All 483 original daily returns match the saved literal audit at 1e-14; native dates and metrics match saved reports.",
    }
    output.mkdir(parents=True, exist_ok=True)
    (output / "attribution.json").write_text(json.dumps(report, indent=2) + "\n")
    pd.DataFrame(daily).to_csv(output / "daily_returns.csv", index_label="date")
    print(json.dumps({"results": results, "first_entry": example}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--notebook", type=Path, required=True)
    parser.add_argument("--data", type=Path, required=True)
    parser.add_argument("--reports", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    audit(args.notebook, args.data, args.reports, args.output)
