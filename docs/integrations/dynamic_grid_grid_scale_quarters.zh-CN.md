# Grid-scale Adaptive 四季对照

## 本轮研究问题

上一轮 2025 年 1 月回放中，NVDA 大跌后仍被候选识别为 TWO_WAY。
1 月 27 日 09:45 ET 的预算为 0.8979；收盘时小反弹令 `unresolved_bars=0`，预算恢复为 1。
这表示“一格反弹机会成立”，不表示更高价库存已恢复。原模型只按等待时间缩减新增仓位，未考虑回撤深度。

本轮只检验一个假设：增加连续的回撤深度折扣，能否改善库存风险与费用后的收益。
不增加强平、停买阈值，不更改 Regime 分类算法，不放宽硬风控，不调每只股票的资金、Core 或网格参数。
遵循 longbridge-quant 的验证约束：工程测试通过不等于研究假设成立，也不能把已看过的季度称为未见 OOS。

## 冻结设计

三个版本在 2025 年 Q1、Q2、Q3、Q4 分别独立回放：

- `baseline_15m_8`：旧 15 分钟＋8 根确认。
- `grid_scale_adaptive`：上一轮 Grid-scale Adaptive，10 个观测时段，标签与重置解耦。
- `grid_scale_depth_4`：仅将 `drawdown_budget_grids` 从 `null` 改为 `"4"`。

每季 100,000 USD、空仓及空指标状态开始；六标的固定为 AAPL、MSFT、NVDA、TSLA、AMZN、META。
两个 Grid-scale 版本具有相同暖机要求；旧 15m 方案的暖机不同，因此不是等有效交易时长对照。
两个 Grid-scale 版本之间才是回撤折扣的单因素对照；与 15m 基线的比较还包含分类、软预算及重置政策差异。
不逐季选择参数，不把四个独立季度收益拼成年收益。本轮不修改 Paper/Live 默认配置。

使用已有 Alpaca raw 一分钟派生缓存，配置要求常规时段交易，但本轮审计发现下半年输入不符合完整交易所日历，详见下文。
缓存元数据未明确 SIP/IEX feed。
没有真实 Quote，仍是 Nautilus OHLC 撮合，不能验证真实点差、排队成交或市场冲击。
资金、费用、滑点、seed 42、组合预算均沿用 `dynamic_grid_comparison.json`，全部生效配置随报告保存。
maker=0.08%、taker=0.10%、commission=0、slippage=0.05%、slippage probability=1。

## 代码与复现

核心修改只在现有 `grid_scale.rs`：可选参数、可恢复的深度观测和连续预算折扣，价格及预算保持 Decimal。
深度是相对窗口最高**已完成收盘价**的跌幅除以当前网格间距，不使用未来 high。

```text
new_budget = old_budget × 4 / (4 + drawdown_grids)
```

0 格不打折，4 格减半，12 格剩四分之一；这是新增 Grid 额度，不是卖出现有持仓的指令。
原有 Core 目标公式、行情新鲜度、硬风险、部分成交、待撤/未知订单预留均不变。
预算改变成交与现金路径，后续资金分配、重建及 Core 实际成交仍可能变化，因此需要记录执行层反馈。
滚动高点移出窗口也可能减轻折扣；这不等同于解套，是当前有限记忆模型的已知限制。

[研究配置](../../crates/backtest/examples/dynamic_grid_regime_depth.json) 已在观察四季结果前固定。
本次 `--ablation` 不执行配置中预留给既有研究入口的 train/validation、敏感性或 Monte Carlo 设置；
四季诊断不等同于 Walk-forward，也不证明参数邻域稳定。
复制组合配置，仅调整 `start_ns/end_ns`，保持标的配置引用指向原文件，然后运行：

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest --bin longbridge-dynamic-grid -j2

target/debug/dynamic-grid-backtest --ablation QUARTER_CONFIG.json \
  crates/backtest/examples/dynamic_grid_regime_depth.json REPORT.json
```

日期范围为左闭右开，按 UTC：Q1 01-01/04-01，Q2 04-01/07-01，Q3 07-01/10-01，Q4 10-01/2026-01-01。
本机通过 FIFO 把报告直接交给 gzip 压缩，逐个季度运行，未删行情或账户检查点。

## 数据日历审计：Q3/Q4 仅用于诊断

对六份实际输入逐行检查，并对照缓存的 Alpaca 交易所日历：

- Q1：每只股票 60 个交易日、23,400 根 Bar；日期及常规时段边界检查通过。
- Q2：每只股票 62 个交易日、24,180 根 Bar；同上。
- Q3：应有 64 个交易日。AAPL/MSFT/AMZN/META 缺少 7 月 3 日；META 另缺少 9 月 12 日。
  NVDA/TSLA 保留了 7 月 3 日，但每只混入了收盘后 180 根一分钟 Bar，且当天 `session_close` 写成 16:00 ET。
- Q4：应有 64 个交易日。除 NVDA 外全部缺少 11 月 28 日；全部缺少 12 月 24 日。
  NVDA 的 11 月 28 日同样混入收盘后 180 根 Bar，且时段结束时间不正确。

2025 年 7 月 3 日、11 月 28 日及 12 月 24 日应于 13:00 ET 提前收市，
亦可核对 [NYSE 官方日历](https://www.nyse.com/publicdocs/ICE_NYSE_2025_Yearly_Trading_Calendar.pdf)。
本次审计未检查每根价格与供应商原始记录的一致性；Q1/Q2 日历检查通过不等于行情质量全面认证。

现有 `load_bars` 接受扩展 CSV，但不使用其中 `session_open/session_close` 校验交易所时段，
策略常规时段判断也不能补回被上游预处理删除的日期。
这是输入及回测日历口径的真实缺陷，不能解释成“股票自然没有成交”。
三种模型使用同一份输入，仍不足以消除该偏差：缺日会改变暖机、风险检查和估值路径，盘后数据会影响成交与 Regime。

因此保留原始结果作为诊断，不裁剪数据后冒充同一试验，也不据此晋升候选。
下一步应重建独立的 Grid 研究缓存：按真实交易日历保留半日市，剔除盘后数据，补齐缺失原始行情后重跑。
当前仅 AAPL 的另一份原始分页缓存可见，其余对应目录仅保留元数据；本机剩余空间不足以安全下载和重建，未执行此步骤。
完整审计含每份 CSV 的 SHA-256，位于 `reports/dynamic-grid-depth-2025-data-audit-20260925.json`。

## 实际结果：2025 年四个独立季度

全部 12 次原生回放完成。表中收益为含期末库存估值、扣除费用后的账户收益；费用单位 USD。
`15m`、`Adaptive`、`Depth4` 分别对应上述三个冻结候选。Q3/Q4 带星号，表示数据日历校验失败。

| 季度 | 版本 | 收益 | 最大回撤 | Sharpe | Sortino | 平均资金利用率 | 费用 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Q1 | 15m | -4.10% | 6.54% | -1.78 | -2.23 | 25.99% | 137.58 |
| Q1 | Adaptive | -6.51% | 8.65% | -2.30 | -2.78 | 29.96% | 279.06 |
| Q1 | Depth4 | -4.61% | 6.29% | -2.40 | -2.89 | 21.76% | 179.38 |
| Q2 | 15m | 9.32% | 3.77% | 3.30 | 6.96 | 27.24% | 279.36 |
| Q2 | Adaptive | 8.24% | 4.70% | 3.29 | 5.86 | 28.90% | 450.71 |
| Q2 | Depth4 | 7.12% | 3.48% | 3.45 | 6.01 | 26.52% | 443.03 |
| Q3* | 15m | 3.88% | 1.47% | 3.59 | 5.73 | 24.53% | 144.06 |
| Q3* | Adaptive | 3.90% | 1.52% | 3.38 | 5.15 | 25.48% | 409.45 |
| Q3* | Depth4 | 3.23% | 1.42% | 3.10 | 4.64 | 22.06% | 296.73 |
| Q4* | 15m | 1.26% | 2.52% | 1.04 | 1.48 | 23.61% | 133.75 |
| Q4* | Adaptive | 1.05% | 4.43% | 0.66 | 0.93 | 28.17% | 399.27 |
| Q4* | Depth4 | 0.58% | 3.83% | 0.43 | 0.59 | 23.36% | 277.06 |

结论限定在此次输入和执行假设内，不进行统计显著性或跨市场推广：

1. 原 Adaptive 没有显示出替代 15m 的优势：Q1/Q2/Q4 收益更低、回撤更大；
   Q3 仅多赚 13.59 USD，而费用从 144.06 升至 409.45 USD，Sharpe 更低。
2. 深度折扣相对原 Adaptive 四季都降低了资金利用率和回撤，但只有 Q1 减少亏损，
   Q2/Q3/Q4 收益均下降；相对 15m，四季收益均更低。它目前更像降仓工具，尚未显示收益增强价值。
3. Q1 原 Adaptive 的 Grid 已实现利润为 948.51 USD，但期末库存浮亏达 7,741.84 USD，
   不能用网格闭环盈利证明组合有效。Depth4 将库存浮亏降至 4,208.82 USD，
   但同时产生 Core 已实现亏损 681.37 USD，最终净亏损 4,613.77 USD。
4. Q1 Depth4 的 MDD 较小，但 Sharpe 比原 Adaptive 更低，进一步说明不同风险指标并不同时改善。

候选不会预测或消除已持有库存的隔夜跳空。Q1 两个 Adaptive 版本的 NVDA 都在
1 月 27 日 09:31 ET 触发既有日损限制，早于当天第一个 09:45 ET 候选观测；
缩减预算的作用主要是改变事前持仓和之后的新增库存，不是绕过硬风险恢复买入。

保留 15m＋8 根作为运行基线，Depth4 保留为默认关闭的研究开关。
不因单季结果修改实盘标的参数、资金预算或风险阈值。

### 可追溯报告

- 汇总：`reports/dynamic-grid-depth-2025-quarter-summary-20260925.json`。
  包含 12 组组合/标的指标、实际配置、缓存审计及全部压缩报告的 SHA-256。
- Q1 完整报告：`reports/dynamic-grid-depth-2025-q1-20260925.json.gz`，含三候选。
- Q2–Q4：`reports/dynamic-grid-depth-2025-q{2,3,4}-{candidate}-20260925.json.gz`，每候选独立报告。
  所有报告保留原生权益路径和审计信息，未只保留最终收益。
- 汇总逐组核对原生 CLI 输出，并用 Decimal 校验六标的净 PnL 之和等于组合净 PnL；12 组均通过。

## 验证状态

针对性测试先验证新增配置缺失时失败，再实现并通过 10 项针对性检查。
相关原生回归 274 项全部通过，包含预算界限、重启恢复、Shadow 全路径一致性、六标的共享现金及硬风险。
这些是本轮真实执行结果，不是全仓测试声明；集成测试文件命中已有 `tests/` 忽略规则，提交前需显式纳入。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib --profile dev -j2 grid_scale

CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading --features examples --lib --test dynamic_grid \
  --cargo-profile dev --test-threads 1 \
  -E 'test(dynamic_grid) | binary(dynamic_grid)' --no-fail-fast
```

本轮两个 debug 入口构建通过，构建及测试日志未发现编译 warning。
相关 Rust 文件格式检查及 `git diff --check` 通过；stable rustfmt 提示不能启用仓库的 nightly 导入分组设置。
两份中文文档通过已有 markdownlint-cli2 检查；通过本机缓存的 CLI 执行，没有安装新依赖。
磁盘可用空间降至百 MiB 以下，因此未追加全仓 check/clippy/test 或 release 构建，不引用上一轮检查代替本轮结果。
没有运行 Walk-forward、Monte Carlo、参数扫描、真实 Quote 压力测试或券商成交验收。

## 如何解读与下一步

降低预算天然可能减少风险，也可能错过反弹；回撤下降不等于 Regime 识别更准确。
没有用“网格闭环高胜率”掩盖期末库存浮亏，也不把已实现 Grid 利润当作账户净收益。

观察到的执行反馈必须独立检查：Q2 深度版本的 Core 再平衡次数从原 Adaptive 的 50 增至 242，
总成交事件从 1,378 增至 1,660，重置次数从 42 增至 69。虽然该版本只直接缩放 Grid 目标，
现金释放、资金重分配和重建可能影响后续 Core 成交，不能声称整个 Core 路径不受影响。
这些计数不能替代逐事件因果归因；本轮没有再加防抖参数来掩盖反馈。

下一步按以下顺序，而不是继续叠加指标或扫最优参数：

1. 修复独立 Grid 数据缓存及交易所日历口径，补足 Q3/Q4 原始数据后再对照。
2. 对齐预热历史、共同计分起点，再补全年连续持仓回放；季度空仓试验不能代表跨季库存策略。
3. 审计软预算 → 资金分配 → Core 再平衡/重置的事件链，区分确有必要的调仓和重复操作。
4. 冻结候选后再做未参与挑参的时间段、真实 Quote/费用压力及参数邻域验证。

`drawdown_budget_grids="4"` 保留为可复现研究候选，不是最优参数或实盘推荐。
