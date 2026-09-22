# Multi-Asset Dynamic Grid：组合实现与验收

## 范围

本实现使用一个 `MultiAssetGridStrategy` 和一个原生 `StrategyCore`，不是为每只股票建立一个 Strategy。
`DynamicGridStrategy` 保留为该类型的兼容名称，旧 `DynamicGridConfig` 转换为只有一个成员的组合。
核心网格规则继续使用 [动态网格说明](dynamic_grid.zh-CN.md) 中的几何网格、成交账本和动态重建逻辑。

支持原生多标的 Bar/Quote 回测、Sandbox，以及现有 Longbridge Paper/Live execution client。
本次没有连接券商、发送订单、提交 Git 或修改 Longbridge adapter 的执行实现。
离线验证不等于远端 Paper 或实盘验收。

## 状态与执行架构

```text
一个 MultiAssetGridStrategy / StrategyCore / account
  ├─ InstrumentId → GridStrategyEngine
  │    ├─ 独立 GridState / center / levels / spacing / generation
  │    ├─ 独立 ATR / ADX / MA / Bollinger / volatility / regime
  │    ├─ 独立 orders / pending reservations / inventory / cycles
  │    └─ 独立 loss limits / reset counts / performance
  ├─ PortfolioRiskManager
  │    ├─ 共享现金与全部未决买单
  │    ├─ 组合回撤 / 日损失 / 总敞口 / 现金储备
  │    └─ 行业 / 相关性 / 动态预算
  └─ 唯一订单提交入口 → 原生 Risk / Execution → Backtest / Sandbox / Longbridge
```

行情按完整 `InstrumentId` 和该股票的 `BarType` 路由。另一只股票的价格不更新本股票的指标、中心或仓位。
成交按 `instrument_id + client_order_id` 找到账本；未知成交、跨标的身份冲突或库存不一致触发组合恢复屏障。
事件在 Nautilus 线程上同步处理，没有在容量检查与提交之间等待异步任务。

买入在创建持久化意图前预检，在真正提交前再经过同一个组合容量算法。
先到达的意图即刻占用现金；后续股票看到已经减少的容量，返回 Allow / Reduce / Defer / Reject。
Reduce 向下取整到该股票 lot；Defer 不提交、不保留空库存 lot；撤单请求、未知状态和部分成交剩余数量仍占用预留。
组合风险优先于单股票开仓许可；组合风险锁定后取消新增买单，仍允许已知库存覆盖的退出。
撤单被拒绝后保留 Unknown 状态和资金预留，通过状态查询恢复，不在事件回调内立即重复撤单。

订单命名包含策略 tag、完整 InstrumentId、grid generation、signed level、side 和持久化 sequence。
`GridOrder.lot_id` 指向入口身份，唯一确定所属 cycle；退出不借用其他股票的库存。

## 资金口径与两层风险

初始只有一笔 `portfolio.capital`。每个股票账本有一个已分配资金基准，用于独立 PnL 与局部回撤归因，
不是可以独立透支的账户。共享现金按下式计算，并再次受券商实际 cash/equity 限制：

```text
portfolio cash = initial capital + Σ(instrument ledger cash - initial instrument allocation)
portfolio equity = portfolio cash + Σ(marked inventory)
```

未分配预算始终保留在共享现金中。任一股票重建不会重新注资，也不会把其他股票预留的现金当作自身余额。
broker free cash 已锁定的订单可能再次被保守预留；这种情况会少下单，不会通过乐观释放资金扩大容量。

| 配置位置 | 口径 |
| --- | --- |
| `strategy.capital_allocation` | 初始组合资金分配比例，也是动态预算恢复的上限。 |
| `strategy.max_position_pct` | 该股票库存加待成交买单 / 当前组合权益。 |
| `strategy.grid.capital` | 运行时由组合分配计算，不是额外入金。 |
| `strategy.grid.capital_allocation` | 该股票已分配资金中用于建立网格的比例。 |
| `strategy.grid.max_position_pct` | 股票自身账本权益口径的第二道持仓限制。 |
| `strategy.grid.max_drawdown / max_daily_loss` | 股票自身资金曲线的局部风险限制。 |
| `portfolio.max_total_exposure / max_total_grid_exposure / max_total_equity_exposure` | 组合权益口径；新增买入对三者均预留。 |
| `portfolio.min_cash_reserve` | 不可被新买单占用的组合权益比例。 |
| `portfolio.max_sector_exposure / max_correlated_exposure` | 同行业或整个相关性连通组的库存加未决买单 / 组合权益。 |

`max_position`、`max_grid_exposure`、`max_notional`、`max_orders`、`max_consecutive_resets` 等仍逐股票生效。
组合日损失使用 UTC 日边界，包含隔夜跳空；组合风险锁跨重启保留，不随单股票盈利或次日到来自动解除。
`reset_risk()` 是显式操作员接口：必须所有订单终结、所有库存对账、价格新鲜且两层风险均允许，才恢复交易。
失败时保持恢复屏障；runner 不自动调用。

限额是订单准入约束，不是对最大亏损的保证。跳空、未知成交和取消失败仍可能使实际敞口暂时超过预算。
组合锁定采用停止加仓/撤 BUY/允许 covered exits；它不会自动把全账户市价清空。
单股票原有 `RiskPolicy::Flatten` 在该股票自身风险触发时仍可使用。

## 动态预算与相关性

动态预算以初始分配为上限，根据独立 regime、ATR/price 和该股票累计 marked PnL 降低或恢复。
持续亏损或波动增加会降低预算；恢复盈利/低风险状态可恢复预算，但不能突破原始分配。
这不是追逐历史赢家或把亏损股票的资金自动加倍。预算削减后的余款留在共享现金中。

普通调整受 `min_reallocation_interval_secs` 和 `min_allocation_change_pct` 双重约束；
Disabled 配置、HighVolatility 或该股票 RiskOff 立即禁止新买入，不等待调仓间隔。
缩减预算不会把已有库存伪装为现金；先取消多余买单，库存保留原退出和成本。

行业来自每股票显式 `sector`，本实现不把样例标签当作实时行业数据库；缺失标签合并为 Unknown 行业。
相关性使用过去 `correlation_lookback_days` 个日历日中的已完成 UTC 日收盘收益。
仅配对相同起止日期的收益区间，不补未来价格，不用当前未结束日的 close 计算相关性。
交易日不同导致样本不足时，相关性输出 null，风控按可能完全相关处理。
正相关超过 threshold 的股票形成连通组，A–B、B–C 的集中度不会因为 A–C 较低而被漏掉。
没有用负相关抵消 long-only 组合的实际现金敞口。

相关矩阵每日按需更新，同日缓存；缓存不作为恢复事实，重启由持久化日收盘重算。
报告输出最终相关矩阵，null 不代表零相关。
持仓股票行情过期时，其他股票不能利用不可靠的组合估值扩张仓位；非持仓股票停牌不会凭空生成价格。

## 配置与命令

回测主配置：`crates/backtest/examples/dynamic_grid_portfolio.json`。
每股票的独立配置位于同级 `instuments` 目录（沿用配置中的目录拼写）：

```text
crates/backtest/examples/
├── dynamic_grid_portfolio.json
└── instuments/
    ├── AAPL.json
    ├── MSFT.json
    ├── NVDA.json
    ├── TSLA.json
    ├── AMZN.json
    └── META.json
```

主文件保留组合风控、回测时间范围和标的文件引用：

```json
"instruments": {
  "AAPL.SIM": "instuments/AAPL.json",
  "MSFT.SIM": "instuments/MSFT.json",
  "NVDA.SIM": "instuments/NVDA.json",
  "TSLA.SIM": "instuments/TSLA.json",
  "AMZN.SIM": "instuments/AMZN.json",
  "META.SIM": "instuments/META.json"
}
```

修改某股票时，只需编辑对应文件的 `strategy.grid`；其资金分配、Bar 类型、tick/lot 和行情路径也在该文件。
文件引用相对于主配置文件所在目录解析，支持绝对路径；不扫描目录、不递归引用，不会自动加入未列出的股票。
`bars_path` / `quotes_path` 保持原有相对于运行工作目录的规则，现有样例仍从仓库根目录运行。
`load_portfolio_config` 在启动前展开并校验所有引用；缺失/非法文件、未知字段和标的身份不匹配直接报错。
旧内联格式和内联/文件混合格式继续支持。回测报告保存完整展开后的参数，并禁止输出覆盖被引用的标的文件。
Paper 通过 `portfolio_config` 引用此主文件，复用全部组合风控和逐标的参数；不再维护独立参数副本。
Sandbox 样例保留原有保守内联配置，runner 继续兼容旧内联格式。

当前六股票共享文件采用进取型工程预设；组合回撤阈值按用户选择设为 25%，Paper 同步使用。
Rust `Default`、单股票/WFA 候选及 Sandbox 样例保持不变。
参数在本轮历史比较前固定，不依据这一季度的收益挑选参数；更高风险预算不保证更高收益。

| 股票 | 分配比例（旧 → 新） | 组合权益口径持仓上限（旧 → 新） |
| --- | --- | --- |
| AAPL | 15% → 18% | 10% → 15% |
| MSFT | 15% → 18% | 10% → 15% |
| NVDA | 10% → 14% | 8% → 12% |
| TSLA | 10% → 10% | 5% → 8% |
| AMZN | 10% → 10% | 8% → 10% |
| META | 10% → 10% | 8% → 10% |

分配预算合计由 70% 增至 80%，但实际库存加待成交买单仍受组合 70% 总敞口上限约束。
预算不等于目标持仓；regime、行业/相关性、动态预算和可用现金仍可降低实际利用率。

| 配置字段 | 旧值 → 进取型回测值 |
| --- | --- |
| `portfolio.max_total_exposure / max_total_grid_exposure / max_total_equity_exposure` | 60% → 70% |
| `portfolio.min_cash_reserve` | 30% → 20% |
| `portfolio.max_portfolio_drawdown` | 15% → 25% |
| `portfolio.max_portfolio_daily_loss` | 5% → 7% |
| `portfolio.max_sector_exposure` | 30% → 40% |
| `portfolio.max_correlated_exposure` | 30% → 50% |
| 每股票 `grid.initial_inventory_fraction` | 0 → 20% |
| 每股票 `grid.max_drawdown` | 10% → 20% |
| 每股票 `grid.max_daily_loss` | 3% → 6% |
| 每股票 `grid.max_unrealized_loss` | 8% → 15% |
| 每股票 `grid.max_consecutive_resets` | 5 → 30 |
| 每股票 `grid.minimum_reset_interval_secs` | 300 → 900 |
| 每股票 `grid.minimum_reset_distance` | 1% → 1.5% |

`initial_inventory_fraction` 是每轮新网格资金划给上方卖出网格的比例，已有逻辑会先买入对应库存；
不是组合权益的 20%，也不是仅进程首次启动时买入一次。每轮仍经过同一个资金与风险准入检查。
这是资金划分比例，不保证实际底仓达到 20%；按每层资金和整股 lot 向下取整后，高价股票的部分或全部上方层数量可能为零。
增加重置间隔/距离用于减少反复移网格；30 次无盈利周期的连续重置仍会锁定 RiskOff，不会自动恢复。
局部亏损阈值使用股票自身账本资金口径，因此可能先于组合 25% 阈值停买。

保留 Equal 仓位、原 ATR/网格层数、趋势/波动过滤、下跌趋势禁买及手续费/滑点假设。
MSFT 使用用户调整后的 2.5%–5% ATR 间距范围，备用 `spacing_pct` 同步为 2.5%，满足既有配置校验；
其余五只股票仍使用 0.5%–3% 范围。备用固定间距不参与当前 ATR 模式的间距计算。
没有增加杠杆、倍增加仓或关闭风控；这些取舍也符合
[Grid Trading Strategy Guide](https://blog.traderspost.io/article/grid-trading-strategy-guide) 对成本、趋势与储备资金的约束。

网格 JSON 样例显式列出全部 54 个 `GridConfig` 字段；组合样例也列出全部 19 个 `PortfolioConfig` 字段。
股票差异配置和上述进取型覆盖均显式写入文件；其余值展开自 Rust 默认值。
单股票和 Walk-forward 候选 JSON 同样完整列出字段，研究参数不再依赖隐藏默认值。
拆分后的标的文件中字段位于 `strategy.grid`；内联格式仍为 `instruments[InstrumentId].strategy.grid`。
组合风控位于主文件顶层 `portfolio`。
Decimal 金额和比例使用字符串；JSON 不支持注释，参数口径见上表及 Rust 字段文档。

拆分/补全字段本身不改变参数或解除 RiskOff；进取型参数是本轮单独、明确的配置调整。
多资产样例中的 `grid.capital` 展示类型默认值，
运行时仍由 `portfolio.capital × strategy.capital_allocation` 覆盖，不代表每股票额外获得一笔资金。
配置在启动时加载，不支持热更新；已有 checkpoint 继续执行配置一致性检查。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples --lib --test dynamic_grid --profile dev -j 2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo build --release -p nautilus-backtest -p nautilus-longbridge --no-default-features --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid --bin dynamic-grid-backtest --bin longbridge-dynamic-grid -j 2

target/release/dynamic-grid-backtest --portfolio crates/backtest/examples/dynamic_grid_portfolio.json reports/dynamic-grid-portfolio-aggressive-2025-q1.json
```

每股票指定自己的 `bars_path`、`quotes_path`、tick 和 lot。读取 plain CSV 或 gzip CSV，支持原有 6/8/9 列 OHLCV 格式。
Bar 时间必须表示完成时间；Tick 模式需要每股票真实 quotes，不能从一只股票的数据生成其他股票。
事件按 timestamp、Bar/Quote 类型、InstrumentId 稳定排序；同时间戳的资金优先级因此可复现，并非随机公平分配。

Longbridge 六股票配置：

Paper 配置文件只保留环境字段及显式映射，例如：

```json
{
  "mode": "Paper",
  "portfolio_config": "../../../backtest/examples/dynamic_grid_portfolio.json",
  "instrument_mapping": {
    "AAPL.SIM": "AAPL.US.LONGBRIDGE",
    "MSFT.SIM": "MSFT.US.LONGBRIDGE",
    "NVDA.SIM": "NVDA.US.LONGBRIDGE",
    "TSLA.SIM": "TSLA.US.LONGBRIDGE",
    "AMZN.SIM": "AMZN.US.LONGBRIDGE",
    "META.SIM": "META.US.LONGBRIDGE"
  },
  "trader_id": "GRID-001",
  "account_id": "LONGBRIDGE-001",
  "state_path": "reports/multi-grid-paper-state.json",
  "report_path": "reports/multi-grid-paper-report.json",
  "sandbox_fee_per_share": null
}
```

`portfolio_config` 相对于 Paper 文件解析；标的引用继续相对于共享主文件解析。
映射必须完整、一对一并保持股票身份，不能把 AAPL 参数映射到 NVDA；新增标的须同时补全映射。
引用模式禁止同时出现内联 `instruments`、`portfolio` 或 `currency`，避免覆盖优先级造成参数漂移。
修改共享主文件的 `portfolio` 或 `instuments/*.json` 后，回测与 Paper 下次启动读取同一来源。
启动只读验证会输出完整的有效策略配置；不会读取历史 CSV、联网或创建 checkpoint。
配置来源文件也受 state/report/lock/next 的覆盖检查保护。

所有 GridConfig 字段、资金分配、持仓比例、行业、启用状态及组合风控保持一致。
必要的执行差异仍保留：Paper 映射为 Longbridge 标的 ID，使用确认完成的自定义 Bar 和 tick 执行；
回测使用配置的 Bar/Quote 回放模式。历史路径、回测时间范围、随机种子与撮合概率不会用于 Paper。
Paper 价格 tick 来自同一标的配置，实际交易 lot 与成交由券商基础设施处理；配置一致不代表撮合或收益一致。
25% 是停买风控阈值，不是亏损保证；旧参数 checkpoint 不会自动迁移或删除，已有持仓时必须先对账和审计迁移。

```bash
# 仅验证配置，不联网、不创建状态文件、不发送订单
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_paper.json
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_sandbox.json

# 以下是用户确认账户与预算后主动启动的命令，本次未运行
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_sandbox.json --run
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_paper.json --run
```

runner 为全部标的设置行情 tick、external order claims、原生风控和 reconciliation 范围。
共享账户只查询一次，不为每股票重复建立 execution client。
Live 仍要求配置 `mode=Live` 且同时传入 `--run --live`；必须使用独立 state/report 文件。

## 组合恢复与升级

一个带独占锁的 checkpoint 原子保存所有股票、组合风险、预算、相关性输入、原生订单/持仓和绩效状态。
broker reconciliation 前先恢复完整原生 cache；所有股票对账通过且共享现金检查通过后才解除恢复屏障。
恢复验证完整 instrument 集合、订单命名空间、sequence、库存关系、broker ownership 和环境/账户身份。
未知活动订单、外部持仓、部分丢失的快照或股票状态互换均不能静默继续。

状态格式升级到 version 2；原单标的 version 1 不能直接加载。
如果已有真实 Paper/Live 库存，必须停机审计并迁移状态，不能删除文件、换新 ID 或使用新账户初始余额假装恢复。
样例采用新的 `multi-grid-*-state.json` 路径，避免覆盖原文件；新路径不绕过 broker 中已有仓位/订单的检查。
同一账户须专供本组合使用；部署层仍需防止多个进程使用不同 checkpoint 同时交易该账户。

## 指标与基准

每股票输出 realized/unrealized/net PnL、费用、滑点、cycles、持仓、reset 和独立曲线。
组合收益、Sharpe、Sortino、MDD、Calmar、敞口和利用率由组合现金加全部已知库存的时间序列计算，
不平均股票 Sharpe，也不把每股票初始资金重复相加。费用、成交额和周期 PnL 按 Decimal 汇总。
组合没有可相加的“股票数量”，通用指标中的组合 maximum_position 为零；使用 maximum_exposure 比较不同股票。
组合没有单一 regime，通用组合 `trend_exposure` 暂为零；趋势敞口应查看每股票报告，不能将该零值理解为没有趋势风险。
`grid_turnover` 表示实际成交额 / 初始资金，`capital_utilization` 包含待成交买单预留。

Equal Weight Buy & Hold 使用同一个原生账户、一个策略实例，按全部配置标的等分初始资金，
在各自第一根可用 Bar 后买入、不再平衡；未开始交易的股票预算继续保持现金。
同时输出每股票 Buy & Hold sleeve。等权基准不使用动态网格的 enabled/regime/风险配置，因此总敞口不同。
费用与滑点仍通过原生撮合产生。比较必须同时查看敞口、现金利用率和换手，不能只看收益。

## 验证与历史结果

### 历史状态查询与回测进度优化

2026-09-21 按用户要求终止了尚未完成的进取型季度回测，未删除旧报告、配置或交易状态。
本轮优化保持全部风控检查、看门狗定时器、历史事件和恢复语义不变：

- 股票风险快照和组合容量查询使用现有原生缓存的短作用域只读借用，避免复制整个账户和持仓历史。
- 余额、归属和数量仍每次读取当前值，调用组合估值或发送命令前释放借用；不缓存过期资金容量。
- Paper 账户查询定时器仅取账户 ID，不再为此复制整个账户。
- 组合回测通过原生消息总线观察 Bar/Quote 进度，不切分回放、不改变撮合或策略状态。
- CLI 在 stderr 输出加载、策略、基准和写报告阶段，每 1,000 条行情以及首尾事件输出计数、回放时间和耗时。
  计数不包含看门狗事件；100% 表示输入行情已发布，不代表最终对账和报告已经写完。
- JSON 报告使用标准缓冲写入并显式检查 flush 错误，保留原报告格式。

网格核心检查 **47/47** 通过（含显式运行的 1 项性能检查），集成检查 **57/57** 通过。
新增检查覆盖余额变化立即生效、拒绝外部策略或空头持仓、借用释放，以及进度观察前后完整报告一致、
同时间多标的 Bar/Quote 计数和跨回测订阅清理。

本地定向性能检查在 macOS 12.7.6、Intel i5-5257U、Rust 1.98.0、dev profile 下运行。
每次执行 16 轮股票和组合风控查询，排除构造耗时，三次取最小值。
修复前账户历史 1 条和 10,000 条分别耗时 0.45 毫秒、102.81 毫秒；修复后分别约 0.25 毫秒、0.25 毫秒。
这是特定查询路径的性能验证，不代表完整季度加速倍数或策略收益改善。

两个 runner 的 Release 构建通过。实际 CLI 使用冻结的当前六股票配置，回放 2025-01-02 的 2,340 根 Bar，
包含读取数据、动态网格、等权基准和完整 JSON 写入；`/usr/bin/time -p` 测得优化前两次为 10.46 / 11.27 秒，
优化后两次为 2.78 / 2.70 秒，约快 4 倍。四份完整 JSON 报告深度比较相等，不只是最终收益相等。
这是单日端到端样本，不外推季度运行时间。2025-01-02 至 2025-01-03 的 4,680 根 Bar 回放也完成，耗时 3.98 秒。
进度输出覆盖动态网格和等权基准；当前没有重新运行整季度回测。
新版 Paper runner 的只读配置输出与优化前逐字节一致，未联网、发送订单或创建交易状态文件。
Rust 格式检查通过；Clippy 保留已有的 12 项其他策略告警及测试文件原有的 2 项分号告警，本次未新增告警。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples --lib risk_reads_do_not_scale_with_account_history --profile dev -j 2 -- --ignored --nocapture
```

### Paper 配置统一阶段

Paper 配置统一后，网格核心单元测试 **43/43**、集成测试 **54/54** 通过。
其中新增 21 项直接测试 Longbridge runner 的生产配置模块，覆盖全部参数一致性、共享文件更新、
相对路径、不读取历史数据、旧内联格式、映射冲突及配置文件覆盖保护。
使用当前共享参数的一天六股票 CLI 回放通过，共 2,340 根 Bar；该检查只验证加载和执行链路，不证明收益有效性。
两个 runner 的最终 Release 构建通过；Paper 实际程序输出的六股票参数与共享配置逐字段一致，
从不同工作目录读取引用也通过，Sandbox 配置未改变，未联网或写入交易状态。
Rust 格式检查通过；Clippy 未新增告警，保留其他策略已有的 12 项告警。
Markdown 检查与修改前均有 136 项既有表格对齐问题，本次不改动无关表格。

组合实现阶段的 trading 单元测试 **600/600**、网格原生集成测试 **16/16** 通过。
配置显式化新增 7 项完整性检查，集成测试重新执行 **23/23** 通过；Paper/Sandbox 加载后的有效配置不变。
配置拆分新增 10 项用例，覆盖内联/混合/文件格式、相对路径、缺失文件、非法字段及标的身份等校验。
应用进取型参数后全部 **33/33** 集成测试重新通过；新版回测 CLI 的 Release 构建通过。
补跑网格核心单元测试 **43/43** 通过（其余 557 项非网格用例未在本轮重跑）。
实际 CLI 分别拒绝报告覆盖主配置和被引用的标的配置；Rust 格式检查及 `git diff --check` 通过。
包括同时间多股票资金预留、Bar/Quote 两种执行、组合风险优先、不同交易日、跨股票无未来数据泄漏、
带非空库存的整体恢复、交换股票快照拒绝、未知撤单不重试，以及已有单股票回归测试。
已有 Longbridge disconnected-guard 集成检查和 Sandbox 显式手续费检查也在这 16 项内，前者不发送远端订单。

两个 runner 的 `cargo build --release` 成功，随后对最终源码再次执行构建成功。
Release Paper/Sandbox 默认校验均成功：各六个标的、完整 external order claims，没有创建状态文件或联网。
附加 runner 单测构建因磁盘余量不足 400 MB 而主动中止，不能算作通过；上述 trading 测试已完整执行。

两个 runner 的 Clippy 检查完成：本功能文件零告警；已有 `momentum_pullback` 模块的 12 项告警未改动。
该次命令使用 `--cap-lints warn`，不声称整个仓库通过严格无告警检查。
受影响 Rust 文件通过 `rustfmt +nightly --check`，相关已跟踪文件通过 `git diff --check`。
本轮未运行全仓 `make pre-commit` / `make pre-flight`，也未提交 PR。

以下首先保留旧保守配置的历史基线，不能当作当前进取型配置的结果。
历史输入为仓库本地六股票分钟 CSV，每只 23,400 根、共 140,400 根 Bar，
实际范围为 2025-01-02 14:31 UTC 至 2025-03-31 20:00 UTC。
初始资金 USD 100,000，随机种子 42；每股票在输入窗口内独立 warm-up。
采用原生 OHLC 撮合和概率 1 的 one-tick slippage；默认 maker 8 bps、taker 10 bps、commission 0，
配置中的 5 bps slippage 用于保守准入/利润门槛，不再从实际成交净 PnL 重复扣除。
这些是可复现测试假设，并非 Longbridge 的实际收费表。

实际输出：`reports/dynamic-grid-portfolio-2025-q1.json`（约 83 MB，包含完整曲线、逐股票指标和最终相关矩阵）。
动态组合与基准的净 PnL、已实现/未实现 PnL、费用、滑点和周期 gross PnL 均已逐股票精确求和对账。

| 指标 | Dynamic Multi-Asset Grid | Equal Weight Buy & Hold |
| --- | ---: | ---: |
| Total Return | +0.11864% | −15.53407% |
| Net PnL / USD | +118.64 | −15,534.07 |
| MDD | 0.51962% | 21.66486% |
| Sharpe | 0.6808 | −2.1266 |
| Sortino | 0.9452 | −2.7067 |
| Calmar | 0.9470 | −2.3211 |
| 最长回撤持续天数 | 24.9434 | 84.1618 |
| Fees / USD | 25.56 | 99.31 |
| 平均资金利用率（含预留） | 1.6522% | 99.3832% |
| 平均库存敞口 / 初始资金 | 1.1307% | 95.4323% |
| 最大库存敞口 / USD | 8,217.57 | 103,694.72 |
| 成交额 / 初始资金 | 0.319714 | 0.9931143 |
| 成交笔数 | 26 | 6 |
| 完成网格周期 | 13 | 不适用 |

| 股票 | Grid Net PnL / USD | 完成周期 | Grid Fees / USD | 最终局部风险状态 |
| --- | ---: | ---: | ---: | --- |
| AAPL | 30.11 | 3 | 6.27 | 连续 reset 达到 5 次上限 |
| MSFT | 0.00 | 0 | 0.00 | 连续 reset 达到 5 次上限 |
| NVDA | 16.00 | 1 | 2.00 | 连续 reset 达到 5 次上限 |
| TSLA | 67.91 | 8 | 15.87 | 连续 reset 达到 5 次上限 |
| AMZN | 4.62 | 1 | 1.42 | 连续 reset 达到 5 次上限 |
| META | 0.00 | 0 | 0.00 | 连续 reset 达到 5 次上限 |

**这不是策略优越性的证明。** 六只股票最终均因默认连续 reset 风控上限而停止新增买入，
MSFT/META 完全没有成交；组合自身回撤/日损失锁未触发。期末动态网格没有剩余库存。
低利用率、保留现金和局部 RiskOff 是与近满仓基准结果差异的重要来源。
该旧基线使用 5 次 reset 上限；当前进取型预设将其提高至 30 次，二者需分别比较。
同一季度的前后对比不是独立 OOS 结论。

13 个周期均为正，但样本过少，profit factor 因没有亏损周期为 null，不报告成无限大。
周期决策价 gross PnL 为 USD 88.26，signed execution shortfall 为 **−55.94**（有利成交差价），
扣除 USD 25.56 费用后净利为 USD 118.64；这解释了大于 1 的 capture ratio，不能误解为零摩擦成交。
基准的 gross PnL 字段仅统计已完成周期，因此为零；它的实际浮动亏损计入 net/unrealized PnL。
`risk.decisions` 统计含预检在内的准入判断次数，不等于实际提交或成交订单数。

### 进取型预设的验证状态

截至 2026-09-21 本轮配置交付，43 项网格核心单元测试和 33 项集成测试通过，Release 构建成功。
相同六股票在 2025-01-02 的单日诊断回放共 2,340 根 Bar：旧配置 8.79 秒、新配置 9.83 秒，均正常完成。
两次等权 Buy & Hold 输出完全一致，组合与股票 PnL/费用精确求和一致，完整周期的费用/滑点恒等式通过。
该单日回放仅验证执行与配置，不用于收益结论或重新选择参数。

先前启动的进取型完整季度回测已按用户要求终止，未生成完整结果，原保守基线未覆盖。
本轮性能验证不会自动重启季度任务，不能宣称利用率、收益或风险调整收益已经改善。
后续需重新运行并核对完整季度结果，再做独立样本外和压力测试；共享 Paper 配置验证不等于远端交易验收。

## 文件与限制

新增核心模块：`dynamic_grid/multi_asset.rs`、`dynamic_grid/portfolio.rs`、`dynamic_grid/portfolio_tests.rs`。
新增回测模块：`crates/backtest/src/dynamic_grid/portfolio.rs` 与六股票 JSON。
重构原 `strategy.rs` 为独立股票引擎；扩展 analytics、原生集成测试、两个现有 runner 和 Paper/Sandbox JSON。
CSV 读取复用 workspace 已有 flate2，未引入独立交易/存储框架。这是 Ponytail 技能对本次实现的主要约束。

当前边界：单账户、单 venue、单 quote currency、long-only、固定 tick；不支持跨币种融资或外部仓位归属。
动态预算是减配/恢复到初始上限，不实施跨股票主动现金转账或自动扩大赢家预算。
原单股票 Traditional Grid、WFA/敏感性/Monte Carlo 入口保留；本次多股票 runner 比较 Dynamic 与等权 Buy & Hold。
组合级 Traditional Grid 对照和研究入口尚未扩展，不把原单股票结果冒充多股票组合的 OOS 稳健性结论。
组合级滚动优化与连续带仓再优化尚未验收，下一阶段应固定组合/参数后进行独立 OOS 与压力测试。
Bar OHLC 路径、历史公司行动、股息、最低收费与实际券商费用仍受原说明中的限制。
真实 Paper 成交、重启后远端恢复、断网/限流/费用对账需要指定账户与预算再验收。
