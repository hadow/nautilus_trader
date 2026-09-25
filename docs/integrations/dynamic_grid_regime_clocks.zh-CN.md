# Dynamic Grid：执行时钟与状态时钟分离

> 历史研究归档：当前股票策略已固定为 15 分钟＋8 根确认。本文实验参数与复现命令不再作为运行入口；
> 已退役配置保存在本地 `reports/dynamic-grid-retired-regime-20260925.tar.gz`。当前用法见
> [最终方案](dynamic_grid_15m_final.zh-CN.md)。以下收益与结论仅记录当时版本。

研究日期：2026-09-24。沿用 NautilusTrader 0.63.0 和既有 Rust Dynamic Grid。
本轮是可独立关闭的研究改造，不是已验证盈利的实盘参数升级。
标的及 sandbox 等示例 JSON 只显式补齐两个旧行为默认值；
Paper/live 没有启用候选或调整风险参数，券商账户和运行中进程不改动。

2026-09-25 更新：后续主研究方向已转为 15 分钟，见
[确认根数对照与新研究基线](dynamic_grid_15m_confirmation.zh-CN.md)。下文保留当时实验结论。

## 改了什么

旧逻辑每分钟计算原始 regime，立即影响订单政策、目标仓位及组合分配；
`regime_confirmation_bars` 只限制重新买入，未阻止一根短暂分类变化引发撤单或减仓。

新增两个参数，默认保持旧行为：

```json
{
  "regime_bar_minutes": 1,
  "confirm_regime_changes": false
}
```

- `regime_bar_minutes`：1 保持原分类输入；5、15、30 从已完成的一分钟 LAST Bar 聚合。
  聚合以纽约 09:30 为起点，区间右端闭合；例如第一根 15 分钟分类 Bar 到 09:45 才可使用。
- `confirm_regime_changes`：启用后，普通状态变化必须连续达到原有
  `regime_confirmation_bars` 才替换有效状态。撤单、目标仓位、覆盖卖单、重置归因和组合分配
  全部读取同一个有效状态，不再只确认新买单。

两项实验均限定 StockAdaptive 与一分钟 LAST 输入；慢聚合还要求常规时段模式。
没有新增分类指标、HMM、日线过滤层、入场/出场各自一套阈值或第二套交易框架。

代价是确认延迟：新的下跌方向尚未确认时，旧 Range 可能继续允许买入。
这必须通过库存损失及回撤验证，不能把少撤单直接等同于更安全。

## 哪些仍然按分钟或 Tick 工作

网格宽度/间距 ATR、突破确认、流动性、跳空、估值及资金预留均不换周期。
订单超时、未知状态、回撤、亏损、现金及组合集中度限制仍按原执行路径优先处理。
分钟高/低波动门槛立即生效，不等待慢 Bar 收盘或软确认。

慢层只提供方向分类；不能把 15 分钟 ATR 或收益波动率与原分钟阈值直接比较。
慢检测器内部关闭重复的波动率分类，分钟检测器仍保留原波动率规则。
慢层 ADX/MA/布林位置窗口以慢 Bar 计数；斜率阈值保持每分钟比例口径，
内部按分钟数换算阈值，避免切换时钟同时悄悄放宽趋势门槛。

连续确认也按分类 Bar 计数：现有 5 根，在一分钟下是约 5 分钟，在 15 分钟下是约 75 分钟。
窗口的实际时间跨度和反应延迟都会变大，不可把差异全部归因为“去掉了噪声”。
本轮尚未加入相同实际时间窗口的匹配对照，也不能证明分类准确率上升。

## 安全与恢复

- 使用仓库既有 `BarBuilder` 聚合价格、数量；没有手写浮点 OHLC 聚合器。
- 最多持久化一个未完成桶，并保存慢指标历史、候选状态、确认状态和计数。
  恢复验证标的、Bar 类型、时间顺序、桶边界和指标重放一致性。
- 中途启动、丢分钟或跨夜不拼接虚假的完整 Bar；等下一完整桶。
  不使用未来收盘、高低价，也不把 Tick 回调次数当成确认根数。
- 分钟信号仍遵守原 `max_signal_age_secs`；慢信号只在自身周期加同一容差内有效。
  不能简单把全局过期阈值从 180 秒改成 15 分钟，从而放过真正失联的快行情。
- 隔夜或缺失整桶后，连续确认重新计数。慢信号预热或过期不禁止有库存覆盖的风险退出；
  普通卖单仍受明确配置的趋势政策约束。
- 慢周期候选在次日开盘不能把昨日下午的分类当作新鲜输入：15 分钟模式需等待
  当日首根完整慢 Bar，再按配置完成连续确认。这会增加开盘停买时间；
  即使指标已有历史，5 根慢 Bar 的确认仍可能等待约 75 分钟，并非没有代价的去噪。
- 旧检查点缺少新字段时按关闭状态恢复；不能直接把旧检查点换成启用新实验的配置。
  改参数仍需遵循既有配置一致性与券商对账屏障，不自动迁移交易账户状态。

常规时段沿用现有纽约时区函数，不新增完整交易所节假日/提前休市日历。
候选选股评分器只有有限分钟历史，尚未恢复慢层确认状态；新实验配置会显式返回
`REGIME_FILTER_REQUIRES_STRATEGY_REPLAY`，而非悄悄按旧分类假装一致。

## 可审计观测

原 `diagnostics.spacing[].regime` 仍是分钟指标原始快照。
实验启用后增加 `decision_regime`，表示实际决策状态；
`regime_source` 仅在分类 Bar 刚收盘时记录原始分类输入。
慢指标预热阻止入场时记录 `REGIME_WARMUP`，不把它混成数据过期或危险行情。
组合和标的权益归因使用有效状态；改变状态标签会改变归因，不能据此单独证明因果收益。

## 对照方法

固定研究配置（已归档） 包含四组：
baseline、confirmed_1m、raw_15m、confirmed_15m。
先做两个单因素对照，再用组合组观察交互，不扫描一整张参数笛卡尔积。

使用 AAPL/MSFT 原始一分钟历史、共享 100,000 美元、随机种子 42。
maker 0.08%、taker 0.10%、附加佣金 0，原生一 Tick 滑点概率 1。
保留原网格 3%–6% 间距、风险预算及全部未改字段。
无真实 Quote，不能声称验证真实点差、排队成交、最低收费或券商实盘效果。
Q1 与 Q2 各自空仓重新启动；这些区间已经研究过，不称为未触碰 OOS。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --test dynamic_grid --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo check -p nautilus-trading -p nautilus-longbridge \
  -p nautilus-backtest \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid,nautilus-backtest/examples -j2

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid --lib --tests -j2

CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_portfolio.json \
  crates/backtest/examples/dynamic_grid_regime_clocks.json \
  reports/dynamic-grid-regime-clocks-q1-rerun.json
```

Q2 将起止时间改为 `1743465600000000000`、`1751328000000000000`，
保留其他配置；结果保存完整有效配置，可用于独立复现。

## 工程验证

- PASS：交易库 706 项、Longbridge 库 42 项通过，4 项原有忽略测试。
  本地模拟 HTTP 服务需要沙箱外绑定 localhost；未使用真实券商。
- PARTIAL：原生集成套件 75 通过、3 失败。新增的慢时钟撮合/硬回撤场景、
  Legacy/Fixed 转换场景均通过；已有 Paper 配置一致性、恢复和订单测试也通过。
- 3 个失败均读取既有 `dynamic_grid_comparison.json`：其单标的资金上限为 20%，
  但引用的 AAPL 分配为 30%。已核对 HEAD，冲突在本轮之前存在；
  没有通过修改风险预算或跳过测试掩盖问题。
- PASS：三个相关 crate 的 `cargo check` 及 debug 回测二进制构建成功；
  本轮不尝试 release 或绕过系统安全限制。
- PARTIAL：Clippy 被 13 个既有错误阻断，位置为 `dynamic_grid/files.rs` 和
  `momentum_pullback`。本轮产生的 4 个告警已修复；没有降低 lint 等级。
- PASS：改动 Rust 文件的 rustfmt 检查、本文 Markdown 校验及 `git diff --check`。
  未运行全 workspace 的测试或 pre-flight，不能宣称全仓验证通过。
- PASS：11 个已有 JSON 文件逐字段核对，除显式补齐 `1`、`false` 两项默认值外，
  与 HEAD 完全一致。旧配置/检查点缺字段仍可加载。

独立构建 backtest crate 的库测试曾拉入大型 DataFusion 测试依赖，已停止；
相同断言改由既有 `crates/trading/tests/dynamic_grid.rs` 原生集成入口执行，未删除验证目标。
该现有文件被仓库 `tests/` 忽略规则覆盖，不会自动出现在 `git diff`；
新增用例保存在本地该文件中，将来准备提交时需显式纳入。本轮未提交或暂存文件。

## 修改范围

- `dynamic_grid/regime_filter.rs`：慢聚合、确认、新鲜度、恢复校验及单元测试。
- `dynamic_grid/strategy.rs`、`multi_asset.rs`：所有决策统一读取有效状态，快指标/硬风险不变。
- `dynamic_grid/regime.rs`：复用同一份状态对应订单政策；保留前轮新增分类归因。
- `dynamic_grid/stock.rs`：复用纽约时区，提供完整分钟的常规时段起点。
- `dynamic_grid/config.rs`、`diagnostics.rs`、`mod.rs`：配置、可选诊断和私有模块接线。
- `dynamic_grid/selection.rs`：尚不支持慢状态的评分路径显式拒绝，不静默失配。
- `dynamic_grid/multi_asset/tests.rs`、`crates/trading/tests/dynamic_grid.rs`：恢复、双标的、原生撮合验证。
- `crates/backtest/src/dynamic_grid/{mod,portfolio}.rs`：保留旧基准的原行为。
- 既有 11 个示例/标的 JSON：仅补关闭状态默认值；新增研究 JSON 和本文档。

## 季度对照结果

两季度、每季四组均完成真实 Rust 原生回放。Q1 为 2025-01-02 至 2025-03-31，
共 46,800 根输入 Bar；Q2 为 2025-04-01 至 2025-06-30，共 48,360 根。
两季分别从相同初始资金开始，不能把两季直接拼成一条连续持仓收益曲线。

下表 Net 和 Fees 单位为美元，Return/MDD/Util 单位为百分比，Util 是平均资金利用率。
`confirmed` 指对所有软状态决策使用连续确认，不是新增硬风险阈值。

### Q1

| Variant       | Net      | Return | MDD  | Sharpe | Sortino | Fees   | Fills | Cycles | Util  |
| ------------- | -------- | ------ | ---- | ------ | ------- | ------ | ----- | ------ | ----- |
| baseline      | -876.29  | -0.88  | 2.07 | -1.23  | -1.62   | 202.05 | 81    | 36     | 13.16 |
| confirmed_1m  | -1001.16 | -1.00  | 1.93 | -1.39  | -1.85   | 212.34 | 86    | 38     | 15.22 |
| raw_15m       | -1594.11 | -1.59  | 2.64 | -1.59  | -1.99   | 53.95  | 25    | 7      | 17.58 |
| confirmed_15m | -1822.79 | -1.82  | 3.14 | -1.60  | -1.96   | 59.51  | 28    | 8      | 19.33 |

### Q2

| Variant       | Net     | Return | MDD  | Sharpe | Sortino | Fees   | Fills | Cycles | Util  |
| ------------- | ------- | ------ | ---- | ------ | ------- | ------ | ----- | ------ | ----- |
| baseline      | -869.41 | -0.87  | 3.94 | -0.61  | -0.80   | 404.82 | 264   | 84     | 12.28 |
| confirmed_1m  | 559.93  | 0.56   | 3.77 | 0.39   | 0.62    | 403.98 | 288   | 90     | 15.09 |
| raw_15m       | 3096.94 | 3.10   | 0.90 | 5.20   | 11.92   | 121.77 | 65    | 19     | 8.11  |
| confirmed_15m | 3130.83 | 3.13   | 0.90 | 5.10   | 11.62   | 123.64 | 73    | 20     | 9.72  |

### 状态抖动与库存代价

下面切换数按每个标的相邻、不超过 90 秒间隔的诊断点计算，再汇总；不计跨夜跳变。
Churn 沿用已有报告定义，不把减少分类切换当作分类准确率。

| Variant       | Q1 switches | Q2 switches | Q1 churn | Q2 churn |
| ------------- | ----------- | ----------- | -------- | -------- |
| baseline      | 4039        | 4299        | 2719     | 4100     |
| confirmed_1m  | 1238        | 1296        | 1897     | 2812     |
| raw_15m       | 181         | 220         | 495      | 585      |
| confirmed_15m | 58          | 79          | 613      | 730      |

1. 一分钟软确认让日内切换减少约 69%–70%，两季 MDD 都略降，
   但 Q1 多亏 124.87 美元，Q2 改善 1,429.34 美元，尚非跨区间一致改善。
   Q1 政策撤单从 2,364 降到 1,274 次，但目标减仓撤单从 316 增至 545 次；
   手续费还略升。减少一种撤单不代表总交易质量自动提高。
2. 两个慢周期方案在 Q2 明显改善，却在 Q1 明显恶化。
   Q1 的 confirmed_15m 已实现盈利 562.83 美元，被期末库存浮亏 2,385.62 美元抵消，
   最大暴露从基线 21,416.00 增至 31,782.88 美元。
   因此不能用更低费用、较高已完成周期胜率或较少撤单掩盖库存风险。
3. 慢周期两季都没有输出 TrendUp/TrendDown 标签，主要在 Range/Disabled 间变化，
   Q2 另有分钟 HighVolatility 覆盖。原斜率门槛经单位换算后，加上更长窗口及确认，
   并没有得到已经验证有效的慢趋势分类器。直接挑 Q2 的高 Sharpe 宣称成功是不成立的。
4. 目前只能证明工程上分离了时钟、减少了状态切换，不能证明识别“更准确”。
   没有独立的状态真值标签，也未控制相同实际指标窗口、阈值尺度、确认延迟和开盘停买的影响。

### 回归一致性

PASS：两季新 baseline 的完整 `report` 与上轮 baseline 逐字段相等，
包含权益路径、周期账本、费用、库存/风险与诊断，不只是最终收益接近。

PASS：四组逐标的、逐时间点核对，分钟 ATR、价格和原始分钟 regime 完全一致。
差异来自实验分类/确认路径及其后续订单结果，而非偷偷改变原始快信号。

报告保存在本地忽略目录：

- `reports/dynamic-grid-regime-clocks-q1-20260924.json`
- `reports/dynamic-grid-regime-clocks-q2-20260924.json`

它们保留完整配置、诊断与路径，体积较大；本轮未扩大诊断保留策略或改写持久化架构。

## 本轮决定及后续边界

两项能力保留为独立实验开关；现有 Paper/live 均保持 `1`、`false`，不因 Q2 盈利而自动启用。
工程验证通过不等于券商验收通过：本轮没有真实券商成交、账户恢复演练或线上部署。

按照 longbridge-quant 的验证约束，这些结果只支持提出候选，不支持宣称样本外有效：

- 优先继续验证改动最小的一分钟软确认；冻结参数后，在未参与当前研究的时期检验
  净收益、库存损失、MDD、撤单和成交成本，而不是再看过 Q1/Q2 后称它们为 OOS。
- 慢周期暂不作为推荐配置。下一项若继续，应单独控制实际指标窗口与确认延迟，
  再检查斜率/ADX 的分布及开盘停买影响；不同时优化所有参数，也不增加更多指标补救结果。
- 本轮没有新增或运行 Walk-forward、Monte Carlo、敏感性扫描或其他策略基准；
  没有真实 Quote、最低收费、排队和延迟压力测试，不能推断实盘可获得相同收益。
