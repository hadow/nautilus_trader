# Dynamic Grid 网格候选选股

## 定位与边界

这个选股器回答「哪些股票值得交给现有 Dynamic Grid 做进一步研究」，不回答「现在买什么」。
核心用 Rust 实现，复用现有 `RegimeDetector`、`GridEngine`、间距计算和组合收益相关性。
没有新增交易框架、交易客户端、自动调仓或另一套风控。现有交易配置不变。

数据路径是：Longbridge 只读行情 → 冻结 JSON 快照 → 离线评分 → 候选报告。
候选是策略的上游研究输入；真正启用时仍需已有组合配置、共享现金预留、订单对账和实时风控。
淘汰一个候选不等于撤单或卖掉该股票的历史库存。

本版是可执行的筛选假设，不是已证明盈利的选股策略。未完成选股层的历史组合回放、样本外检验或实盘验证。

## 为什么不是涨幅榜或波动率榜

网格需要的是扣费后有意义的往返价差，不是单纯涨得多、跌得多或波动率高。
单边下跌也会产生很多买点，已完成周期还可能全赢，但未平库存仍可能大幅亏损。
因此同时观察往返、方向性、成本余量和未完成下跌，不用「已完成周期胜率」单独选股。

采用两层数据，避免把日线 ATR 错当成生产分钟网格的 ATR：

- 最近 60 个日收益区间：计算路径、回撤、跳空占比和相关性。
- 与配置 `bar_type` 相同周期的已完成 Bar：通过生产指标和间距函数得到实际网格间距。

当前状态不是 `Range` 时附上提示，不在选股层再加一道统一禁买开关。
这不会关闭交易策略已有的趋势或风险规则。

## 先检查能否交易，再排序

数据、成本和最小交易单位属于有效性检查：

- Bar 必须属于同一标的、同一周期，时间严格递增，OHLC 有效且已完成。
- 历史不足、指标未预热、报价缺失、报价过期或未来报价均不冒充有效数据。
- 复用标的配置的最低价格、最大价差限制，并检查日均成交额代理值。
- 复用真实 tick、lot、网格层数和 sizing；所有买层取整为零时不入选。
- 用一个实际可负担网格买卖对检查双边费用、滑点、全价差及安全边际。

成本探针使用：

```text
gross = exit_price × quantity − entry_price × quantity
rate = max(maker_fee, taker_fee) + commission
fees = max(entry_notional × rate, minimum_commission_per_order)
     + max(exit_notional × rate, minimum_commission_per_order)
costs = fees + (entry_notional + exit_notional) × slippage
      + spread × quantity + entry_notional × minimum_profit_margin
edge = gross − costs
```

`edge <= 0` 时报告 `GRID_DISABLED_BY_COST`。价差和滑点同时扣除是保守探针，不代表实际成交账单。
最低费用默认零只是未配置假设；有最低收费的账户必须填入实际值。
`grid_capital` 是单标的可交易性预算探针，不表示每个候选都获得这笔资金，也不是目标仓位。

## 评分口径

每项归一化到 0–1，四项等权，最终为 0–100 分：

1. **往返机会**：收盘价自局部高点下跌一个有效间距，再自低点回升一个间距，确认一次反弹。
   达到 `target_rebounds` 次计满分；一次跳空最多完成一次状态变化，不虚构中间价位的逐层成交。
2. **非方向性**：`1 − abs(last_close − first_close) / sum(abs(close_change))`。
   完全不动不算优质震荡，得零分。
3. **成本余量**：`edge / gross`，衡量理论价差被成本吃掉多少。
4. **库存路径质量**：`(1 − MDD) × (1 − gap_share) × (1 − unresolved_days / lookback)`。
   `gap_share = sum(abs(open − previous_close)) / sum(true_range)`。

这里的「反弹」不是模拟成交，更不是已经盈利的完整网格周期：价格可能反弹一个间距后仍低于原买价。
统计当前间距在过去路径上的可容纳性，是决策时已知的描述性特征，不能作为过去交易收益。
未完成下跌、日收盘回撤单独报告，防止只挑已经成功反弹的样本。

跳空只影响软评分，没有「发生 gap 就禁买」规则；相同波幅下更多来自盘中往返的股票得分较高。
这也不是证明隔夜跳空一定更差，只是待检验的偏好。

默认 55 分才有资格入选，最多 5 只，不强行填满。
按分数降序、标的 ID 升序稳定排序，再限制每行业数量和已选标的间的正相关。
相关性复用 `PortfolioRiskManager`，收益区间的起点和终点都必须一致；不足 30 个配对区间时不假设互相独立。
这些是研究池分散约束，不替代交易时按金额检查的行业、相关性和总暴露限制。

## 配置与运行

配置文件为 `crates/adapters/longbridge/examples/dynamic_grid_selector.json`，全部筛选默认参数显式列出。
默认股票池是人工指定的 20 只股票，不是全美股扫描，也不是历史时点成分股全集。
行业是配置分组，需要维护，并非实时拉取的标准行业分类。

`grid_template` 指向已有 `GridInstrumentFile` JSON；各股票可用 `grid_config` 覆盖。
路径相对选股配置文件。仅复用 `grid`、`bar_type`、`price_increment`、`lot_size`，不读取其中 CSV 行情路径。
默认沿用 AAPL 参数作为公共研究模板，MSFT 使用已有独立配置；这不代表模板已经适合其他股票。
采集时重新映射 `InstrumentId`，不会把 AAPL 的行情或指标共享给其他股票。
行情快照包含每只股票解析后的完整配置，因此后续改模板不会影响旧快照的复算。

在仓库根目录编译 debug 入口：

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-longbridge \
  --features dynamic-grid --example longbridge-dynamic-grid-selector -j2
```

已配置 Longbridge OAuth 后采集。输出目录需预先存在，输出文件不得已存在：

```bash
target/debug/examples/longbridge-dynamic-grid-selector --collect \
  crates/adapters/longbridge/examples/dynamic_grid_selector.json \
  reports/grid-selector-snapshot.json
```

离线排名不需要 OAuth、不联网：

```bash
target/debug/examples/longbridge-dynamic-grid-selector --rank \
  reports/grid-selector-snapshot.json reports/grid-selector-report.json
```

报告保留所有成功采集标的的分数、指标、入选状态、原因和提示；采集失败单独保留在 `collection_failures`。
API 失败、空深度或无有效候选不是成功选出股票，不能只读取 `selected` 而忽略这些字段。
禁止把旧报告视作当前下单许可。

## 数据时点与局限

行情入口复用 adapter 的 OAuth、限流重试、解析器，以及现有美国交易日历、夏令时和半日市逻辑。
只请求 `Intraday` 常规时段、`NoAdjust` 未复权行情。
SDK Bar 的起始时间转为真实完成时间；请求发起时未完成的 Bar 被排除。
日线保守排除纽约当日，最多落后一交易日，避免把半天成交额与整日成交额混算。

深度快照接口没有交易所时间戳。采集器保留深度接收时间，并用最新成交时间代理检查其陈旧程度，
不是证明盘口实时性。盘前、盘后、休市或没有盘口权限时可能输出零候选，应在常规时段重新采集，不能伪造零价差。
顺序采集不是所有股票同一纳秒的横截面；完成采集后统一检查报价年龄，采集太久的早期报价会被拒绝。

未复权数据遇拆股会扭曲路径和相关性，分红也会影响价格收益；本版不声称收益已做总回报调整。
报告统一附带财报、重大新闻及公司行动尚需检查的提示，目前不能判断低开是否有重大利空。
60 日日线的机会计数会遗漏盘中往返，因此只能用于候选筛查，后续应使用相同分钟行情做真实策略回放。

接口依据：[Longbridge 历史 K 线说明](https://open.longbridge.com/docs/quote/pull/history-candlestick)、
[Longbridge Rust QuoteContext](https://longbridge.github.io/openapi/rust/longbridge/quote/struct.QuoteContext.html)。

## 验证与下一项研究

运行核心和采集时间边界测试：

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --example longbridge-dynamic-grid-selector --profile dev -j2 -- --test-threads=1
```

覆盖震荡相对持续下跌、缺失报价、价差过宽、整手为零、最低佣金、未来坏记录隔离、
同时间区间相关性、五标的集中限制、确定性排序和 Bar 未收盘不可用。

下一项实验应冻结本版评分，按周用当时可得数据选池，下周才交给同一个 Dynamic Grid 核心交易。
与固定股票池比较，严格保持资金、风险、费用、滑点与执行假设一致；落选标的保留旧库存正常退出，不直接删除。
报告净收益、库存 MDD、资金利用率、费用、持有时间、选池换手、失败样本和未完成周期。
再做按时间分隔的训练、验证、测试，以及分数、窗口和相关性阈值的邻域敏感性；不要反复查看测试期后仍称其为 OOS。
还需要历史时点股票池和退市样本，才能评估当前手选池的幸存者偏差。

在这些实验完成前，不把分数称作收益预测，不宣称优于固定池，不自动加入 Paper 或 Live 交易。

## 首次工程验收记录

2026-09-24 北京时间，本次实际运行结果：

- **PASS**：debug 编译；Trading 库 658 项、Longbridge 库 42 项、选股采集入口 2 项测试通过。
  合计 702 项通过，既有 4 项忽略测试保持不变；其中本次新增 16 项测试。
- **PASS**：新增入口的定向 Clippy、改动文件的 Rust 格式检查、本文 Markdown 检查。
- **PARTIAL**：Trading 库 Clippy 被原有 13 条 lint 阻挡，位于 `dynamic_grid/files.rs` 和
  `momentum_pullback`；本次没有修改或屏蔽这些问题。未宣称全仓检查通过。
- **PASS**：真实 Longbridge 行情采集 20/20 成功，每只 61 根日线、400 根信号 Bar。
  快照时刻为 `2026-09-23T22:59:48.075Z`；AAPL 日线范围为 2026-06-26 至 2026-09-22，
  信号 Bar 至 2026-09-23 常规时段收盘。
- **PASS**：同一快照离线排名两次，`cmp` 检查报告逐字节一致。
- **PARTIAL**：当时处于盘后，20 只股票均为 `STALE_OR_FUTURE_QUOTE` 中的「陈旧报价」，
  本次候选数为 0，不能据此认为所有股票不适合网格。没有为输出名单而改写时间戳或放宽门槛。
- **NOT RUN**：候选池历史组合回测、收益对照、OOS、Walk-forward、候选自动轮换及交易验收。

本地研究产物为 `reports/grid-selector-20260923T2300Z.snapshot.json`、
`reports/grid-selector-20260923T2300Z.report.json` 和同内容的 `.repeat.json`。
这些是行情研究产物，不含账户持仓或交易凭据。

代码集中在 `crates/trading/src/examples/strategies/dynamic_grid/selection.rs`；
采集和离线入口位于 `crates/adapters/longbridge/examples/node_dynamic_grid_selector.rs`。
除此以外只注册了模块与 Cargo example，并新增选股配置和本文；未修改原交易逻辑、Paper/Live 配置或账户状态。
