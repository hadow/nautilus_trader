"""Check that passive range research cannot borrow future or incomplete closes."""

import pytest
from dynamic_grid_range_observation import observe_ranges


def test_range_uses_completed_closes_without_future_prices() -> None:
    observations = [
        {"ts_ns": 1, "price": "100", "regime_source": {"ts_ns": 1}},
        {"ts_ns": 2, "price": "1000", "regime_source": None},
        {"ts_ns": 3, "price": "102", "regime_source": {"ts_ns": 3}},
        {"ts_ns": 5, "price": "9999", "regime_source": {"ts_ns": 5}},
    ]
    grids = [
        {
            "grid_id": 1,
            "created_ns": 2,
            "center": "100",
            "lower_bound": "90",
            "upper_bound": "110",
        },
        {
            "grid_id": 2,
            "created_ns": 3,
            "center": "100",
            "lower_bound": "90",
            "upper_bound": "110",
        },
    ]
    result = observe_ranges(observations, grids, 2)
    assert result[0]["range"] is None
    assert result[0]["observed_closes"] == 1
    assert result[1]["range"] == {
        "lower": "100",
        "upper": "102",
        "width_pct": "0.02",
        "contains_anchor": True,
    }
    assert result[1]["active_grid_width_pct"] == "0.2"
    assert result[1]["last_completed_close_ns"] == 3
    assert observe_ranges(observations[:3], grids, 2) == result


def test_range_rejects_misaligned_or_replayed_observations() -> None:
    grid = {
        "grid_id": 1,
        "created_ns": 3,
        "center": "100",
        "lower_bound": "90",
        "upper_bound": "110",
    }
    bad = {"ts_ns": 1, "price": "100", "regime_source": {"ts_ns": 2}}
    with pytest.raises(ValueError, match="does not match"):
        observe_ranges([bad], [grid], 2)
    with pytest.raises(ValueError, match="strictly chronological"):
        observe_ranges([bad, bad], [grid], 2)
    with pytest.raises(ValueError, match="at least two"):
        observe_ranges([], [grid], 1)
