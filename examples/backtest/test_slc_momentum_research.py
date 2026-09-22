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

from examples.backtest.slc_momentum_research import metrics
from examples.backtest.slc_momentum_research import sensitivity_configs
from examples.backtest.slc_momentum_research import trade_metrics


def test_metrics_keep_flat_days_and_intraday_drawdown() -> None:
    result = {
        "synthetic": True,
        "starting_equity": "100",
        "configuration": {
            "trading_start": 1,
            "trading_end": 100_000_000_000_000,
            "sessions": [{"open": 1, "close": 3}, {"open": 4, "close": 6}],
        },
        "report": {
            "trades": [],
            "turnover": "20",
            "equity": [
                {"timestamp": 2, "session_open": 1, "equity": "90", "exposure": "10"},
                {"timestamp": 3, "session_open": 1, "equity": "100", "exposure": "0"},
                {"timestamp": 5, "session_open": 4, "equity": "100", "exposure": "0"},
            ],
        },
    }
    summary = metrics(result)
    assert abs(summary["max_drawdown"] - 0.1) < 1e-12
    assert summary["sharpe"] is None
    assert summary["expectancy"] is None
    assert summary["cagr"] == 0
    assert summary["missing_equity_sessions"] == []


def test_trade_metrics_use_net_pnl_and_r() -> None:
    result = trade_metrics(
        [{"pnl": "100", "r_multiple": "1"}, {"pnl": "-50", "r_multiple": "-0.5"}]
    )
    assert result["expectancy"] == 25
    assert result["profit_factor"] == 2
    assert result["average_r"] == 0.25
    assert result["win_rate"] == 0.5
    assert trade_metrics([])["profit_factor"] is None


def test_exposure_excludes_overnight_delivery_delay() -> None:
    result = {
        "synthetic": True,
        "starting_equity": "100",
        "configuration": {
            "trading_start": 10,
            "trading_end": 2000,
            "sessions": [{"open": 10, "close": 30}, {"open": 1000, "close": 1030}],
        },
        "report": {
            "trades": [],
            "turnover": "0",
            "equity": [
                {"timestamp": 10, "session_open": 10, "equity": "100", "exposure": "10"},
                {"timestamp": 20, "session_open": 10, "equity": "100", "exposure": "0"},
                {"timestamp": 1001, "session_open": 10, "equity": "100", "exposure": "0"},
                {"timestamp": 1002, "session_open": 1000, "equity": "100", "exposure": "0"},
            ],
        },
    }
    summary = metrics(result)
    assert summary["time_exposure"] == 0.5
    assert abs(summary["average_notional_exposure"] - 0.05) < 1e-12


def test_sensitivity_does_not_mutate_baseline() -> None:
    config = {"strategy": {"momentum": {"lookbacks": [1, 5, 10, 20]}}}
    neighbors = sensitivity_configs(config)
    assert len(neighbors) == 23
    assert config["strategy"]["momentum"]["lookbacks"] == [1, 5, 10, 20]
    assert neighbors[0][1]["strategy"]["momentum"]["lookbacks"] == [1, 4, 8, 16]


def test_walk_forward_keeps_test_sealed_and_uses_baseline_without_evidence(
    tmp_path, monkeypatch
) -> None:
    from examples.backtest import slc_momentum_research as research

    calls = []
    config = {
        "strategy": {
            "trading_start": 10,
            "trading_end": 69,
            "sessions": [{"open": n, "close": n + 9} for n in range(10, 100, 10)],
        }
    }

    def replay(binary, candidate, output, label):
        calls.append(
            (label, candidate["strategy"]["trading_start"], candidate["strategy"]["trading_end"])
        )
        return {"trades": 1, "sharpe": 1000, "missing_equity_sessions": []}

    monkeypatch.setattr(research, "run_native", replay)
    folds = research.walk_forward(tmp_path, config, tmp_path, (2, 2, 2))
    assert len(folds) == 1
    assert folds[0]["winner"] == "base"
    assert folds[0]["status"] == "BASELINE_ONLY_INSUFFICIENT_VALIDATION_TRADES"
    assert calls[-1] == ("wf0_test", 50, 69)
    assert all(end < 50 for label, start, end in calls[:-1])
    assert config["strategy"]["trading_start"] == 10


def test_execution_audit_bounds_native_rounding_and_rejects_missing_cash() -> None:
    import pytest

    from examples.backtest.slc_momentum_research import validate_execution

    trade = {
        "signal": {
            "symbol": "A.SIM",
            "timestamp": 3,
            "available_at": 4,
            "momentum": {"daily_cutoff": 1, "timestamp": 2},
        },
        "opened_at": 5,
        "closed_at": 6,
        "quantity": "10",
        "allocation": {"quantity": "10"},
        "entry_value": "100",
        "exit_value": "110",
        "fees": "1",
        "pnl": "9",
        "native_booked_pnl": "8.99",
        "initial_risk": "5",
        "sell_fill_count": 2,
    }
    result = {
        "remaining_positions": 0,
        "remaining_orders": 0,
        "starting_equity": "100",
        "report": {"errors": [], "trades": [trade], "equity": [{"equity": "108.99"}]},
    }
    validate_execution(result)
    result["report"]["equity"][0]["equity"] = "108"
    with pytest.raises(ValueError, match="equity does not reconcile"):
        validate_execution(result)
    result["report"]["equity"][0]["equity"] = "109"
    trade["opened_at"] = 4
    with pytest.raises(ValueError, match="same-event entry"):
        validate_execution(result)
