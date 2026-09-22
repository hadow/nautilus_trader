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

"""Audit existing native fills and summarize the fixed five-stock experiment."""

import argparse
import csv
import json
import math
import statistics
from collections import Counter
from datetime import datetime
from decimal import Decimal
from pathlib import Path
from zoneinfo import ZoneInfo


def summarize(root):
    plan = json.loads((root / "plan.json").read_text())
    manifest = json.loads((root / "data_manifest.json").read_text())
    rows, yearly, audits, coverage = [], [], {}, []
    timezone = ZoneInfo("America/New_York")

    def session(timestamp):
        return (
            datetime.fromtimestamp(int(timestamp) // 10**9, timezone).date().isoformat()
        )

    for symbol in plan["symbols"]:
        directory = root / symbol / "full"
        metrics = json.loads((directory / "metrics.json").read_text())
        parity = json.loads((directory / "parity_report.json").read_text())
        report = json.loads((directory / "strategy_report.json").read_text())
        assert metrics["symbol"] == symbol and not report["errors"]
        for key in (
            "feature_match",
            "signal_match",
            "entry_exit_price_match",
            "entry_exit_timestamp_and_side_match",
        ):
            assert parity[key], (symbol, key)
        with (directory / "trades.csv").open() as stream:
            trades = list(csv.DictReader(stream))
        counts = Counter(session(t["entry_timestamp"]) for t in trades)
        assert len(trades) == metrics["number_of_trades"]
        assert max(counts.values(), default=0) <= plan["max_entries_per_day"]
        assert all(
            session(t["entry_timestamp"]) == session(t["exit_timestamp"])
            for t in trades
        )
        position = Decimal(0)
        entries = Counter()
        ids = []
        for fill in report["fills"]:
            if position == 0:
                entries[session(fill["timestamp"])] += 1
            position += Decimal(fill["quantity"]) * fill["side"]
            ids.append(fill["order_id"])
        assert position == 0 and entries == counts
        assert len(ids) == len(set(ids)), (
            "duplicate order IDs in complete simulated fills"
        )
        with (directory / "daily_returns.csv").open() as stream:
            daily = list(csv.DictReader(stream))
        assert len(daily) == metrics["sessions"]
        source = manifest["files"][symbol]
        if source["comparison"] == "primary":
            assert [d["date"] for d in daily] == manifest["sessions"][14:]
        metadata = json.loads(Path(source["source"]).with_suffix(".json").read_text())
        coverage.append(
            {
                "symbol": symbol,
                "raw_rows": metadata["source_manifest"]["rows"],
                "complete_days": source["source_sessions"],
                "used_days": len(daily),
                "coverage_vs_aapl": source["coverage_vs_aapl"],
                "comparison": source["comparison"],
            }
        )
        rows.append(
            {
                "symbol": symbol,
                "first_date": daily[0]["date"],
                "last_date": daily[-1]["date"],
                **metrics,
                "max_daily_entries": max(counts.values(), default=0),
                "days_with_three_entries": sum(n == 3 for n in counts.values()),
                "long_trades": sum(int(t["side"]) == 1 for t in trades),
                "short_trades": sum(int(t["side"]) == -1 for t in trades),
            }
        )
        audits[symbol] = parity
        # Reporting only: these are returns already realized by the native account ledger.
        for year in sorted({d["date"][:4] for d in daily}):
            returns = [float(d["return"]) for d in daily if d["date"].startswith(year)]
            vol = statistics.stdev(returns)
            yearly.append(
                {
                    "symbol": symbol,
                    "year": year,
                    "sessions": len(returns),
                    "return": math.prod(1 + r for r in returns) - 1,
                    "sharpe": statistics.mean(returns) / vol * math.sqrt(252)
                    if vol
                    else None,
                }
            )
    for name, data in (("summary", rows), ("yearly", yearly), ("coverage", coverage)):
        with (root / f"{name}.csv").open("w", newline="") as stream:
            writer = csv.DictWriter(stream, fieldnames=list(data[0]))
            writer.writeheader()
            writer.writerows(data)
    (root / "verification.json").write_text(
        json.dumps(
            {
                "native_runs": len(rows),
                "entry_limit": plan["max_entries_per_day"],
                "max_observed": max(r["max_daily_entries"] for r in rows),
                "all_intraday_flat": True,
                "unique_order_ids": True,
                "parity": audits,
            },
            indent=2,
        )
        + "\n"
    )
    lines = [
        "# 五只普通股票：每日最多三次开仓",
        "",
        "最终标的：AAPL（科技）、JPM（金融）、XOM（能源）、BA（工业）、PFE（医疗）。回测前仅按数据质量调整名单，未按收益选择。",
        "",
        "2021-01-04 至 2025-12-31，最近六年以内的五个完整年度。原生 Rust Strategy + NautilusTrader BacktestEngine；每只股票独立 100,000 美元账户连续运行，资金逐日复利，不是五股组合。前 14 个完整日仅作预热。",
        "",
        "## 连续回测结果",
        "",
        "| 股票 | 累计收益 | 年化收益 | Sharpe | 最大回撤 | 完整交易数 | 多 / 空 | 单日最多开仓 |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for r in rows:
        label = r["symbol"] + (
            "（稀疏样本）" if r["symbol"] in manifest["sparse_symbols"] else ""
        )
        lines.append(
            f"| {label} | {r['total_return']:.2%} | {r['annualized_return']:.2%} | {r['sharpe']:.3f} | {r['max_drawdown']:.2%} | {r['number_of_trades']} | {r['long_trades']} / {r['short_trades']} | {r['max_daily_entries']} |"
        )
    lines += [
        "",
        "## 逐年收益",
        "",
        "| 股票 | 2021 | 2022 | 2023 | 2024 | 2025 |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for symbol in plan["symbols"]:
        values = [f"{y['return']:.2%}" for y in yearly if y["symbol"] == symbol]
        lines.append(f"| {symbol} | " + " | ".join(values) + " |")
    lines += [
        "",
        "2021–2024 与 2025 按时间先后分段观察，约为 80% / 20%；没有参数搜索，也没有在分界处重置账户或丢弃 2024 年预热历史。2025 列来自同一原生账本，仅为固定参数末段表现；当前预选存续股票及事后共同日筛选仍有选择偏差，不能据此宣称独立样本外 alpha。",
        "",
        "## 开仓限制和成交口径",
        "",
        "- 每只股票、每个 ET 交易日最多 3 次新开仓尝试。多空合计；反手先平仓，再消耗一次新开仓机会。持有同方向仓位不重复计数。",
        "- 本轮验证连续运行的回测实例。开仓计数尚未持久化，盘中重启实盘实例前必须从当日订单恢复额度；不能把本轮检查当作实盘重启安全验收。",
        "- 止损、反手的平仓部分及日终清仓始终允许；达到上限后不为凑够次数强制交易。提交前占用次数，拒单或撤单不退还额度，部分成交不重复计数。下一交易日重置。",
        "- `max_entries_per_day: 3` 是执行限制，原始数学信号保留。实际逐笔账本和订单 ID 已核对上限、跨日清零、日内平仓及重复订单。",
        "- 固定 Notebook 参数：lookback 14、VM 1、RVOL 1、目标日波动 3%、最高目标杠杆 4；每 30 分钟判断，信号确认后的下一分钟 open 成交，15:59 清仓。波动率仅用此前已完成日。",
        "- 每次成交每股佣金 0.0035 美元、滑点费用 0.001 美元，包括平仓。未模拟真实价差、冲击、借券可得性、借券费或融资本息；不是可直接兑现的实盘净收益。股票财报和突发事件的分钟内滑点可能显著更大。",
        "- 年化为保留交易日的 252 日口径，回撤来自日终权益。4 倍是目标杠杆上限，持仓价格变动后的实际杠杆可能超过它。",
        "",
        "## 数据质量与缓存",
        "",
        "| 股票 | Alpaca 原始分钟数 | 完整日数 | 实际评估日数 | 相对 AAPL 覆盖 |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for r in coverage:
        lines.append(
            f"| {r['symbol']} | {r['raw_rows']:,} | {r['complete_days']} | {r['used_days']} | {r['coverage_vs_aapl']:.2%} |"
        )
    lines += [
        "",
        "Alpaca raw 1Min、原始每分钟 VWAP；请求保留 Notebook 未显式指定 feed 的方式。仅使用 ET 09:30–16:00 连续 390 根的完整交易日。半日市及缺失分钟的日期被排除，不补造分钟。先按 AAPL 完整日覆盖率至少 95% 决定共同样本成员，再取交集；稀疏标的单列诊断。排除日期同时改变历史特征窗口，这是兼容研究口径的限制。",
        "",
        "原始 gzip 分页与请求 manifest：`test_data/local/intraday_momentum/common_stocks/<SYMBOL>/alpaca/`。归一化数据为 `bars.csv`，共同日期输入为 `common/<SYMBOL>.csv`。缓存按标的及起止日期隔离，支持分页续传；重复相同请求复用缓存。下载两路、每路间隔至少 1.3 秒，并对 429 退避，保留 2 GiB 磁盘余量。",
        "",
        "旧 ETF 实验的 `multi_asset/common/*.csv` 已无损压缩为 `.csv.gz`，节省约 487 MiB；SHA256 校验记录见 `cache_compression.json`。原始行情和旧报告保留，旧 CSV 可用 `gzip -dk <文件.csv.gz>` 恢复。",
        "",
        "全部 7 只股票的原始缓存已在禁止网络请求的条件下复读通过，新增请求为 0，见 `cache_verification.log`。",
        "",
        "未采用的 CAT、UNH 归一化 `bars.csv` 也已无损压缩为 `bars.csv.gz`，校验见 `unused_data_compression.json`；最终五只股票的回测输入保持原路径可直接运行。",
        "",
        "CAT 和 UNH 的完整日仅 564、676 天（约为 AAPL 的 45%、54%），因此在任何新股票收益回测前替换为 BA、PFE。它们的下载缓存仍保留，本次实际下载了 7 只、最终回测 5 只，过程记录见 `plan.json`。没有将稀疏样本包装成完整五年结果。",
        "",
        "本轮使用 raw 价格，不引入分红回溯调整。选定股票官方拆股资料未显示 2021–2025 年拆股，PFE 的 Viatris 分拆在 2020 年、本轮范围之前。日内不跨夜，但除息日的前收盘及历史波动特征仍可能受影响。来源：[Apple](https://investor.apple.com/faq/)、[JPM](https://jpmorganchaseco.gcs-web.com/ir/shareholder-information/stock-split-history)、[XOM](https://investor.exxonmobil.com/stock-info/stock-split-history)、[Boeing](https://investors.boeing.com/investors/investor-resources/)、[Pfizer](https://investors.pfizer.com/Investors/stock-dividend/dividend-split-history/default.aspx)、[Viatris 分拆](https://www.pfizer.com/news/press-release/press-release-detail/pfizer-completes-transaction-combine-its-upjohn-business)。",
        "",
        "## 验证与复现",
        "",
        "本轮 30 项 Rust 检查（策略单元、公式/时序、未来数据隔离、原生成交与确定性）和 2 项离线数据测试通过。新开仓限制另有反手、第四次拦截、平仓保留、次日重置测试。Rust backtest 目标的 Clippy、Ruff、格式检查通过；包含其他策略依赖的广义 Clippy 被已有 `momentum_pullback` 的 12 项告警阻断，见 `clippy.log`。修改文件清单见 `files_changed.txt`。",
        "",
        "决策时点的特征、原始信号，以及全部成交的方向、时间、价格逐项与因果 Rust Reference Model 比较。购买力约束会改变股数和资本路径，完整成交一致性以每份 `parity_report.json` 为准；`fill_match=false` 不能解读为完全复现 Notebook。",
        "",
        "```bash",
        "target/debug/intraday-backtest \\",
        "  --config examples/research/intraday_common_stocks.json \\",
        "  --symbol AAPL \\",
        "  --input test_data/local/intraday_momentum/common_stocks/common/AAPL.csv \\",
        "  --start 2021-01-04 --end 2025-12-31 \\",
        "  --output reports/intraday/common_stocks_rerun/AAPL/full",
        "```",
        "",
        "其余股票同时替换 `--symbol` 和输入路径。下载日期参数使用 `--start 2021-01-04 --end 2026-01-01`（UTC 截止）；回测日期参数为 ET 交易日期，含结束日。全部实际命令保存在 `run_manifest.json`。",
        "",
        "完整指标见 `summary.csv`，年度收益见 `yearly.csv`，数据覆盖见 `coverage.csv`，检查见 `verification.json`；每股 `full/` 内保留 native metrics、trades、daily_returns、equity_curve、drawdown、parity、strategy_report 及压缩日志。Python 只下载、整理及汇总账本，生产信号和执行均为 Rust；没有连接交易账户下单。",
        "",
        "数据 API：[Alpaca bars](https://docs.alpaca.markets/us/reference/stockbars)、[限额](https://docs.alpaca.markets/us/docs/about-market-data-api)。",
    ]
    (root / "README.md").write_text("\n".join(lines) + "\n")
    print(f"Validated {len(rows)} native runs; {root / 'README.md'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", type=Path, required=True)
    summarize(parser.parse_args().reports)
