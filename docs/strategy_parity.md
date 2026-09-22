# Notebook 与 Rust 的一致性边界

数学规格见 [strategy_spec.md](strategy_spec.md)，问题清单见 [notebook_issues.md](notebook_issues.md)。
这里明确区分三项验收，避免把带前视的原收益当成可交易收益。

| 层次 | 输入/时钟 | 验收 | 用途 |
| --- | --- | --- | --- |
| 原 Notebook 审计 | 原分钟起点标签、原 cell 25 收益计算 | 原 train/test 指标；Rust audit 对 golden 日收益误差 <1e-10 | 解释和复现原输出，禁止用于下单 |
| 纯 Rust reference | 同分钟历史特征；显式 Long/Short/Flat | sigma、bands、provider AVWAP、RVOL、入场优先级、止损标志 | 所有运行模式共用的数学内核 |
| Nautilus Strategy | 已完成 bar → 目标 → 下一分钟 open quote / 实际 broker fill | 特征、信号、方向、入退场、数量、费用、资金曲线 | 原生回测、dry-run、sandbox paper、live 接口 |

## 明确的差异

- `Timing::NotebookLabel`：index=30 是 10:00 起点、10:01 完成的 bar。
- `Timing::SessionClose`：默认；在 10:00 完成时刻用 09:59 起点 bar 决策。最后新信号为 15:30。
- 两种时钟都只用已完成数据。`--timing notebook` **不会**启用原 Notebook 的前视收益算法。
- 原 Notebook 的 `exec_minute_index` 未用于收益对齐；独立 `audit::notebook_returns` 保留该 bug 作为离线审计。
- 原 cell 25 的波动率含当日收盘；生产使用此前 14 个已知日收益，不足时杠杆为 1。
- 原收益先 compound 分钟百分比，再乘杠杆；Nautilus 根据固定股数、实际 fills 和 USD 账户记账。
- 默认 15:59（日历收盘前 1 分钟）清仓；收盘后验证 flat。原 Notebook index==390 从未执行。
- 所有成交均计费用，包括反手的平仓、开仓两笔，以及日终平仓。每股费率用 Decimal，汇总后再取 USD 分精度。
- 原 Notebook 理论股数不受当前价格购买力限制；Nautilus 执行层取更小的可承受数量。这不修改原始信号。
- 长样本 reference 现金 ledger 保留小数分；Nautilus Money 按美元分记账。交易 PnL 与账户 PnL 可有数分钱差异。

## 成交时间

回测每根 bar 提供两个事件：分钟 open quote 的 `ts=start+1ns`，已完成 VWAP bar 的 `ts=end`。
Strategy 在 close 产生信号后等待下一 quote，撮合由现有 BacktestEngine/ExecutionEngine 完成。
因此持仓不会取得生成信号那一分钟的 open→close 收益。1ns 仅用于相同边界的事件排序，不模拟网络延迟。
真实行情模式在 confirmed bar 到达后发单；预期价是当时可见 bid/ask，实际价格以 Fill 为准，不能称为 next open 保证。

## Golden 与源数据

用户授权后按 Notebook 请求参数从 Alpaca 下载 SPY raw 1Min，共 199 页、1,985,051 行，缓存 gzip。
原 CSV 已丢失；下载行数和 Notebook 保存的 train/test 指标一致，但不能验证原 CSV 的逐字节身份。
provider revision、默认 feed 的历史修订可能造成差异，provenance 保留这一限制。

`tests/data/intraday_momentum` 是 2016-01-04 至 2016-02-17 的 30 个完整交易日、11,700 根一分钟 bar。
前 14 日预热，16 日可交易。独立 expected 来自离线执行指定 Notebook 纯函数，以及明确修正的 next-open/固定股数 ledger。
生产程序从不运行 Python，也不调用 Python 子进程。离线 golden 的 Python Decimal 费用汇总后按分舍入，避免 4.995 被浮点误写成 4.99。

默认完整样本沿用 Notebook 的“只保留 390 行完整日”选择，排除了 29 个半日市或缺失日。
这是一个事后样本选择限制；不能据此声称实际部署会有同样的数据完整性。
实际 Longbridge session 来自 trading_days/half_trading_days，使用 America/New_York 和真实 DST。
半日市之后，同分钟历史缺少该分钟时，reference 不伪造 sigma，也不会发出该分钟信号。

## 对账方法

`parity_report.json` 比较每个有效决策的特征、目标和杠杆；逐笔比较 signal timestamp、fill timestamp、方向、价格、数量、成本。
保留前 20 个 divergence，计算日收益和权益相关系数。它明确区分数量差异与信号/价格错误。
`--audit-existing` 只读取已有执行报告并重算审计/敞口统计，不重新运行交易引擎或选择参数。

相关性高不是逐笔一致：长样本的 `fill_match=false` 和数量差异会保留。详见 [parity_report.md](parity_report.md)。

## 方向退出与账户风险研究变体

`model.directional_stops: true`（原生 Strategy 配置为 `directional_stops`）仅改变退出条件的选取：模型持多时只检查 `long_stop`，持空时只检查 `short_stop`。入场仍优先于止损，RVOL 仍仅限制新的原始突破信号；持仓没有触发自身方向止损时，RVOL 降低不会单独强制退出。默认 `false` 保留原 Notebook 行为，离线 literal Notebook 审计拒绝该变体，避免误标一致性。

原生回测现可配置 `max_daily_loss` 和可选 `risk_per_trade`。前者达到后平仓、锁定当日，下个交易日恢复；订单失败、失联等异常仍停机。后者按当前入场报价与同方向 band/VWAP 的距离限制股数，并在已观察到的报价上监控入场以来账户权益损失，独立于 30 分钟信号时钟。计划止损位已在错误方向时，执行层拒绝增加风险，但保留原始模型信号供审计。

账户风险模式以专用账户为前提；其他策略交易或出入金会影响权益触发。它不是 broker 托管止损，也不能保证损失上限。回测只在合成的每分钟 open 报价检查风险，没有模拟分钟内最低价/最高价、断线时保护单或真实报价深度。风险触发后使用当前已知报价成交，正常策略信号仍等待下一分钟 open。因此风险退出不应强行套用“signal timestamp + 1ns”的普通入场断言。

风险退出或风控拒绝入场后，实际空仓时必须等下一次有效原始突破信号，不能把模型残留的“继续持仓”目标当作新开仓信号。

`risk_events` 记录触发时间、原因、权益及美元触发额度。`parity_report.json` 保留风控导致的仓位/退出差异，并分别检查数学信号和成交是否早于信号；无约束 Reference ledger 没有账户风控，不能要求这两条资金曲线相等。

分阶段配置、输入哈希、成对区块 bootstrap、固定规则滚动外推结果见 [研究报告](../reports/intraday/robustness/README.md)。研究结论不自动改变现有 paper/live 的配置。
