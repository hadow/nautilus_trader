# Dynamic Grid：15 分钟主研究基线

> 历史研究归档：当前股票策略已固定为 15 分钟＋8 根确认。本文实验参数与复现命令不再作为运行入口；
> 已退役配置保存在本地 `reports/dynamic-grid-retired-regime-20260925.tar.gz`。当前用法见
> [最终方案](dynamic_grid_15m_final.zh-CN.md)。以下收益与结论仅记录当时版本。

研究日期：2026-09-25。本轮将后续研究聚焦于 **15 分钟分类＋8 根连续确认**，
分钟数据继续负责执行与硬风控。日线方案保留为历史实验，不再作为主研究方向。
这不是已验证的实盘参数升级；现有标的 JSON、Paper/live 配置和券商账户均未改变。

## 最小改动

复用 `RegimeFilter`、NautilusTrader `BarBuilder` 和现有原生回测入口，
只调整已有参数，没有改分类算法、增加指标、增加硬控开关或扩大风险预算。
按照 longbridge-quant 的验证约束，先冻结单因素候选，再检验后续时期，不能用收益挑选代替有效性证明。

主研究预设（已归档）：

```json
{
  "daily_regime": null,
  "regime_bar_minutes": 15,
  "confirm_regime_changes": true,
  "regime_confirmation_bars": 8
}
```

文件是研究覆盖配置，不是券商 runner 的完整组合配置。
确认根数对照（已归档）
保留 3、5、8 根三组，原来的 5 根是本轮对照组。

- 15 分钟 Bar 必须完整收盘才能更新分类；中途启动、丢分钟或重复事件不会补造确认。
- 8 根对应连续约 120 分钟，5 根约 75 分钟，3 根约 45 分钟；首次指标预热另计。
  原有隔夜重新确认规则保留，延长确认会增加开盘等待，也会延后下降趋势减仓。
- 硬回撤、现金预留、订单超时等仍走原执行路径，分钟高波动立即生效，不等待 8 根确认。
  有库存覆盖的风险退出不因确认变慢而被禁用。
- 补充中文参数注释和 3/5/8 根回归用例；不另建状态机或执行框架。

## 同条件实验

标的 AAPL、MSFT；初始共享资金 $100,000；随机种子 42。
使用仓库既有 Alpaca 原始一分钟 CSV，而非本轮新获取行情。
maker 0.08%、taker 0.10%、附加佣金 0；一个 tick 滑点概率为 1。
保持原 3%–6% 网格间距、资金分配和风险限制。
各组有效配置逐字段比较，唯一差异为 `regime_confirmation_bars`。

H1：2025-01-02 至 2025-06-30，95,160 根 Bar，跨季度保留库存。
先看完 H1，再冻结 8 根候选验证 Q3；没有根据 Q3 重新挑参数。
Q3：2025-07-01 至 2025-09-30，49,140 根 Bar，各组重新空仓启动。
Q3 数据缺少 7 月 3 日半日市，沿用现有缺桶/过期处理，不伪造补齐。
两个回放不能相加冒充连续九个月持仓收益；本轮不是完整 walk-forward，也不声称严格未触碰 OOS。

### 连续 H1

Net、Fees 为美元；MDD、Util 为百分比；Switches 为日内状态切换数。

| Bars | Net     | MDD  | Sharpe | Fees   | Util  | Switches | Churn |
| ---- | ------- | ---- | ------ | ------ | ----- | -------- | ----- |
| 3    | 1611.73 | 5.16 | 0.510  | 138.85 | 20.21 | 198      | 2936  |
| 5    | 752.54  | 6.19 | 0.236  | 128.97 | 21.43 | 138      | 2269  |
| 8    | 3423.59 | 3.07 | 1.284  | 147.02 | 14.79 | 85       | 1151  |

8 根相较 5 根：净收益增加 $2,671.05，日内切换减少 38.4%，Churn 减少 49.3%。
但费用增加 $18.05，并非仅靠少交易省手续费获利。
Grid 已实现盈亏从 $2,769.18 增至 $3,658.27，期末浮亏从 $2,067.41 降至 $283.34。
Q1 净亏损从 $1,822.79 降至 $252.78；带着 Q1 库存进入 Q2 的净增益从 $2,575.33 增至 $3,676.37。
平均暴露也明显下降，改善不能全部归因于分类准确性或网格套利能力提升。

### 冻结后的 Q3 验证

| Bars | Net     | MDD   | Sharpe | Fees  | Util  | Switches | Churn |
| ---- | ------- | ----- | ------ | ----- | ----- | -------- | ----- |
| 3    | 1730.99 | 0.214 | 5.385  | 56.73 | 11.92 | 99       | 505   |
| 5    | 1385.79 | 0.372 | 4.344  | 51.14 | 11.46 | 70       | 324   |
| 8    | 1449.29 | 0.414 | 3.793  | 55.65 | 12.15 | 48       | 277   |

8 根仍比 5 根少切换 31.4%、多盈利 $63.50，但 MDD 和 Sharpe 更差；3 根在这个季度收益更高。
因此保留 8 根作为降低切换的研究候选，不宣称它是普遍最优、邻域稳定或样本外胜出的参数。

切换统计逐标的汇总相邻诊断点，间隔不超过 90 秒才计入日内；
另外计得 H1 隔夜切换 77/67/58 次，Q3 为 41/40/40 次（依次为 3/5/8 根）。
Churn 沿用报告原定义，不能与分类切换或实际成交数混用。

## 验证结果与边界

- PASS：三组分钟价格、ATR、原始分钟快照及原始 15 分钟分类输入逐点一致。
  差异仅来自确认及其后续真实模拟订单路径，没有用未来数据更改指标。
- PASS：Dynamic Grid 模块 162 项测试通过，4 项既有忽略；包含新增 3 个确认计数/恢复用例。
- PASS：原生撮合的慢 regime、分钟执行及硬回撤优先级集成场景通过。
  该既有集成文件受 `tests/` 忽略规则影响；本轮新增用例位于 `regime_filter.rs`，没有依赖被忽略文件交付。
- PASS：debug 二进制构建及对应 `cargo check` 成功，修改文件的格式/Markdown 校验通过；
  没有重试 release 或绕过系统安全限制。
- PARTIAL：交易库 Clippy 被 `momentum_pullback` 的 12 个既有错误阻断；未降低 lint 标准或修改无关策略。
  未重跑全 workspace，也没有本轮券商成交验收。
- 限制：两个区间的 15 分钟源仍主要输出 Range/Disabled，没有 TrendUp/TrendDown。
  原斜率阈值按分钟口径换算后为 `0.001 × 15 = 0.015`，不能把收益改善称为趋势识别更准确。
- 限制：仅两个股票、少量成交，没有真实 Quote、最低收费或排队/延迟压力验证。
  所有模拟收益包含模型化的成交价改善，不保证真实券商可以获得相同价格。

后续已完成[15 分钟斜率校准单因素实验](dynamic_grid_15m_slope.zh-CN.md)：
较低门槛增加了趋势标签，但收益与切换未共同改善，因此保留本页主基线。
不要同时调整 Core、网格间距和资金预算；目标仓位逐分钟取整导致的小额反复调仓另立对照。
日线/周线、多指标投票、新硬阈值、额外参数扫描，本轮均未增加。

## 复现

从仓库根目录执行，主研究预设只回放一组 Dynamic Grid：

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_daily_h1.json \
  crates/backtest/examples/dynamic_grid_15m_baseline.json \
  reports/dynamic-grid-15m-baseline-h1.json
```

`dynamic_grid_daily_h1.json` 只复用相同的半年数据窗口；策略由覆盖配置显式设为 15 分钟，未启用日线。
将研究文件改为 `dynamic_grid_15m_confirmation.json` 即复现三组对照；
将数据窗口配置改为 `dynamic_grid_regime_q3.json` 即复现 Q3。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --profile dev -j2 examples::strategies::dynamic_grid -- --test-threads=1

CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --test dynamic_grid --profile dev -j2 \
  slow_regime_native_replay_keeps_minute_execution_and_hard_drawdown -- --test-threads=1

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading --features examples --lib -j2
```

完整配置、权益路径、周期账本和诊断报告已无损压缩，`gzip -t` 校验通过：

- `reports/dynamic-grid-15m-confirmation-h1-20260925.json.gz`。
- `reports/dynamic-grid-15m-confirmation-q3-20260925.json.gz`。

压缩仅替换本轮生成的大体积 JSON，可解压恢复；没有删除行情、源码或检查点。
