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

"""Validate and summarize native Rust reports without recalculating strategy signals."""

import argparse
import csv
import json
from decimal import Decimal
from pathlib import Path


def summarize(root):
    manifest = json.loads((root / "data_manifest.json").read_text())
    plan = json.loads((root / "run_manifest.json").read_text())
    rows = []
    results = {}
    coverage = []
    parity = {}
    new_rows = 0
    new_pages = 0
    for symbol, source in manifest["files"].items():
        results[symbol] = {}
        metadata = json.loads(Path(source["source"]).with_suffix(".json").read_text())
        if symbol != "SPY":
            new_rows += metadata["source_manifest"]["rows"]
            new_pages += metadata["source_manifest"]["pages"]
        coverage.append(
            {
                "symbol": symbol,
                "raw_rows": metadata["source_manifest"]["rows"],
                "complete_sessions": source["source_sessions"],
                "coverage_vs_spy": source["coverage_vs_spy"],
                "comparison": source["comparison"],
            }
        )
        for split in ("train", "test"):
            directory = root / symbol / split
            metrics = json.loads((directory / "metrics.json").read_text())
            audit = json.loads((directory / "parity_report.json").read_text())
            state = json.loads((directory / "strategy_report.json").read_text())
            assert metrics["symbol"] == symbol
            assert not state["errors"], (symbol, split, state["errors"])
            for key in (
                "feature_match",
                "signal_match",
                "entry_exit_price_match",
                "entry_exit_timestamp_and_side_match",
            ):
                assert audit[key], (symbol, split, key)
            with (directory / "daily_returns.csv").open() as stream:
                dates = [row["date"] for row in csv.DictReader(stream)]
            assert len(dates) == metrics["sessions"]
            if source["comparison"] == "primary":
                selected = [
                    day
                    for day in manifest["sessions"]
                    if (day < manifest["boundary"]) == (split == "train")
                ]
                assert dates == selected[manifest["warmup_per_period"] :], (
                    symbol,
                    split,
                )
            results[symbol][split] = metrics
            parity[f"{symbol}/{split}"] = {
                key: audit[key]
                for key in (
                    "feature_match",
                    "signal_match",
                    "entry_exit_price_match",
                    "quantity_match",
                    "fill_match",
                    "daily_return_correlation",
                )
            }
            rows.append(
                {
                    "split": split,
                    "comparison": source["comparison"],
                    "first_date": dates[0],
                    "last_date": dates[-1],
                    **metrics,
                }
            )
    with (root / "summary.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    (root / "summary.json").write_text(
        json.dumps(
            {
                "results": results,
                "coverage": coverage,
                "parity": parity,
                "config": plan["config"],
            },
            indent=2,
        )
        + "\n"
    )

    lines = [
        "# 多标的日内动量策略回测",
        "",
        "使用原生 Rust IntradayMomentumStrategy 和 NautilusTrader BacktestEngine，分别替换标的。每组独立以 100,000 美元起始，训练和测试分别重置账户及历史；不是多资产组合回测。",
        "",
        f"固定分界：{manifest['boundary']}。主比较训练有效期 {manifest['train_evaluation_start']} 至 {manifest['train_end']}，{manifest['train_sessions'] - 14} 天；测试有效期 {manifest['test_evaluation_start']} 至 {manifest['test_end']}，{manifest['test_sessions'] - 14} 天。每段另有 14 个共同交易日预热。未进行参数搜索。",
        "",
        "## 主比较",
        "",
        "| 标的 | 训练年化收益 | 训练 Sharpe | 测试年化收益 | 测试 Sharpe | 测试最大回撤 | 测试完整交易数 |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for symbol in manifest["primary_symbols"]:
        train = results[symbol]["train"]
        test = results[symbol]["test"]
        lines.append(
            f"| {symbol} | {train['annualized_return']:.2%} | {train['sharpe']:.3f} | {test['annualized_return']:.2%} | {test['sharpe']:.3f} | {test['max_drawdown']:.2%} | {test['number_of_trades']} |"
        )
    lines += [
        "",
        "## 测试期成本分解",
        "",
        "以下按本次实际成交股数的账本求和；费前盈亏等于逐笔净盈亏加回成交费用，不是另外运行的零费用复利回测。",
        "",
        "| 标的 | 费前盈亏（美元） | 成交费用（美元） | 逐笔净盈亏（美元） |",
        "| --- | ---: | ---: | ---: |",
    ]
    for symbol in manifest["primary_symbols"]:
        metrics = results[symbol]["test"]
        net = Decimal(metrics["trade_pnl"])
        cost = Decimal(metrics["cash_commissions"])
        lines.append(f"| {symbol} | {net + cost:,.2f} | {cost:,.2f} | {net:,.2f} |")
    lines += [
        "",
        "相同名义仓位下，按股收取的往返费用比例约为 0.009 / 股价，所以低价 ETF 承受更高的相对成本。逐笔账本与账户最终金额可能存在分级舍入差异；后续应使用实际券商费用重新核验。",
        "",
        "全量指标见 [summary.csv](summary.csv) 和 [summary.json](summary.json)。各标的目录内包含逐笔成交、每日收益、权益曲线、回撤、参考模型差异和压缩运行日志。",
        "",
        "## 数据与稀疏样本",
        "",
        "| 标的 | 原始分钟行数 | 完整交易日 | 相对 SPY 覆盖率 | 用途 |",
        "| --- | ---: | ---: | ---: | --- |",
    ]
    for row in coverage:
        label = "共同样本主比较" if row["comparison"] == "primary" else "稀疏样本诊断"
        lines.append(
            f"| {row['symbol']} | {row['raw_rows']:,} | {row['complete_sessions']} | {row['coverage_vs_spy']:.2%} | {label} |"
        )
    for symbol in manifest["sparse_symbols"]:
        metrics = results[symbol]["test"]
        lines += [
            "",
            f"{symbol} 测试仅 {metrics['sessions']} 个有效日，诊断累计收益 {metrics['total_return']:.2%}，Sharpe {metrics['sharpe']:.3f}，最大回撤 {metrics['max_drawdown']:.2%}。其保留日期及历史窗口不同，不参与主表排名；完整报告保留在 `{symbol}/train`、`{symbol}/test`。",
        ]
    removed = json.loads((root / "comparison_dates.json").read_text())["removed_dates"]
    lines += [
        "",
        "数据范围沿用 Notebook：2016-01-04T00:00:00Z 至 2025-12-31T00:00:00Z，最后美股常规交易日为 2025-12-30。Alpaca raw 1Min、供应商 VWAP、与 Notebook 相同的未显式指定 feed 请求。下载后按 ET 常规时段筛选，保留恰好 390 根且时间连续的完整日。",
        "",
        "主比较先要求各标的完整日覆盖率达到 SPY 的 95%，再取共同完整日；该规则在查看新标的收益前确定。DIA 因覆盖率不足单列。主测试相对之前 SPY 483 日样本移除："
        + "、".join(removed)
        + "。SPY 已重跑，所以本表基准不同于原 8.04% 年化结果。",
        "",
        f"新标的共缓存 {new_rows:,} 根、{new_pages} 个 gzip 分页；SPY 复用旧缓存。两路下载，每路请求间隔至少 1.3 秒，支持重试与续传，磁盘保留至少 2 GiB。已禁止网络后复读全部缓存，新增请求为 0，见 `cache_verification.log`。",
        "",
        "原始缓存：`test_data/local/intraday_momentum/multi_asset/<SYMBOL>/alpaca/`；完整日 CSV：同目录的 `bars.csv`；本轮输入：`test_data/local/intraday_momentum/multi_asset/common/<SYMBOL>.csv`。请求信息、输入 SHA256、完整日和排除规则保存在 manifest 文件中。",
        "",
        "## 计算口径和限制",
        "",
        "- 固定参数：lookback 14、VM 1、RVOL 1、目标日波动率 0.03、最高目标杠杆 4；使用当前生产完成时钟、下一分钟 open、此前已完成日波动率、固定股数、15:59 清仓。",
        "- 费用沿用兼容模型：每次成交每股 0.0035 美元佣金及 0.001 美元滑点费用，包括日终清仓；以分钟 open 构造报价，未模拟真实盘口深度、部分成交、额外买卖价差、市场冲击及借券费用，不能当作实盘净收益。",
        "- 年化按保留交易日的 252 日口径计算，回撤为日终权益回撤。共同完整日是事后研究筛选，不是可提前知道的实盘交易日历；半日市和不完整日被排除，历史特征窗口也因此变化。",
        "- 缺少一分钟 bar 可能源于该分钟没有合格成交，也可能是数据问题；本轮未制造零量 bar 或用未来价格补齐。",
        "- 使用 raw 数据，未新增分红、拆股和分拆上市调整。XLK、XLE 在下载前因已知 2025-12-05 拆股被换成 XLI、XLV；未实现公司行动调整的结果须保留这一限制。",
        "- 所有运行的特征、信号、成交时间和价格均通过参考核对；数量及费用仍可因购买力约束、整数股和资本路径不同而不完全一致，不能把 `fill_match=false` 写成完全复现。",
        "- 标的是当前预选的存续 ETF。本轮是在已观察的历史区间做固定参数比较；若据此选择表现最好的标的，还需新的未使用区间验证，不能把优胜者表现当成新的独立样本外证明。",
        "",
        "## 复现单标的测试",
        "",
        "在仓库根目录直接运行 Rust 二进制：",
        "",
        "```bash",
        "target/debug/intraday-backtest \\",
        "  --config examples/research/intraday_backtest.json \\",
        "  --symbol QQQ \\",
        "  --input test_data/local/intraday_momentum/multi_asset/common/QQQ.csv \\",
        "  --start 2023-12-28 --end 2025-12-30 \\",
        "  --output reports/intraday/multi_asset_rerun/QQQ/test",
        "```",
        "",
        "将 QQQ 替换为相应标的，并同时替换输入路径。训练期使用 `--start 2016-01-04 --end 2023-12-27`。`--symbol` 选择模拟合约，不会自动下载或替换输入 CSV。Python 仅用于离线下载、整理和汇总，信号与回测执行均在 Rust 中。",
        "",
        "资料：[Alpaca 历史 bars](https://docs.alpaca.markets/us/reference/stockbars)、[Alpaca 数据限额](https://docs.alpaca.markets/us/docs/about-market-data-api)、[State Street 拆股公告](https://investors.statestreet.com/investor-news-events/press-releases/news-details/2025/State-Street-Investment-Management-Announces-Share-Splits-for-Five-Select-Sector-SPDR-ETFs/default.aspx)。",
    ]
    (root / "README.md").write_text("\n".join(lines) + "\n")
    print(f"Validated {len(rows)} native runs; report: {root / 'README.md'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", type=Path, required=True)
    summarize(parser.parse_args().reports)
