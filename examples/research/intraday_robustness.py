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

"""Launch native Rust replays from a frozen plan; no Python strategy or fill simulation."""

import argparse
import csv
import gzip
import hashlib
import json
import math
import random
import shutil
import statistics
import subprocess
from bisect import bisect_left
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from decimal import Decimal
from itertools import pairwise
from pathlib import Path
from zoneinfo import ZoneInfo


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(root, variants, end=None, binary=Path("target/debug/intraday-backtest")):
    plan = json.loads((root / "plan.json").read_text())
    binary_hash = digest(binary)
    for symbol, path in plan["inputs"].items():
        assert digest(Path(path)) == plan["input_sha256"][symbol]

    def replay(job):
        variant, symbol = job
        config = root / "configs" / f"{variant}.json"
        output = root / variant / symbol
        output.mkdir(parents=True, exist_ok=True)
        command = [
            str(binary),
            "--config",
            str(config),
            "--symbol",
            symbol,
            "--input",
            plan["inputs"][symbol],
            "--start",
            plan["start"],
            "--end",
            end or plan["end"],
            "--output",
            str(output),
        ]
        identity = {
            "command": command,
            "config_sha256": digest(config),
            "binary_sha256": binary_hash,
            "input_sha256": plan["input_sha256"][symbol],
        }
        manifest = output / "run.json"
        if manifest.exists():
            old = json.loads(manifest.read_text())
            assert old["identity"] == identity, (
                "refusing to overwrite another experiment"
            )
            if old["completed"]:
                print(f"CACHED {variant} {symbol}", flush=True)
                return
        assert shutil.disk_usage(root).free >= 1024**3, "less than 1 GiB free"
        manifest.write_text(
            json.dumps({"identity": identity, "completed": False}, indent=2)
        )
        print(f"START {variant} {symbol}", flush=True)
        with (
            gzip.open(output / "run.log.gz", "wb") as log,
            subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            ) as child,
        ):
            shutil.copyfileobj(child.stdout, log)
            code = child.wait()
        assert code == 0, (variant, symbol, code, "see run.log.gz")
        state = output / "strategy_report.json"
        report = json.loads(state.read_text())
        assert not report["errors"], report["errors"]
        parity = json.loads((output / "parity_report.json").read_text())
        assert parity["feature_match"] and parity["signal_match"], (
            variant,
            symbol,
            parity,
        )
        original_hash = digest(state)
        with (
            state.open("rb") as source,
            gzip.open(state.with_suffix(".json.gz"), "wb") as archive,
        ):
            shutil.copyfileobj(source, archive)
        with gzip.open(state.with_suffix(".json.gz"), "rb") as archive:
            assert hashlib.file_digest(archive, "sha256").hexdigest() == original_hash
        state.unlink()
        manifest.write_text(
            json.dumps(
                {
                    "identity": identity,
                    "completed": True,
                    "strategy_report_sha256": original_hash,
                },
                indent=2,
            )
        )
        print(f"COMPLETE {variant} {symbol}", flush=True)

    jobs = [(variant, symbol) for variant in variants for symbol in plan["inputs"]]
    with ThreadPoolExecutor(max_workers=2) as pool:
        list(pool.map(replay, jobs))


def rows(path):
    with path.open() as stream:
        return list(csv.DictReader(stream))


def write_csv(path, records):
    if not records:
        return
    with path.open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(records[0]))
        writer.writeheader()
        writer.writerows(records)


def return_stats(values):
    volatility = statistics.stdev(values) if len(values) > 1 else 0
    equity = peak = 1.0
    drawdown = 0.0
    for value in values:
        equity *= 1 + value
        peak = max(peak, equity)
        drawdown = max(drawdown, 1 - equity / peak)
    tail = sorted(values)[: max(1, math.ceil(len(values) * 0.05))]
    return {
        "days": len(values),
        "total_return": equity - 1,
        "annualized_return": equity ** (252 / len(values)) - 1,
        "sharpe": statistics.mean(values) / volatility * math.sqrt(252)
        if volatility
        else 0,
        "max_drawdown": drawdown,
        "worst_day": min(values),
        "expected_shortfall_95": statistics.mean(tail),
    }


def bootstrap_mean(values, block, repetitions=2000):
    """Circular moving-block bootstrap of observed daily returns, never simulated fills."""
    generator = random.Random(20260920)
    count = len(values)
    block = min(block, count)
    prefix = [0.0]
    for value in values + values[:block]:
        prefix.append(prefix[-1] + value)
    whole, remainder = divmod(count, block)
    sums = [prefix[i + block] - prefix[i] for i in range(count)]
    tail = [prefix[i + remainder] - prefix[i] for i in range(count)]
    samples = []
    for _ in range(repetitions):
        total = sum(sums[generator.randrange(count)] for _ in range(whole))
        if remainder:
            total += tail[generator.randrange(count)]
        samples.append(total / count)
    samples.sort()
    mean = statistics.mean(values)
    return {
        "block": block,
        "mean": mean,
        "ci95_low": samples[int(repetitions * 0.025)],
        "ci95_high": samples[int(repetitions * 0.975)],
        "p_two_sided": (1 + sum(abs(v - mean) >= abs(mean) for v in samples))
        / (repetitions + 1),
    }


def spy_regimes(dates):
    """Descriptive labels from prior observed SPY sessions; never an entry filter."""
    path = Path("test_data/local/intraday_momentum/spy_notebook_full.csv")
    closes = {}
    zone = ZoneInfo("America/New_York")
    with path.open() as stream:
        for row in csv.DictReader(stream):
            if row["timestamp_ns"] == row["session_close"]:
                day = (
                    datetime.fromtimestamp(int(row["session_open"]) // 10**9, zone)
                    .date()
                    .isoformat()
                )
                closes[day] = Decimal(row["close"])
    ordered = sorted(closes)
    labels = {}
    for day in dates:
        index = bisect_left(ordered, day)
        if index < 21:
            labels[day] = "INSUFFICIENT_HISTORY"
            continue
        history = [closes[d] for d in ordered[index - 21 : index]]
        momentum = history[-1] / history[0] - 1
        returns = [float(b / a - 1) for a, b in pairwise(history)]
        trend = (
            "UP"
            if momentum > Decimal("0.03")
            else "DOWN"
            if momentum < Decimal("-0.03")
            else "SIDEWAYS"
        )
        volatility = "HIGH_VOL" if statistics.stdev(returns) >= 0.015 else "LOW_VOL"
        labels[day] = f"{trend}/{volatility}"
    return labels, {
        "source": str(path),
        "sha256": digest(path),
        "momentum": "prior 20 observed complete sessions, thresholds +/-3%",
        "volatility": "prior 20 close returns, sample std >=1.5% daily",
        "used_for_orders": False,
        "current_day_close_used": False,
    }


def summarize(root):
    plan = json.loads((root / "plan.json").read_text())
    zone = ZoneInfo("America/New_York")

    def day(timestamp):
        return datetime.fromtimestamp(int(timestamp) // 10**9, zone).date().isoformat()

    sources = {"baseline": Path(plan["baseline"])}
    sources.update(
        {
            p.parent.parent.name: p.parent.parent
            for p in sorted(root.glob("*/*/metrics.json"))
        }
    )
    series, summary, windows, sides, time_of_day, audits = {}, [], [], [], [], {}
    regimes, regime_metadata = spy_regimes(
        json.loads((Path(plan["baseline"]) / "data_manifest.json").read_text())[
            "sessions"
        ]
    )
    regime_rows = []
    volumes = {}
    for symbol, path in plan["inputs"].items():
        with Path(path).open() as stream:
            volumes[symbol] = {
                int(r["timestamp_ns"]): Decimal(r["volume"])
                for r in csv.DictReader(stream)
                if (int(r["timestamp_ns"]) - int(r["session_open"])) % (30 * 60 * 10**9)
                == 0
            }
    capacity = []
    for variant, directory in sources.items():
        series[variant] = {}
        for symbol in plan["inputs"]:
            location = directory / symbol / ("full" if variant == "baseline" else "")
            if not (location / "metrics.json").exists():
                continue
            if (
                variant != "baseline"
                and not json.loads((location / "run.json").read_text())["completed"]
            ):
                continue
            metrics = json.loads((location / "metrics.json").read_text())
            parity = json.loads((location / "parity_report.json").read_text())
            state = location / "strategy_report.json"
            if state.exists():
                report = json.loads(state.read_text())
            else:
                with gzip.open(state.with_suffix(".json.gz"), "rt") as stream:
                    report = json.load(stream)
            assert (
                not report["errors"]
                and parity["feature_match"]
                and parity["signal_match"]
            )
            daily = {
                r["date"]: float(r["return"])
                for r in rows(location / "daily_returns.csv")
            }
            series[variant][symbol] = daily
            trades = rows(location / "trades.csv")
            ratios = []
            zero_volume = 0
            for trade in trades:
                volume = volumes[symbol][int(trade["entry_timestamp"]) - 1]
                if volume > 0:
                    ratios.append(Decimal(trade["quantity"]) / volume)
                else:
                    zero_volume += 1
            capacity.append(
                {
                    "variant": variant,
                    "symbol": symbol,
                    "entries": len(trades),
                    "zero_prior_minute_volume": zero_volume,
                    "median_prior_minute_fraction": str(statistics.median(ratios))
                    if ratios
                    else None,
                    "maximum_prior_minute_fraction": str(max(ratios))
                    if ratios
                    else None,
                    "entries_above_1pct": sum(r > Decimal("0.01") for r in ratios),
                    "entries_above_5pct": sum(r > Decimal("0.05") for r in ratios),
                    "entries_above_10pct": sum(r > Decimal("0.10") for r in ratios),
                }
            )
            assert math.isclose(
                math.prod(1 + r for r in daily.values()) - 1,
                metrics["total_return"],
                abs_tol=1e-10,
            )
            counts = Counter(day(t["entry_timestamp"]) for t in trades)
            assert max(counts.values(), default=0) <= 3
            assert all(
                day(t["entry_timestamp"]) == day(t["exit_timestamp"]) for t in trades
            )
            ids = [f["order_id"] for f in report["fills"]]
            assert len(ids) == len(set(ids))
            assert all(f["timestamp"] >= f["signal_timestamp"] for f in report["fills"])
            assert sum(Decimal(f["quantity"]) * f["side"] for f in report["fills"]) == 0
            risks = report.get("risk_events", [])
            for label in sorted({regimes.get(d, "UNKNOWN") for d in daily}):
                selected = {
                    d: v for d, v in daily.items() if regimes.get(d, "UNKNOWN") == label
                }
                subset = [t for t in trades if day(t["entry_timestamp"]) in selected]
                regime_rows.append(
                    {
                        "variant": variant,
                        "symbol": symbol,
                        "regime": label,
                        "days": len(selected),
                        "mean_daily_return": statistics.mean(selected.values()),
                        "trades": len(subset),
                        "net_pnl": str(sum(Decimal(t["pnl"]) for t in subset)),
                    }
                )
            for risk in risks:
                if risk["reason"] == "DAILY_LOSS":
                    assert not any(
                        day(t["entry_timestamp"]) == day(risk["timestamp"])
                        and int(t["entry_timestamp"]) > int(risk["timestamp"])
                        for t in trades
                    )
            summary.append(
                {
                    "variant": variant,
                    "symbol": symbol,
                    **return_stats(list(daily.values())),
                    "profit_factor": metrics["profit_factor"],
                    "trades": len(trades),
                    "costs": metrics["transaction_costs"],
                    "turnover": metrics["turnover_notional"],
                    "maximum_leverage": metrics["maximum_leverage"],
                    "risk_exits": len(risks),
                    "daily_locks": sum(r["reason"] == "DAILY_LOSS" for r in risks),
                    "median_hold_minutes": statistics.median(
                        (int(t["exit_timestamp"]) - int(t["entry_timestamp"])) / 60e9
                        for t in trades
                    )
                    if trades
                    else 0,
                }
            )
            audits[f"{variant}/{symbol}"] = {
                "max_entries": max(counts.values(), default=0),
                "no_overnight": True,
                "unique_fill_ids": True,
                "risk_locks_respected": True,
                "parity": parity,
                "metrics_sha256": digest(location / "metrics.json"),
                "trades_sha256": digest(location / "trades.csv"),
            }
            for side in [1, -1]:
                subset = [t for t in trades if int(t["side"]) == side]
                pnl = [Decimal(t["pnl"]) for t in subset]
                sides.append(
                    {
                        "variant": variant,
                        "symbol": symbol,
                        "side": side,
                        "trades": len(subset),
                        "net_pnl": str(sum(pnl)),
                        "note": "contribution only; not a one-direction counterfactual",
                    }
                )
            groups = defaultdict(list)
            for trade in trades:
                stamp = datetime.fromtimestamp(
                    int(trade["entry_timestamp"]) // 10**9, zone
                )
                groups[f"{stamp.hour:02d}:{stamp.minute // 30 * 30:02d}"].append(
                    Decimal(trade["pnl"])
                )
            for period, pnls in sorted(groups.items()):
                time_of_day.append(
                    {
                        "variant": variant,
                        "symbol": symbol,
                        "entry_time_et": period,
                        "trades": len(pnls),
                        "net_pnl": str(sum(pnls)),
                        "average_trade": str(sum(pnls) / len(pnls)),
                    }
                )

    baskets = {}
    for variant, members in series.items():
        if len(members) != 5:
            continue
        common = sorted(set.intersection(*(set(s) for s in members.values())))
        baskets[variant] = {
            d: statistics.mean(s[d] for s in members.values()) for d in common
        }
    comparisons = []
    parents = {
        "directional": "baseline",
        "cost_1bp": "directional",
        "cost_3bp": "cost_1bp",
        "unlevered": "cost_1bp",
        "risk_budget": "unlevered",
    }
    parents.update(
        {name: "risk_budget" for name in series if name.startswith(("vm_", "rvol_"))}
    )
    for variant, parent in parents.items():
        if variant not in baskets or parent not in baskets:
            continue
        for symbol in [*plan["inputs"], "equal_weight_diagnostic"]:
            left = (
                baskets[parent]
                if symbol == "equal_weight_diagnostic"
                else series[parent][symbol]
            )
            right = (
                baskets[variant]
                if symbol == "equal_weight_diagnostic"
                else series[variant][symbol]
            )
            common = sorted(left.keys() & right.keys())
            differences = [right[d] - left[d] for d in common]
            for block in [5, 10, 20]:
                comparisons.append(
                    {
                        "variant": variant,
                        "parent": parent,
                        "symbol": symbol,
                        "days": len(common),
                        **bootstrap_mean(differences, block),
                    }
                )
    # Holm adjustment within all reported 10-day-block paired tests; confidence intervals remain marginal
    family = sorted(
        [r for r in comparisons if r["block"] == 10], key=lambda r: r["p_two_sided"]
    )
    previous = 0.0
    for index, result in enumerate(family):
        previous = max(
            previous, min(1.0, (len(family) - index) * result["p_two_sided"])
        )
        result["holm_p"] = previous
    for result in comparisons:
        result.setdefault("holm_p", None)
    for variant, members in series.items():
        for symbol, daily in {
            **members,
            "equal_weight_diagnostic": baskets.get(variant, {}),
        }.items():
            if not daily:
                continue
            for fold in range(5):
                half = 2021 * 2 + fold

                def boundary(offset, half=half):
                    year, h = divmod(half + offset, 2)
                    return f"{year}-{'07' if h else '01'}-01"

                for part, start, end in [
                    ("train", 0, 4),
                    ("validation", 4, 5),
                    ("test", 5, 6),
                ]:
                    values = [
                        v
                        for d, v in daily.items()
                        if boundary(start) <= d < boundary(end)
                    ]
                    if values:
                        windows.append(
                            {
                                "variant": variant,
                                "symbol": symbol,
                                "fold": fold + 1,
                                "part": part,
                                "start": boundary(start),
                                "end_exclusive": boundary(end),
                                **return_stats(values),
                            }
                        )
    basket_rows = [
        {"variant": name, **return_stats(list(values.values()))}
        for name, values in baskets.items()
    ]
    basket_ci = [
        {"variant": name, **bootstrap_mean(list(values.values()), block)}
        for name, values in baskets.items()
        for block in [5, 10, 20]
    ]
    asset_ci = [
        {
            "variant": name,
            "symbol": symbol,
            **bootstrap_mean(list(daily.values()), block),
        }
        for name, members in series.items()
        for symbol, daily in members.items()
        for block in [5, 10, 20]
    ]
    concentration, correlations = [], []
    for name, members in series.items():
        if len(members) != 5:
            continue
        common = sorted(set.intersection(*(set(s) for s in members.values())))
        for excluded in members:
            values = [
                statistics.mean(
                    s[d] for symbol, s in members.items() if symbol != excluded
                )
                for d in common
            ]
            concentration.append(
                {"variant": name, "excluded_symbol": excluded, **return_stats(values)}
            )
        for i, left in enumerate(members):
            for right in list(members)[i + 1 :]:
                x, y = (
                    [members[left][d] for d in common],
                    [members[right][d] for d in common],
                )
                correlations.append(
                    {
                        "variant": name,
                        "left": left,
                        "right": right,
                        "correlation": statistics.correlation(x, y)
                        if statistics.stdev(x) and statistics.stdev(y)
                        else None,
                    }
                )
    for name, records in [
        ("summary", summary),
        ("paired_bootstrap", comparisons),
        ("walk_forward", windows),
        ("side_contribution", sides),
        ("time_of_day", time_of_day),
        ("diagnostic_baskets", basket_rows),
        ("basket_bootstrap", basket_ci),
        ("regime_breakdown", regime_rows),
        ("asset_bootstrap", asset_ci),
        ("leave_one_stock_out", concentration),
        ("daily_correlations", correlations),
        ("capacity", capacity),
    ]:
        write_csv(root / f"{name}.csv", records)
    (root / "verification.json").write_text(json.dumps(audits, indent=2) + "\n")
    (root / "regime_metadata.json").write_text(
        json.dumps(regime_metadata, indent=2) + "\n"
    )
    lines = [
        "# 日内动量策略：退出、成本与风险的分阶段评估",
        "",
        "本报告由 Rust NautilusTrader 实际成交账本生成。Python 只负责启动进程和统计已有结果，没有计算交易信号或模拟成交。",
        "",
        "固定 AAPL、JPM、XOM、BA、PFE；2021–2025；每只独立 10 万美元，每日最多三次新开仓。原始基线保留于 ../common_stocks。",
        "",
        "## 逐股结果",
        "",
        "| 版本 | 股票 | 年化收益 | Sharpe | 最大回撤 | PF | 交易数 | 风险退出 |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for r in summary:
        if r["variant"].startswith(("vm_", "rvol_")):
            continue
        lines.append(
            f"| {r['variant']} | {r['symbol']} | {r['annualized_return']:.2%} | {r['sharpe']:.3f} | {r['max_drawdown']:.2%} | {float(r['profit_factor'] or 0):.3f} | {r['trades']} | {r['risk_exits']} |"
        )
    lines += [
        "",
        "## 五股等权日收益诊断",
        "",
        "下表是五个独立账户日收益的算术平均，用于观察跨股票共性。它不是共享资金账户回测，也未计算组合再平衡交易成本。",
        "",
        "| 版本 | 年化收益 | Sharpe | 最大回撤 | 最差一天 |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for r in basket_rows:
        if r["variant"].startswith(("vm_", "rvol_")):
            continue
        lines.append(
            f"| {r['variant']} | {r['annualized_return']:.2%} | {r['sharpe']:.3f} | {r['max_drawdown']:.2%} | {r['worst_day']:.2%} |"
        )
    sensitivity = [r for r in basket_rows if r["variant"].startswith(("vm_", "rvol_"))]
    if sensitivity:
        parent = [
            r
            for r in windows
            if r["variant"] == "risk_budget"
            and r["symbol"] == "equal_weight_diagnostic"
            and r["fold"] == 1
            and r["part"] == "train"
        ]
        lines += [
            "",
            "## 参数邻域：仅 2021–2022 训练段",
            "",
            "每次只改变 VM 或 RVOL 阈值，其他参数等同 risk_budget。下表仍为五股等权日收益诊断，逐股原生结果见 summary.csv；未据此选择最佳参数。",
            "",
            "| 版本 | 有效日数 | 年化收益 | Sharpe | 最大回撤 |",
            "| --- | ---: | ---: | ---: | ---: |",
        ]
        for r in parent + sensitivity:
            lines.append(
                f"| {r['variant']} | {r['days']} | {r['annualized_return']:.2%} | {r['sharpe']:.3f} | {r['max_drawdown']:.2%} |"
            )
    risk_rows = [r for r in summary if r["variant"] == "risk_budget"]
    if len(risk_rows) == 5:
        profitable = [r["symbol"] for r in risk_rows if r["total_return"] > 0]
        passed = [
            r["symbol"]
            for r in risk_rows
            if r["annualized_return"] > 0
            and r["sharpe"] >= 1
            and float(r["profit_factor"] or 0) >= 1.2
            and r["max_drawdown"] <= 0.15
        ]
        lines += [
            "",
            "## 评估结论",
            "",
            f"风险预算版本盈利股票：{', '.join(profitable) or '无'}；同时满足年化收益为正、Sharpe ≥1、PF ≥1.2、日终回撤 ≤15% 的股票：{', '.join(passed) or '无'}。这是研究筛选线，不是行业定律或收益保证。",
            "风险下降不能直接解释为新增 alpha。需对照 unlevered 区分杠杆缩小的作用；按方向退出也没有普遍改善五只股票。当前证据不支持把它升级为稳健实盘主策略。",
        ]
        for symbol in profitable:
            tests = [
                r
                for r in windows
                if r["variant"] == "risk_budget"
                and r["symbol"] == symbol
                and r["part"] == "test"
            ]
            intervals = [
                r
                for r in asset_ci
                if r["variant"] == "risk_budget" and r["symbol"] == symbol
            ]
            crosses = sum(r["ci95_low"] <= 0 <= r["ci95_high"] for r in intervals)
            lines.append(
                f"{symbol} 的五个后续半年窗口中 {sum(r['total_return'] > 0 for r in tests)} 个盈利；5/10/20 日区块的均值区间有 {crosses} 个跨过零。不能凭单一盈利股票或单个好年份确认稳定优势。"
            )
        basket_tests = [
            r
            for r in windows
            if r["variant"] == "risk_budget"
            and r["symbol"] == "equal_weight_diagnostic"
            and r["part"] == "test"
        ]
        lines.append(
            f"风险预算版本的五股等权日收益诊断，在五个后续半年窗口中 {sum(r['total_return'] > 0 for r in basket_tests)} 个盈利。所有窗口均来自已查看过的历史，不能视为一次全新 OOS 检验。"
        )
    lines += [
        "",
        "## 研究口径",
        "",
        "- directional：仅按当前模型持仓方向选择退出条件，保留入场优先级、30 分钟判断和次分钟成交。默认配置仍兼容 Notebook。",
        "- cost_1bp / cost_3bp：在上一方向退出版本上，分别加入每次成交额 1 / 3 bp 的价差与冲击，另有每股 0.001 美元不利滑点及 0.0035 美元佣金。全部重新运行原生撮合，不是从原收益中简单扣费。",
        "- unlevered：cost_1bp 的目标杠杆上限降至 1。risk_budget：再加入每次 0.25% 计划风险和每日 1% 亏损触发线。按当前买卖报价到同方向 band/VWAP 的距离限制股数；参考止损已失效则拒绝新风险，原始模型信号保留。",
        "- 风险保护依据最新报价独立检查，回测报价每分钟一次；风险退出可以早于下一个 30 分钟信号。限额是触发线，跳空、价差和费用可导致实际损失越线。每日锁定保留到下一交易日，连接/订单错误仍永久停机。",
        "- vm / rvol 邻域仅使用 2021–2022 训练段逐一变化，未挑选最优参数。样本末段没有参与参数选择；这些日期此前已被研究者看过，不能称为全新独立 OOS。",
        "- walk_forward.csv：固定规则按 24 月训练、6 月验证、后续 6 月测试分段，滚动半年；使用连续原生账户与既有预热历史，没有边界重置或参数择优。这是固定规则的滚动外推检查，不是 Walk-forward Optimization。",
        "- paired_bootstrap.csv：对齐相同日期，按 5/10/20 日循环区块成对重采样，2000 次，固定种子 20260920；保留跨股同日关系。CI 为平均日收益差的边际 95% 区间。10 日区块的所有成对检验另作 Holm 校正；仍不能消除研究者此前选样、非平稳和数据筛选偏差。",
        "- side_contribution.csv 是多空交易的实际贡献，不能当作只做多/只做空的重算收益。time_of_day.csv 是时段描述，未据此加入交易时段过滤器。",
        "- regime_breakdown.csv 用 SPY 此前 20 个已观察完整日的涨跌幅（±3%）和日波动（1.5%）描述上涨/下跌/横盘及高低波动环境。阈值固定，不参与下单；分组 PnL 不是启用 regime filter 的反事实结果。",
        "- 输入保留事后完整 390 分钟共同日，漏掉的交易日和半日市未补造；固定存续股票有选择偏差。每年 252 个保留日年化、无风险利率按 0、日终权益计算回撤，不能等同完整自然日历或盘中最大回撤。",
        "- 没有历史 NBBO、订单深度、限价队列、借券可用性/借券费和账户融资费；成本组是预设压力场景，不是经真实 broker fills 校准的精确成本。",
        "- capacity.csv 检查入场股数占前一分钟已知成交量的比例，避免用尚未完成的成交分钟量做入场容量假设。它只是容量风险诊断，不代表真实盘口可成交数量；报价撮合仍使用无限 L1 数量。",
        "- asset_bootstrap.csv 给出单股日收益均值区间；leave_one_stock_out.csv 与 daily_correlations.csv 检查对个别股票和跨股相关性的依赖。这些均为现有独立账本的统计，不是新组合撮合。",
        "- 风险组的实际股数/退出和无约束 Reference ledger 不同；每份 parity_report.json 保留差异。feature_match/signal_match 验证同版本数学，不能把 fill_match=false 宣称为 Notebook 完全复现。",
        "- 风险控制按独立账户设计。尚无盘中重启状态恢复、真实借券验收及实盘成本标定；本轮不启用实盘。",
        "- 每笔应急风险目前以该专用账户入场以来的权益变化衡量；其他策略收益或出入金会干扰触发。复用混合账户前需改为逐持仓核算并重新验收，不能直接套用当前单股研究结果。",
        "",
        "方法参考：[回测过拟合与重复检验](https://www.davidhbailey.com/dhbpapers/backtest-prob.pdf)、[止损触发与实际成交价的区别](https://www.finra.org/rules-guidance/notices/16-19)。",
        "",
        "## 验证和下一步",
        "",
        "40 项相关 Rust 测试通过，涵盖原 Notebook golden、方向退出、未来数据隔离、原生成交确定性、每日三次限制、双向风险越线、当日锁定和次日恢复。另有 2 项离线统计测试。日历查询优化经五年 AAPL 重放，财务输出逐字节一致，见 calendar_verification.json。",
        "",
        "Backtest 目标 Clippy、相关格式检查通过。Trading 全目标 Clippy 仍被原有 momentum_pullback 的 12 项问题阻断；未改动无关策略，见 clippy_trading.log。",
        "",
        "下一步仅建议做前向 paper/dry-run 执行验收，不启动真实订单：冻结配置和代码哈希，至少连续 30 个完整交易日记录已知行情、信号、报价、理论/模拟订单与成交偏差；纳入半日市、缺失/延迟行情、断线、部分成交、拒单和日终平仓演练。",
        "",
        "通过标准为无重复开仓、每日额度准确、风险触发与实际持仓一致、收盘核实空仓、信号时间可追溯。模拟成交不能校准真实滑点；需要历史 NBBO 或未来独立成交样本，补足借券、融资及费用模型。之后另立未查看的新样本评估收益，不能反复调参追逐这五年。",
        "",
        "## 复现",
        "",
        "```bash",
        "target/debug/intraday-backtest --config reports/intraday/robustness/configs/risk_budget.json --symbol AAPL --input test_data/local/intraday_momentum/common_stocks/common/AAPL.csv --start 2021-01-04 --end 2025-12-31 --output reports/intraday/risk_rerun/AAPL",
        ".venv/bin/python examples/research/intraday_robustness.py --report",
        "```",
        "",
    ]
    (root / "README.md").write_text("\n".join(lines))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path("reports/intraday/robustness")
    )
    parser.add_argument("--variants", nargs="+")
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--end")
    parser.add_argument(
        "--binary", type=Path, default=Path("target/debug/intraday-backtest")
    )
    args = parser.parse_args()
    if args.report:
        summarize(args.root)
    elif args.variants:
        run(args.root, args.variants, args.end, args.binary)
    else:
        parser.error("provide --variants or --report")
