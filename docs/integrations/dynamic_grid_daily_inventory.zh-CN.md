# Dynamic Grid：日线库存状态主基线

> 历史研究归档：当前股票策略已固定为 15 分钟＋8 根确认。本文实验参数与复现命令不再作为运行入口；
> 已退役配置保存在本地 `reports/dynamic-grid-retired-regime-20260925.tar.gz`。当前用法见
> [最终方案](dynamic_grid_15m_final.zh-CN.md)。以下收益与结论仅记录当时版本。

2026-09-25；NautilusTrader 0.63.0，Rust，共享现有 MultiAssetGridStrategy。
本轮建立后续研究主线，不自动替换 Paper/live 参数，不操作券商账户或重启交易进程。

## 两个时钟，一套策略

| 层       | 数据                     | 职责                                                |
| -------- | ------------------------ | --------------------------------------------------- |
| 库存状态 | 已完成常规交易日线       | 上行／下行／中性，影响原有 Core/Grid 目标与组合分配 |
| 网格执行 | 原一分钟 Bar／Tick       | 原 ATR 间距、穿越、突破确认、限价单、流动性、估值   |
| 硬风险   | 原账户／订单／定时器事件 | 现金预留、回撤、日损、集中度、未知订单、撤单对账    |

周线暂不增加。日线比分钟更慢，不代表更准确，也不保证回撤更小。
本改造不修改网格层数、资金、手续费、滑点或风险上限。

## 分类规则

复用既有 ATR、ADX、移动平均实现及周期参数；日线检测器与分钟检测器各自保存历史。
`regime_average` 继续支持 Simple/Exponential，研究候选保持原有 Simple，不同时增加 EMA 实验。

```text
normalized_slope = (MA_now - MA_k_days_ago) / (k × daily_ATR)

ADX >= adx_trend_min
AND normalized_slope > slope_atr_threshold
AND close 位于 MA 上方（包含原 price_ma_confirmation_pct 缓冲）
    → TrendUp

同理反向 → TrendDown
其他已预热状态 → Range，原始原因标记 DAILY_NEUTRAL
未预热 → Disabled
```

这里的 Range 表示“没有确认方向”的中性库存预算，不承诺统计意义上的均值回归。
日线模式固定要求价格与均线方向一致，不沿用旧模式可关闭的价格方向确认开关。
日线不使用分钟 ATR%、分钟斜率比例、布林位置冲突来切换库存预算。
指标仍有完整暖机要求；日线 ATR14 与分钟 ATR14 不是相同时间跨度。

候选状态连续达到 `regime_confirmation_bars` 才替换确认状态。
日线候选设置为 2，意味着两根完整交易日线，而非两分钟；转换可能延迟至少两个收盘。
这不是优化得出的最优值，`slope_atr_threshold=0.05` 同样只是冻结研究候选。

分钟 HighVolatility/LowVolatility 继续阻止新 BUY，并撤销仍在挂单的 BUY，
但不会把日线 TrendUp/Range 强制改成分钟高波动库存目标。
原始 Grid SELL 不出售 Core；日线模式允许这些有库存覆盖的止盈单继续维护。
真实目标减仓、硬风险退出、订单预留和撤单终态屏障保持原路径。

## 日历、预热与恢复

- 日历复用已有 `IntradayMomentumSession`，行情聚合复用原生 `BarBuilder`。
  交易日实际 open/close 决定边界，不把 24 小时或 390 根硬套到半日市。
- 分钟时间戳为收盘时间：09:31 是当日第一根，实际 session close 才产生日线。
  未收盘日线、部分日线、缺失一分钟后的不完整日线不会用于分类。
- 周末／节假日不重置确认。上一交易日信号在下一根应到日线收盘前仍有效；
  如果应到收盘已缺失，则停止新增买入。分钟行情仍保留原 180 秒新鲜度限制。
- `prepare_daily_regime` 只在策略启动前注入日历和历史，不回放到订单引擎，不产生持仓。
  每个标的独立准备，预热数据必须严格早于实际回放起点。
- 恢复保存日线指标窗口、候选／确认状态、日历以及未完成日聚合桶。
  重叠日历不允许静默改变；过期的历史范围可以保留，新范围可扩展。
  重放预热或订阅缓存分钟不会重复更新日线。
- 新旧模式不可直接交换同一持仓检查点；原配置一致性检查与 broker reconciliation 没有放宽。

回测通过标的数据元数据 `calendar_path` 读取独立 date/open/close JSON，
再校验 CSV 内显式 `session_open/session_close`；预热价格只读起点前分钟。
本次 AAPL/MSFT 文件各 558 个交易日的分钟序列均完整，但独立 Alpaca 日历有 565 日：
CSV 缺少 2023-11-24、2024-07-03、2024-11-29、2024-12-24、2025-07-03、
2025-11-28、2025-12-24 共 7 个半日市。现已保留这些日历日期，缺失收盘不会当成休市。
本次 Q1/Q2 回放内没有这些缺失日；预热期仍按真实日历识别确认序列中断。
旧六列 OHLCV CSV 没有日历元数据，启用日线模式会明确拒绝。
单标的日线回测也用组合 runner（配置一个标的），不新增第二套回测接口。

## 配置与运行

所有原标的与内联示例明确增加 `daily_regime: null`，默认行为不变。
`calendar_path` 属于标的数据元数据（与 `bars_path` 同级，相对工作目录），不是交易阈值。
仅研究候选覆盖以下字段，其余按标的原配置：

```json
{
  "regime_bar_minutes": 1,
  "confirm_regime_changes": true,
  "regime_confirmation_bars": 2,
  "daily_regime": { "slope_atr_threshold": 0.05 }
}
```

冻结候选文件：
dynamic_grid_daily_research.json（已归档）。
同时切换了分类时钟、归一化方法、确认和库存/执行职责，属于整体方案对照，
不能将收益差异单独归因为“用了日线”。后续需要匹配规则的时间尺度消融。

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_portfolio.json \
  crates/backtest/examples/dynamic_grid_daily_research.json \
  reports/dynamic-grid-daily-q1-20260925.json
```

Q1/Q2 都已被此前研究查看，不称为未触碰 OOS。单季度空仓启动不能代替跨季度连续持仓验证。
既有 walk-forward runner 可复用：每日库存信号允许使用窗口起点前历史，交易账户仍在每个窗口空仓开始。
本轮不做参数扫描，不根据季度结果回头调整候选。

数据为本地 `wsl_test_data/{AAPL,MSFT}/bars.csv.gz`，Alpaca manifest 标记 `raw`，
feed 为未显式指定的 API 默认值。共享初始资金 100,000 美元，随机种子 42，
maker 0.08%、taker 0.10%、额外 commission 0，一 Tick 滑点概率 1；没有真实 Quote。
这些是原生 Bar 撮合研究，不等价于真实点差、排队与券商最低费用验收。

## Longbridge 状态：PARTIAL

实际 runner 已接入只读预热：复用 OAuth、限流/重试、NoAdjust 原始日线、分钟解析及美股交易日历。
已结束日线映射到真实常规时段收盘时间，盘中启动补齐当日已完成分钟。
未启用日线时不增加历史请求。预热超时在 node 启动前返回错误。

目前复用的日历只覆盖未来约 7 个自然日；覆盖耗尽会停买，需要在到期前受控重启刷新。
尚未实现不停机日历刷新，也未验收预热与实时订阅切换之间的补数；若有缺分钟，
当日不会被伪装成完整日线，随后按缺失日线停买。
日线研究模式不能据此视为可无人值守的长期实盘版本。

未进行真实账户成交验收；日线 provider 与分钟聚合日线的交易范围、收盘竞价差异，
复权／公司行动一致性仍需对账。最大预热限制为 1000 根，超过会报错而非截断。
现有选股评分器不支持可恢复日线状态，继续明确拒绝此实验配置。

## 验证与结果

### 固定参数回放

两只股票、同一初始资金和成本；Q1 为 2025-01-02 至 03-31，Q2 为 04-01 至 06-30。
H1 从 01-02 连续运行到 06-30，跨季度保留库存、现金、网格和风险状态，不拼接两份独立季度结果。

| 区间／方案    | 净损益 USD | 收益率  | MDD    | Sharpe | Sortino | 费用 USD | 平均资金利用率 |
| ------------- | ---------: | ------: | -----: | -----: | ------: | -------: | -------------: |
| Q1 原分钟基线 | -876.29    | -0.876% | 2.069% | -1.230 | -1.623  | 202.05   | 13.16%         |
| Q1 日线库存   | -1998.13   | -1.998% | 2.535% | -2.684 | -3.320  | 133.31   | 19.19%         |
| Q2 原分钟基线 | -869.41    | -0.869% | 3.944% | -0.612 | -0.795  | 404.82   | 12.28%         |
| Q2 日线库存   | 221.89     | 0.222%  | 3.165% | 0.231  | 0.303   | 175.36   | 18.70%         |
| H1 原分钟基线 | -642.09    | -0.642% | 3.647% | -0.313 | -0.442  | 553.79   | 12.50%         |
| H1 日线库存   | -1076.32   | -1.076% | 3.870% | -0.690 | -0.981  | 275.52   | 17.91%         |

H1 日线方案的成交名义金额为 307,467.68 美元（基线 599,562.74）；
完成 Grid 周期 53 个（基线 113），网格重置 9 次（基线 36），Grid churn 2249（基线 6876）。
相邻分钟诊断采样中的日内状态变化从 8342 次降至 12 次，但净损益和 MDD 均未改善。
H1 日线 Grid 已实现损益 -1163.88、Core 已实现损益 -22.46、期末未实现损益 +110.02 美元。

结论：PASS（时钟分离／抖动减少），PARTIAL（研究方向），未通过实盘候选晋级。
“更少切换、更低费用”得到本样本支持；“更高收益、更低风险”没有得到一致支持。
上一轮 confirmed_15m 的 Q2 盈利 3130.83 美元，本轮日线仅 221.89，
因此更长周期并非单调改善，不能将日线直接宣布为已验证的最优方案。
保留日线作为下一阶段工程研究主线，同时保留分钟／15 分钟对照和现有实盘配置。

完整原生报告（含逐笔周期、权益、CAGR、Calmar、风险和诊断）：
[Q1](../../reports/dynamic-grid-daily-q1-20260925.json)、
[Q2](../../reports/dynamic-grid-daily-q2-20260925.json)、
[连续 H1](../../reports/dynamic-grid-daily-h1-20260925.json)。
输入分别为 46,800、48,360、95,160 根 Bar，真实 Quote 均为 0。

```bash
target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_daily_h1.json \
  crates/backtest/examples/dynamic_grid_daily_research.json \
  reports/dynamic-grid-daily-h1-20260925.json
```

Q2 独立配置只将组合 `start_ns/end_ns` 改为 `1743465600000000000/1751328000000000000`；
本次解析后的完整输入记录在 Q2 报告内，未改变标的参数。

### 工程验证

- PASS：交易模块 716 项通过、4 项原有忽略；Longbridge 42 项通过。
  沙箱不允许本地模拟服务器绑定端口，提升权限重跑后通过，没有连接真实券商。
- PASS：新增验证涵盖日线收盘、半日市、DST、缺分钟、日历不可改写、跨周末有效性、
  日线预热前瞻拒绝、重启继续聚合、两个标的独立状态、分钟原始指标不变。
- PASS：数据首日无任何预热历史时，原生双标的冷启动 120 根 Bar，零订单、无隐藏 RiskOff，
  明确等待 `REGIME_WARMUP`；不会因没有可加载的历史而错误退出或偷用未来日线。
- PASS：debug 回测构建；两个 runner 的 `clippy --no-deps`；相关 Rust 格式与 `git diff --check`。
- PARTIAL：原生集成测试 75/78 通过，3 个原有配置测试失败：
  `dynamic_grid_comparison.json` 最大分配 20%，而引用 AAPL 配置分配 30%。
  本轮没有为通过测试而放宽该风险上限。
- PARTIAL：交易 crate 的 Clippy 被原有 `momentum_pullback` 的 12 项问题阻挡，不能称全仓绿色。
  未运行全 workspace 全特性测试或 release 构建。
- PASS：Q1、Q2 关闭候选后的完整报告与 2026-09-24 基线逐字段相等，
  包括权益、周期、费用、风险状态、诊断；两组的分钟 ATR、价格、原始分类快照也完全一致。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --test dynamic_grid --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest --bin longbridge-dynamic-grid --no-deps -j2
```

`crates/trading/tests/dynamic_grid.rs` 原本被仓库 `tests/` 忽略规则覆盖，本地测试确实执行；
本轮补齐它的配置构造及日历公平性断言，但没有擅自 stage 或修改全局忽略规则。
后续提交需由维护者明确纳入该已有集成测试文件。

### 已定位的后续问题

日线分类更平滑，不等于整个目标仓位已经变成日频。
`update_position_target` 仍按分钟价格／权益计算目标股数，继而执行取整与减仓。
Q2 日线候选的 AAPL 有 83 个 Core 已平仓周期，全部仅 1 股，持有中位数 3 分钟，
其中 49 个不超过 5 分钟；Core 已实现损益 -19.76 美元。
这与“Core 为较长期库存”的目标不一致，应该成为下一项独立工程实验：
稳定 Core 目标更新／取整边界，同时保留分钟级硬风险减仓和普通 Grid SELL。
本轮不把这项额外机制混入已冻结的日线对照，不增加补丁式指标或放松下跌禁买。

Q1 日线候选 `POSITION_TARGET_REDUCTION` 撤单记录为 976（基线 316），
Q2 为 1483（基线 537）；状态抖动减少并没有消除目标数量引起的撤挂。
这些是撤单诊断记录，不等于同样数量的成交或已实现损失。

longbridge-quant 的验证约束贯穿本轮：不把已看过的 Q1/Q2 称为 OOS，
不把信号变平滑当成预测准确率，不因某一季度盈利就切换实盘。
Walk-forward、敏感性和 Monte Carlo 仍可复用原框架，但本轮没有执行日线候选的这些检验。

### 代码导航

本工作区还有上一轮未提交的慢周期实验，本轮是在其上继续扩展，不覆盖或提交已有工作。

- `crates/trading/src/examples/strategies/dynamic_grid/daily_regime.rs`：新增日历与完整日线聚合。
- 同目录 `config.rs`、`regime.rs`、`regime_filter.rs`：可选日线配置、ATR 标准化斜率、确认与有效期。
- 同目录 `strategy.rs`、`multi_asset.rs`：库存状态与快执行门槛分离、启动预热、恢复验证。
- 同目录 `files.rs`：独立日历路径数据元数据；`multi_asset/tests.rs` 等覆盖时钟与恢复。
- `crates/backtest/src/dynamic_grid/{mod,portfolio,portfolio_research}.rs`：预热／日历、组合回放、研究数据一致性。
- `crates/backtest/bin/dynamic_grid.rs`：输出不可覆盖输入日历的保护。
- `crates/adapters/longbridge/bin/dynamic_grid.rs`：复用实际 SDK 的日线与当日分钟只读预热。
- `crates/backtest/examples/dynamic_grid_daily_{research,h1}.json`：冻结候选和连续窗口；
  旧示例只补显式关闭值，AAPL/MSFT 补数据日历路径。
- `crates/trading/tests/dynamic_grid.rs`：现有集成测试配置兼容、研究不得替换日历的断言。

没有删除原策略、修改 Cargo profile、添加依赖、提交 Git 或操作券商账户。
