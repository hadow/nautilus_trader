# Dynamic Grid：单层动态买入

本候选只改变网格买单的激活节奏，不改变 15 分钟＋8 根确认、网格几何、目标仓位、
资金上限、覆盖卖单或动态重置。它不是定投，也不保证比原模式收益更高。

后续整股预算、2 月换层对照及历史范围观测见
[整股预算与受控换层验证](dynamic_grid_funding_review.zh-CN.md)。

## 配置与语义

每个标的的 `strategy.grid.entry_mode` 可设置为 `AllLevels` 或 `Sequential`。
缺省为 `AllLevels`，保留原行为；默认值不写入规范化快照，兼容原配置与检查点。
本轮不修改现有 paper/live 配置，不自动迁移账户状态或重启交易进程。

`Sequential` 的规则：

- 每只股票最多一张未终结的 Grid 买单，包含种子买单；Core 买单不占此名额。
- 已有买单保留，直到成交、原策略撤单或过期，不逐 Tick 改价追踪。
- 空闲时选择最近的合格层。限价买入层必须严格低于当前决策价格；跨过的层不追补数量。
- 一次决策最多创建一个买入意图。提交、实际买入成交或终态回报后，
  必须出现收盘时间更晚的已完成信号 Bar 才能再次激活买单。
  当前配置是分钟 Bar，不是 15 分钟 regime，也不是固定等待 60 秒。
- 部分成交、撤单待确认和 Unknown 仍占名额；撤单请求本身不释放预留。
- 上方种子库存仍按原计划分层，但也逐笔建立；跌破当前网格中心后不继续市价补种子仓。
- 实际成交库存仍按原规则维护覆盖止盈卖单；新的补买等待不阻止该路径。
- 重置不能清空补单时间屏障；恢复仍须通过原订单、持仓、配置一致性校验。

一张买单可能发生多笔部分成交，所以该规则不等于“每分钟最多一笔成交”，
也不保证所有买入（包括 Core）都被拆分。持续下跌仍可能逐分钟累积库存，
因此原目标仓位和组合风控必须保留。

## 短回放对照

复用已有原生回放和单因素对照入口，不新增执行模拟器：

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest --features examples \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_entries_smoke.json \
  crates/backtest/examples/dynamic_grid_entries_comparison.json \
  reports/dynamic-grid-entries-smoke.json
```

两组仅 `entry_mode` 不同；共享六只股票、2025-01-02 至 2025-01-17 的数据、
资金、费用、滑点、随机种子和风控。配置中 AAPL 也参与历史研究，
不代表接管 paper/live 中隔离的 AAPL 外部持仓。

短回放只用于执行行为和研究诊断，不是 OOS 或策略有效性证明。
输入只有分钟 OHLCV，没有真实 Quote、排队位置或券商网络延迟，
不能据此断言实盘闪跌和 V 型反弹的成交结果。

## 基础 Sequential 验证结果

2026-09-26 本地 debug 回放；初始资金 $100,000，AAPL、MSFT、NVDA、TSLA、AMZN、META，
共 11 个交易日、25,740 根分钟 Bar、0 条 Quote。沿用各标的配置，maker 费率 0.08%、
taker 费率 0.10%、额外 commission 为 0；原生撮合模型的一 tick 不利滑点概率为 1，
随机种子 42。这不是券商实际账单最低收费模型。

| 指标                             | 原多层 AllLevels | 单层 Sequential |
| -------------------------------- | ---------------: | --------------: |
| 净收益（美元，含未实现盈亏）     | 538.38           | 420.97          |
| 最大回撤                         | 0.772%           | 0.683%          |
| 成交回报数                       | 60               | 35              |
| 完成网格周期                     | 14               | 9               |
| 已实现 Grid 盈亏（美元）         | 48.55            | 24.44           |
| 费用（美元）                     | 41.84            | 33.94           |
| 成交金额（美元）                 | 44,821.09        | 35,628.27       |
| 平均资金利用率（库存＋待买预留） | 14.44%           | 11.33%          |
| 平均库存暴露占比                 | 12.96%           | 10.94%          |
| 最大库存市值（美元）             | 28,374.48        | 25,634.87       |
| Grid churn（撤单请求数）         | 149              | 21              |

两组都没有组合 RiskOff。单层模式减少预留、成交、费用和库存，也减少了本窗口的利润；
它是买入节奏控制，不是提高资金利用率或收益的保证。净收益的大部分来自期末库存浮盈，
不能把它全部归因于完成网格周期的套利。两组原生事件回放各约 15–16 秒，非性能优化结论。

原先 debug 二进制的同窗口报告保存为 `reports/dynamic-grid-entries-before.json`；
新版 `AllLevels` 的完整有效配置和完整报告与它精确一致，包含各标的/组合权益路径、
周期、费用、诊断、相关性和风险状态。两模式报告为 `reports/dynamic-grid-entries-smoke.json`。

网格模块回归：207 项通过，4 项原有手动性能测试未运行。其中新增 14 项覆盖单层入场、
跳层、部分成交、撤单待确认、Unknown、重复成交、终态补单等待、种子库存、检查点恢复、
未来时间戳拒绝，以及 Nautilus 原生跳空撮合。人工价格路径 `100 → 93 → 100` 下，
原模式完成 3 个周期，单层完成 1 个：说明少买的同时确实可能少捕获快速反弹利润。
另外，现有 `dynamic_grid` 集成测试经 nextest 隔离运行，87 项全部通过。
Longbridge 库 55 项、runner 20 项通过；包含本机模拟 HTTP 服务的对账测试在允许绑定
回环端口的环境运行，不使用真实券商凭据或订单。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib --profile dev -j2 dynamic_grid -- --test-threads=1

CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading --features examples --test dynamic_grid \
  --cargo-profile dev --test-threads 1
```

启用时只需在目标标的的 `strategy.grid` 内增加 `"entry_mode": "Sequential"`。
已有账户检查点会因配置改变而拒绝直接恢复；不要删除检查点强行启动或在线切换后遗留多层买单。
本次未改 paper/live 配置、未重启现有交易进程，也没有进行真实券商成交验收。
已重建 debug 回测程序；Longbridge runner 本轮仅编译测试目标，未替换正在使用的可执行程序。
将来部署候选前需执行 `cargo build -p nautilus-longbridge --features dynamic-grid --bin longbridge-dynamic-grid -j2`，
并单独完成订单/检查点迁移及模拟账户验收；构建成功不代表可以直接修改现有账户配置启动。

修改文件的 nightly rustfmt 检查、`git diff --check` 通过。全仓 `cargo +nightly fmt --all -- --check`
未通过：存在本轮未修改的 `node_intraday_momentum.rs` 与 `slc/mod.rs` 导入格式差异。
`cargo clippy -p nautilus-trading --features examples --lib -- -D warnings` 未通过：
剩余 12 项均位于未修改的 `momentum_pullback/model.rs`、`momentum_pullback/strategy.rs`；
本轮 Dynamic Grid 生产代码无剩余 Clippy 诊断。未宣称全仓静态检查通过，也未做无关修复。

## Sequential 后续单因素验证

本轮仍以原 Sequential 为对照，新增两个默认关闭的研究选项，不改变 paper/live 配置：

- `sequential_requote_bars: 2`：同一更近限价层连续两根已完成信号 Bar 确认后，
  才请求撤销原买单。只处理当前网格、零成交、Accepted、仍低于市价的限价买单。
  撤单确认前保留全部剩余预留；终态之后等待新的完成 Bar，再重新选层和经过原金额风控。
  不缓存一张“必定补发”的新单，不移动网格，不放大额度。
- `fill_cost_exits: true`：全额成交且尚无卖单历史的 Grid 限价买入，允许改用更近的原网格层止盈。
  新目标必须在原买入限价层或更低，并覆盖实际入场金额、已收取入场费用、
  预计出场费用、出场滑点与原最低利润要求。普通取整造成的一 tick 差不算换层。
  Core、种子买入、旧代库存及已有卖单不改价；部分成交仍立即走原覆盖卖单路径。

两项均要求 `entry_mode: Sequential`。不配置时不写入规范化配置，保留旧配置快照。
换层确认只由完成 Bar 更新，重复 Tick 不累计；漏 Bar、隔夜、候选层变化会重新计数。
未完成确认不写入检查点，重启/对账后重新观察，不能凭旧候选直接撤挂。
这里的两根 Bar 是信号分钟线，与 15 分钟＋8 根 regime 确认无关。

成本止盈只修改尚未挂单库存的目标，不修改原始买入参考价。完整周期仍按真实成交及费用核算；
发生较大价格改善时，按原始决策价计算的 gross PnL 可能为负，而实际成交净利润为正，
二者不能混用。成本门槛是模型预算，并不保证实际券商账单下每笔必然盈利。

### 诊断先行

`diagnostics.sequential_entries` 记录通过上游门禁后，每根信号 Bar 的首次入场决策。
类别包含 `WAIT_NEW_BAR`、`WORKING_BUY`、`WORKING_BUY_NEARER_LEVEL`、`NO_ELIGIBLE_LEVEL`、
`INSTRUMENT_SIZING`、`PORTFOLIO_SIZING`、`FINAL_ORDER_GATE` 和 `SUBMITTED`。
它不是每个 Tick 的计数，也不是互相独立的交易机会；上游阻止原因仍看原 `blocked_bars`。
更近候选仅证明几何、库存占位、方向及成本条件满足，不代表撤单后必有预算或真实成交。

原短窗口观测：NVDA 203 根、TSLA 112 根 Bar 出现更近候选，合计 315 根。
另有 1,316 根被工作买单占位、360 根没有合格层、5 根等待新 Bar、25 根提交买单。
META 的两个网格各 16 层全部数量为零，解释了其 360 根无合格层；本轮没有顺带改其资金或层数。

加入诊断后，旧 AllLevels 完整报告保持精确一致；Sequential 排除新增诊断字段后也精确一致。
核对包含有效配置、全部权益点、周期、费用、库存、相关性和风险状态，不只比较最终收益。
诊断回放保存于 `reports/dynamic-grid-sequential-diagnostics.json`。

### 独立回放命令

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest --features examples \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_entries_smoke.json \
  crates/backtest/examples/dynamic_grid_sequential_research.json \
  reports/dynamic-grid-sequential-research.json
```

三组分别为原 Sequential、只启用换层、只启用成本止盈，没有组合两项，也没有参数扫描。
数据、资金、费用、滑点和种子均与前述 11 个交易日短窗口相同。
这是已反复观察的研究窗口，不是样本外数据。

### 最终短窗口结果

| 指标                     | Sequential | 仅受控换层 | 仅成本止盈 |
| ------------------------ | ---------- | ---------- | ---------- |
| 净收益（美元，含浮盈）   | 420.97     | 451.15     | 420.97     |
| 已实现 Grid 盈亏（美元） | 24.44      | 61.22      | 24.44      |
| 期末未实现盈亏（美元）   | 394.73     | 383.78     | 394.73     |
| 完成网格周期             | 9          | 19         | 9          |
| 成交回报数               | 35         | 77         | 35         |
| 费用（美元）             | 33.94      | 43.46      | 33.94      |
| 成交金额（美元）         | 35,628.27  | 46,567.87  | 35,628.27  |
| 最大回撤                 | 0.6834%    | 0.6927%    | 0.6834%    |
| 平均资金利用率           | 11.3311%   | 11.5382%   | 11.3311%   |
| 最大库存市值（美元）     | 25,634.87  | 27,207.95  | 25,634.87  |
| 平均完成周期持有小时     | 35.81      | 27.58      | 35.81      |
| 撤单请求数               | 21         | 37         | 21         |

受控换层实际撤单 7 次（NVDA 4 次、TSLA 3 次）。新增订单和成交会进一步影响后续原策略撤单，
因此总撤单请求增加 16 次，不应把两者混为一谈。网格已实现利润增加 $36.78，
组合净收益增加 $30.18；费用增加 $9.52，最大库存和 MDD 也略增。
资金利用率仅提高约 0.21 个百分点，并没有解决整体低利用率问题。

成本止盈在修复取整边界后没有触发实际目标调整，完整报告与原 Sequential 精确一致。
人工原生撮合路径 `100 → 93 → 95` 中，它完成 1 个周期，原 Sequential 为 0 个；
这证明执行机制有效，不证明它在实际样本中有增量价值。
开发过程中发现普通取整也会把目标降低一 tick，已用先失败后通过的回归测试排除，
没有为改善历史收益调整费用、间距、确认数或风险预算。

结论：受控换层保留为下一阶段候选，成本止盈保持关闭等待真正跨层价格改善样本。
两项均不晋升默认或 paper/live 配置；大部分组合收益仍来自期末浮盈。
本轮没有 OOS、Walk-forward、参数扫描或两项叠加实验，也没有真实 Quote、排队、网络延迟验收。
按 longbridge-quant 的单因素验证约束，这些数字只能支持继续验证，不能宣称长期更优。
下一步先冻结受控换层规则，验证独立时间窗口及费用/延迟压力，再做模拟账户撤单竞态验收；
暂不加入库存偏斜选层或更多参数。

### 工程验收与修改范围

- PASS：网格模块 227 项测试通过，4 项原有手动性能测试未运行。本轮新增 20 个测试用例，
  覆盖诊断去重、两根确认、撤单预留屏障、部分成交/Unknown 不换层、漏 Bar、候选变化、
  重启重新确认、真实费用、Core/种子/旧网格保护、取整边界和原生跳空反弹撮合。
- PASS：原生 `dynamic_grid` 集成测试 87 项通过；包含多标的共享资金、风险及恢复回归。
- PASS：Longbridge 离线库测试 55 项、runner 测试 20 项通过。
  其中 HTTP 对账使用本机测试服务，不代表本轮进行了真实券商验收。
- PASS：`cargo check`、debug 回测构建、修改文件 nightly rustfmt、Markdown lint、`git diff --check`。
- PARTIAL：严格 Clippy 失败，仍为未修改的 `momentum_pullback/model.rs` 与 `strategy.rs` 的
  12 项既有诊断；Dynamic Grid 无新增 Clippy 诊断。未绕过检查或修复无关策略。
- NOT RUN：全仓/release 构建、OOS/Walk-forward、账户实测。磁盘余量约 1 GiB，
  本轮使用已有 debug 产物，没有清理用户缓存或尝试绕过系统安全限制。

```bash
CARGO_INCREMENTAL=0 cargo check -p nautilus-trading --features examples --lib -j2

CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib --profile dev -j2 dynamic_grid -- --test-threads=1

CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading --features examples --test dynamic_grid \
  --cargo-profile dev --test-threads 1

CARGO_INCREMENTAL=0 cargo test -p nautilus-longbridge --features dynamic-grid \
  --lib --bin longbridge-dynamic-grid --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading --features examples \
  --lib -j2 -- -D warnings
```

修改集中在现有 `dynamic_grid` 模块：

- `config.rs`：两个 opt-in 字段，默认序列化兼容与配置校验。
- `strategy.rs`：复用候选选层，记录首次决策，确认换层及成交后目标调整。
- `orders.rs`：在共享库存账本里校验并调整尚未覆盖的止盈目标。
- `diagnostics.rs`：Sequential 计数及目标调整审计。
- `multi_asset.rs`：启用换层时要求时间型信号 Bar。
- `multi_asset/entry_tests.rs`：扩充当前策略/订单链路测试，不另建执行框架。
- `crates/backtest/examples/dynamic_grid_sequential_research.json`：新增三组独立候选配置。
- 本文：记录规则、实际结果、复现方法及限制。

未删除文件、未修改当前 paper/live 配置、未重启交易进程、未提交 Git。
已有工作区内其他修改保留。TDD 用于先复现行为与取整问题，再验证修复；
研究结果不足以支持启用成本止盈或把受控换层直接上线。
