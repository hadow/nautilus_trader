# Notebook → Rust 验收报告

数据与计算日期：2026-09-19。源为用户授权的 Alpaca SPY 1Min 下载；生产运行时不使用 Python。
完整操作入口、配置和架构图见 [中文运行说明](integrations/intraday_momentum.zh-CN.md)。

## 数据证据

原请求区间 2016-01-04T00:00Z 至 2025-12-31T00:00Z，raw adjustment、API 默认 feed，与 Notebook 请求参数相同。
下载 199 页、1,985,051 根，全部落本地压缩缓存；缓存再次读取验证成功，无须重复下载。
按原 Notebook 的 390 根完整日筛选得到 2,484 sessions / 968,760 bars，排除 29 个半日或不完整日。
原 CSV 未提供，供应商可能修订历史；只宣称行数及原代码保存指标复现，不宣称遗失文件逐字节相同。

Notebook SHA256：`2f21c39f5b292643ab8c1eb53e0ceac24c870f642408b871283eeb6737219788`。
源码/配置/golden 文件已扫描，没有复制 Notebook API_KEY 或 SECRET_KEY 的值。

## 原代码收益审计（存在前视，不能用于实盘预期）

| 原 Notebook | Train | Test |
| --- | ---: | ---: |
| 可交易天数 | 1973 | 483 |
| 累计收益 | 288.5923% | 50.4958% |
| 原函数年化收益（原精度） | 19% | 24% |
| 原函数年化波动（原精度） | 14% | 17% |
| 原函数 Sharpe（原精度） | 1.29 | 1.34 |

这些数值重现 Notebook 保存的输出。原收益仍包含信号/区间错位与当日波动率泄漏。
审计文件：`reports/intraday/notebook/notebook_audit.json`，原日收益 CSV 同目录。
Rust `audit::notebook_returns` 在独立 golden 上也复现原 cell 25 日收益，容差 1e-10；不进入 Strategy 的运行路径。

## Rust Nautilus 因果回测

参数固定为 Notebook 最终值：L=14、VM=1、RVOL=1、target vol=3%、目标杠杆上限4。
起始资金每个 split 各 100,000 USD。按之前收益确定杠杆；next-minute open 成交；提前一分钟 EOD 清仓。
每边每股 0.0035 USD 佣金 + 0.001 USD 模型滑点，以每笔成本汇总后取美元分。
研究设置关闭额外 3% 日损/400k 固定 notional 上限，但保留购买力限制；与提供的 paper 安全配置不同。
年化按 252 交易日，Sharpe 无风险为 0，回撤取日终权益；未运行额外 OOS 参数优化。

Train 输入 2016-01-04–2023-12-27（1987 sessions），Test 输入 2023-12-28–2025-12-30（497 sessions）；
各自重置、预热 14 日后可交易 1973 / 483 日。默认信号按完成时钟，非 Notebook 起点标签时钟。

| 指标 | Rust Train | Rust Test |
| --- | ---: | ---: |
| Total Return | 100.7092% | 15.9851% |
| Annualized Return | 9.3063% | 8.0441% |
| Annualized Volatility | 14.1271% | 15.7458% |
| Sharpe | 0.699947 | 0.568773 |
| Sortino | 1.159423 | 0.990351 |
| Max Drawdown（日终） | 17.1308% | 10.6489% |
| Calmar | 0.543248 | 0.755393 |
| Win Rate | 49.3834% | 49.6614% |
| Profit Factor | 1.148748 | 1.109427 |
| Average Trade USD | 56.45 | 36.08 |
| Round trips | 1784 | 443 |
| Turnover / initial equity | 15201.874544 | 3281.423425 |
| Transaction Costs USD | 21,119.26 | 2,541.66 |
| Maximum observed exposure USD | 815,150.87 | 485,626.72 |
| Maximum observed leverage | 4.170448 | 4.108022 |
| Final Equity USD | 200,709.25 | 115,985.13 |

该收益差异同时来自因果时序、波动率信息边界、固定股数 ledger、EOD 费用及执行约束，不能将全部差异归因于某一个 bug。
目标杠杆 4 不等于持仓期间硬封顶；最大观测值来自实际 fills 与每分钟 close，未包含分钟内极值。
费用包含模型滑点近似，未宣称包含真实 Longbridge 历史收费、借券费、逐时 spread 或 market impact。
完整结果位于 `reports/intraday/train` 和 `reports/intraday/test`，均有七种所要求的输出文件。

## Reference vs Nautilus

| 检查 | Golden | Train | Test |
| --- | ---: | ---: | ---: |
| 有效特征决策数 | 192 | 23676 | 5796 |
| Feature match | True | True | True |
| Signal match | True | True | True |
| 持仓方向 match | True | True | True |
| 入退场时刻/方向 match | True | True | True |
| 入退场价格 match | True | True | True |
| 总 fills | 26 | 3568 | 886 |
| 数量完全一致 fills | 26 | 918 | 284 |
| Fill 全字段 match | True | False | False |
| 日收益相关系数 | 0.9999999999985838 | 0.9999892210259774 | 0.999957482506482 |
| 权益相关系数 | 0.9999999999992837 | 0.9999991714434023 | 0.9999816625532291 |

Golden 为 30 个真实日、11,700 bars，其中 16 日可交易，13 round trips / 26 fills。
其信号时刻、方向、数量、成交价和费用与独立 Notebook-derived expected 全部一致；账户记分导致最终权益与无分舍入 ledger 差 0.0011 USD。
长样本的数量/费用 **不是**全部一致，报告保留 `fill_match=false`，未把高度相关冒充逐笔相等。
例如 2024-01-29 的理论数量822与执行数量816不同：执行按当前价约束购买力，后续资金路径也会不同。
对账保留前 20 个 divergence。纯 reference 的期末权益分别 200882.7517 / 116334.2404 USD，
Nautilus 分别 200709.25 / 115985.13 USD。

## 验证与限制

已运行的测试与构建明细记录在 `reports/intraday/verification.json`；原生 golden 集成测试显式单独运行。
测试覆盖公式、原始收益审计、长期/短期信号、入场优先级、零均量 RVOL、DST/半日市、精确 sizing、日损限制、
重复目标/挂单、退出拒绝、EOD 幂等、供应商 VWAP 序列化/精度、逐股费用和未来扰动。

Targeted Rustfmt 与离线脚本 Ruff 检查覆盖本次文件。整个 examples feature 的 Clippy 被此前存在的
`momentum_pullback` lint 问题阻断；不声称整个 workspace 的 cargo test / pre-flight 已通过，也未修改无关模块。

## Live readiness

默认 dry-run；paper 只安装 Nautilus sandbox execution；live 只有显式 mode 与账户标识确认才安装 broker execution。
启动参数检查验证了前两者无 broker 下单路由，未确认 live 立即拒绝。
Longbridge 实际 dry-run 预热返回 `301607: history candlestick symbol count out of limit, requested:1000/limit:1000`，
在下单路由和策略启动前停止。本轮没有真实下单，也没有发生真实行情 sandbox 成交。

目前不能宣称 live-ready：额度、完整交易日 paper、断线重连/补数据、持久化仓位恢复、真实账户身份、
broker 购买力与借券/费用仍需按运行说明的清单验收。遇到已有 SPY exposure 的重启会 HALT，不自动认领。
参数搜索入口已实现但本轮未运行新的 162 组优化，没有新的 walk-forward/优化收益可报告。
原策略本身和原作者参数选择仍有 data snooping 风险；完整日事后筛选也是未消除的偏差。

## 本轮文件范围

完整清单：`reports/intraday/files_changed.txt`。其中已有 intraday 文件属于修订，不表示这些未跟踪文件最初都由本轮创建。
原工作区其他 SLC、Wyckoff、momentum_pullback 等未提交修改不属于本轮清单，未重置或提交。

| 范围 | 文件 |
| --- | --- |
| Reference / audit / Nautilus strategy | `crates/trading/src/examples/strategies/intraday_momentum/{reference,audit,model,config,strategy,mod,tests}.rs` |
| 原生回测与优化 | `crates/backtest/bin/intraday/{mod.rs}`、`intraday_backtest.rs`、`intraday_optimize.rs` |
| Longbridge 原生入口/日历/配置 | `crates/adapters/longbridge/bin/intraday.rs`、`examples/intraday_calendar.rs`、`examples/intraday_native.json`；旧 node 示例复用日历 |
| 数据字段与解析 | `crates/model/src/data/bar_vwap.rs`、`data/mod.rs`、Longbridge `src/common/parse.rs`、`src/data.rs` |
| 共享费用模型 | `crates/execution/src/models/fee.rs`：原 PerContract 模型新增 Decimal rate 构造，原 Money 构造保留 |
| Cargo 接入 | backtest / longbridge / trading manifests、strategy mod 注册、Cargo.lock |
| 离线工具 | `examples/research/intraday_{alpaca_download,golden,notebook_audit}.py`；`intraday_backtest.json` |
| Tests / fixtures | `crates/trading/tests/intraday_*`、`lookahead.rs`、`tests/data/intraday_momentum/*` |
| 文档 | repository_audit、strategy_spec、notebook_issues、strategy_parity、lookahead_audit、parity_report、中文运行说明 |

附加成本模式 smoke test 只在训练期最早 golden 上运行，没有再次利用 OOS 选参：
spread=1bps、impact=.5bps、每股 slip=.001，16 天累计 -2.7612%，13 round trips；
现金佣金97.62 USD、成交价摩擦554.6902 USD，总模型交易成本652.3102 USD。
它验证配置的成本进入撮合和报告，不是经过 broker 历史报价校准的收益预测。
