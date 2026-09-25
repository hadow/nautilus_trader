# Dynamic Grid 网格尺度 Regime

## 定位

新增可选 `grid_scale_regime`，不替换默认 **15 分钟＋8 根确认**，不修改 Paper/Live 参数或启动券商交易。
分类、仓位与订单仍使用同一个 Rust `MultiAssetGridStrategy`；没有新建账户、撮合或持久化框架。

这个候选回答的是“过去的路径是否具有当前网格尺度的往返机会”，而不是用更多指标投票预测涨跌。
按 longbridge-quant 的验证约束，工程可用不等于样本外有效；参数为预先指定的研究起点，不是实盘推荐。

## 因果观测

- 复用纽约 09:30 对齐的完整 15 分钟聚合；一分钟缺失时丢弃该桶，不拼凑 OHLC 路径。
- 默认保留最近 **10 个实际观测时段，包括当前时段**。普通隔夜、周末不清空方向记忆。
- 各时段需观测到 09:45 首个完整桶；历史时段至少有 14 个完整桶，兼容正常半日市长度。
  暖机不是“收到 10 根日线”，也不是“自然时间经过 10 天”。
- 未完成桶、重复事件、未来数据均不能推进候选状态；新完整桶未到达时，原有新鲜度检查仍可禁买。
- 先使用当前已冻结网格间距；尚未建网格时使用当时分钟 ATR 计算的候选间距。
  这不是根据未来网格回算历史成交，只是以当前尺度重新观察历史路径。
- ATR、报价、跳空、流动性、账户、未知订单超时及硬风控继续沿用原有时钟和限制。

## 特征与分类

设窗口首尾收盘为 `P0/Pt`，间距为 `s`：

```text
drift_grids = (Pt - P0) / P0 / s
efficiency  = abs(Pt - P0) / sum(abs(Pi - Pi-1))
```

价格、间距、预算和漂移计算使用 `Decimal`。零路径的效率为零。
反弹计数复用选股模块的 `rebound_opportunities`：先从高点跌出一格，再从随后低点反弹一格，计一次机会。
跳空最多推进一次观测状态，不把跳过的价格当作多笔成交。等待时间单位是已观测的 15 分钟桶，不是自然分钟。

判定优先级：

1. 历史不足：`WARMUP / Disabled`，候选不可新增库存。
1. 效率至少 0.6、净漂移至少 2 格：按方向分类 `TrendUp/TrendDown`。
   同方向趋势已建立时，保持门槛降至效率 0.4、距离 1 格；这就是滞回，不再叠加 8 根确认。
1. 窗口高低收盘差尚不够一格：`QUIET / LowVolatility`，禁止新增网格库存。
1. 至少 2 次反弹、效率不高于 0.4：`TWO_WAY / Range`。
1. 其他情况：`UNCERTAIN / Range`，不是操作性故障，不自动变成 Disabled。

分钟级高/低波动保护仍优先。`regime_source` 继续是旧分类器的对照输入，
`decision_regime` 是实际政策标签，`grid_scale` 才是候选特征与原因；不要把旧 ADX 误认为新分类器的输入。

## 分类与执行政策分开

`grid_scale_regime.mode` 支持三个选项：

- `Shadow`：仅增加审计观测，旧分类、订单、目标仓位和重置不变。
- `Classifier`：只替换有效分类，沿用现有分类对应的仓位、趋势与减仓政策。
- `Adaptive`：替换分类并启用柔性新增网格预算。

Adaptive 的基础系数：TWO_WAY 为 1；UNCERTAIN/TrendUp 为 `uncertain_budget`（默认 0.5）；
TrendDown、QUIET、WARMUP 为 0。再乘以：

```text
1 - unresolved_bars / observed_bars
```

这只缩减原 `target_position` 算出的 Grid 额度，不能放大额度，不作用于 Core。
因此原有上升趋势网格乘数与候选预算会相乘，不能将 0.5 理解为账户使用率。

柔性预算下降时：

- 已有库存和待买量都占额度；不能预支未成交卖单释放的容量。
- 超额买单优先撤减；CancelPending/Unknown 仍保持预留。
- 现有库存不因为软预算下降立即市价卖出，已有覆盖止盈继续保留。
- 基础持仓上限、高波动减仓、组合限制和显式风险政策不被豁免；Core 仍按独立目标管理。
- 下跌突破暂停新增库存的规则保留，Dynamic Reset 仍须通过撤单确认屏障。

`reset_on_regime_change=false` 独立关闭“仅因标签变化重置网格”。
边界突破、显著间距变化、重置间隔/距离/次数限制及撤单对账不变。Shadow 始终忽略此开关。

## 配置与使用

按标的在其现有 `grid` 对象中增加以下内容；不配置或设为 `null` 即保持旧方案。
旧 `regime_confirmation_bars=8` 字段继续保留作基线审计，不控制候选滞回。

```json
{
  "grid_scale_regime": {
    "mode": "Adaptive",
    "lookback_sessions": 10,
    "trend_enter_efficiency": "0.6",
    "trend_exit_efficiency": "0.4",
    "trend_drift_grids": "2",
    "minimum_rebounds": 2,
    "uncertain_budget": "0.5",
    "drawdown_budget_grids": null,
    "reset_on_regime_change": false
  }
}
```

需要 `StockAdaptive`、常规时段、一分钟 LAST 输入；Classifier/Adaptive 还要求开启趋势过滤。
不接受把候选配置悄悄用于 LegacyDgt；基准转换会主动清除候选配置。

可选 `drawdown_budget_grids` 用于研究回撤深度的连续折扣，默认 `null` 完全保留原预算。
它只在 Adaptive 模式影响新增 Grid 目标；Shadow/Classifier 可记录观测但不改变原来的仓位政策。
数值为正格数，允许范围为 1–100，表示从观测窗口最高收盘价回撤多少格时，将原预算减半：

```text
drawdown_grids = (highest_observed_close - current_close) / highest_observed_close / spacing
new_budget = old_budget × drawdown_budget_grids / (drawdown_budget_grids + drawdown_grids)
```

只使用已完成的 15 分钟收盘，不读取未来最高价、不把一次小反弹当作全部回撤已收复。
它不直接修改 Regime 判定或 Core 目标公式，不改变撤单屏障及硬风控，也不因折扣而强平已有 Grid 库存。
但成交、现金与网格重建反馈仍可能改变后续 Core 实际成交，不能把“公式不变”理解为“交易路径不变”。
因此不能阻止已经持有的仓位遭遇跳空损失；滚动高点移出窗口时，折扣可能减轻，不能误称为实际解套。
四季对照采用预先固定的 4 格，不根据某一季度挑参，配置见
[深度折扣研究配置](../../crates/backtest/examples/dynamic_grid_regime_depth.json)。

新增 [研究配置](../../crates/backtest/examples/dynamic_grid_regime.json) 使用现有 `PortfolioVariant`，
提供旧基线、Shadow、仅分类、柔性预算、仅重置解耦、完整候选六组。
后四组分别对照，不做整表参数暴力搜索；资金、费用、滑点和原有风险阈值不覆盖。

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest --features examples \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_comparison.json \
  crates/backtest/examples/dynamic_grid_regime.json \
  reports/dynamic-grid-regime-ablation.json
```

这是显式启动六次回放的命令，不是当前已完成的收益实验。首次只需在复制出的研究文件中保留 Shadow 或单个候选。
测试组合使用 AAPL、MSFT、NVDA、TSLA、AMZN、META 六只股票，沿用 comparison 的共享资金与风险预算。
本地已缓存六只股票的 Alpaca raw 一分钟数据；AMZN 的失效路径已修正为实际缓存位置，没有重复下载。
缓存元数据未明确记录 SIP/IEX feed，仅注明 API 默认值，不能据此宣称具备完整市场报价或盘口真实性。
选股器也复用候选分类，但没有存续网格，使用当时可计算间距；启用候选时需提供足够分钟历史，缺失则暖机失败。

## 恢复与审计

每个标的独立保存有界收盘历史、时段身份、前一方向标签和最新快照，随原检查点恢复。
恢复校验包括时间顺序、时段对齐、历史长度、正价格、间距、特征重算和最新分类桶一致性。
模式变更属于配置变更，不允许把旧运行检查点直接当作已暖机候选检查点复用。
旧配置没有新字段时，序列化不额外写空对象，维持既有检查点兼容性。

报告位置为各标的 `diagnostics.spacing[].grid_scale`，仅在完整分类桶收盘时写一条。
包含间距、漂移、效率、反弹次数、平均等待、未收复等待、预算和判定原因。
金额、成交去重、挂单预留、券商 reconciliation 仍由原有订单与账户链路管理。

## 验证边界

工程检查覆盖微小噪声、有效往返、单边下跌、实际间距、跳空不造路径、滞回、跨日与恢复一致性，
以及原生执行的 Shadow 全报告一致性、候选暖机后成交、硬风险和买卖预留。
实际运行结果如下；未运行的季度回测、OOS、收益提升不作成功声明。

### 2026-09-25 工程验证

- PASS：相关三 crate 的 `cargo check --lib --bins`，Debug 回测及 Longbridge 启动器构建。
- PASS：相关主套件 308 项、需要本地 HTTP 监听的 SDK reconciliation 测试 1 项，
  追加筛选套件 7 项（其中 6 项与主套件重复，1 项为新增六标的原生执行测试），合计 310 项不同测试通过。
- PASS：触及 Rust 文件格式检查、文档 lint、`git diff --check`。
- PARTIAL：相关 crate 的 Clippy 被既有 `momentum_pullback/model.rs` 与 `strategy.rs` 的 12 处错误阻断；
  未关闭 lint、未顺带改动另一策略，不能宣称 Clippy 全绿。
- NOT RUN：全仓构建、全仓测试、release 构建及真实券商验收。磁盘空间不足 1 GiB，未清理用户缓存。

集成测试位于现有 `crates/trading/tests/dynamic_grid.rs`；该文件命中仓库已有的 `tests/` 忽略规则。
本次没有修改该规则或操作暂存区，后续准备提交时需要显式检查集成测试是否被纳入。

相关编译与测试命令：

```bash
CARGO_INCREMENTAL=0 cargo check \
  -p nautilus-trading -p nautilus-longbridge -p nautilus-backtest \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid,nautilus-backtest/examples \
  --lib --bins -j2

CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --test dynamic_grid --cargo-profile dev --test-threads 1 \
  -E 'test(dynamic_grid) | binary(dynamic_grid) | package(nautilus-longbridge)' \
  --no-fail-fast
```

SDK 测试只使用本地 HTTP fixture，不连接真实账户；受限沙箱可能需要允许本地监听。

### 六标的真实缓存短回放

数据：2025-01-02 至 2025-01-31，六标的共 46,800 根 RTH 一分钟 Bar，真实 Quote 为零。
每组初始资金 100,000 USD、seed 42；commission=0、maker=0.08%、taker=0.10%、
配置 slippage=0.05%、撮合 slippage probability=1，均复用现有模拟执行假设。
这不是按券商账单校准的最低收费/完整盘口模型，实际 fill 成本见报告。

| 指标           | 15m＋8 基线 | Shadow    | Grid-scale Adaptive |
| -------------- | ----------: | --------: | ------------------: |
| 净收益 USD     | 812.93      | 812.93    | -700.08             |
| 收益率         | 0.813%      | 0.813%    | -0.700%             |
| 最大回撤       | 1.496%      | 1.496%    | 2.670%              |
| 平均资金利用率 | 18.933%     | 18.933%   | 16.714%             |
| 成交次数       | 109         | 109       | 298                 |
| 完成网格周期   | 39          | 39        | 102                 |
| 重置次数       | 11          | 11        | 10                  |
| 费用 USD       | 66.52       | 66.52     | 138.31              |
| 换手金额 USD   | 72,699.78   | 72,699.78 | 160,412.48          |

移除新增 `grid_scale` 诊断字段后，Shadow 与基线的完整报告严格相等，包括各标的权益路径、周期、费用及风险。
候选每标的记录 519 个分类观测，其中 286 个已暖机；首次可用为 1 月 16 日 09:45 ET。
前九个交易日不交易，因此这里不是等暴露、等有效交易天数的收益对照，不能把冷启动差异归因于识别能力。

本段历史中已暖机候选没有输出趋势：MSFT 全为 UNCERTAIN，
AAPL 为 238 次 UNCERTAIN、48 次 TWO_WAY，其余四只全为 TWO_WAY。
NVDA 的 Adaptive 净收益为 -1,578.76 USD（基线 -338.16 USD），触发 Maximum daily loss。
更多已完成周期不等于更好的库存风险识别；本次没有显示出候选整体增量价值，保留旧默认方案。
六只股票扩大了跨标的并发和价格尺度覆盖，但仍集中于科技/成长相关风险，不是全行业泛化检验。

原始报告（含实际生效配置、逐标的权益/诊断）保存在
`reports/dynamic-grid-grid-scale-six-stocks-2025-01-smoke-20260925.json.gz`。
这次使用 comparison 配置的副本，时间范围为 `[1735689600000000000, 1738368000000000000)`；
研究副本只保留 `baseline_15m_8`、`grid_scale_shadow`、`grid_scale_adaptive` 三组。
未执行另外三组单因素消融、Walk-forward、Monte Carlo 或参数扫描，也未据此调参。

尚未证明新方案优于旧方案。下一步需要冻结参数后做时间顺序验证，并增加等暴露控制组，
防止把“少持仓”误称为“识别更准确”。既有研究框架每个窗口从空仓开始，必须单独报告暖机损失及窗口边界效应；
反复查看过的 Q1/Q2/Q3 不能重新命名为未见 OOS。

已知限制：只观察 15 分钟收盘，可能漏掉桶内往返；没有引入交易所日历核验完整历史，无法识别全部盘尾缺数；
观察次数与完成交易不是同一个指标；柔性保留库存也保留其继续下跌的风险。
网格建立时的逐层数量仍被冻结，软预算恢复不会自动放大旧层数量，直到原有重建流程更新；
预算是风险上限而非保证用满资金的承诺。
拒绝交易的反事实收益、长期未完结批次的专门归因以及等暴露对照结果尚未生成。
