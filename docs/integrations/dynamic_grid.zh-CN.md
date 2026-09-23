# Stock Adaptive Dynamic Grid（SADG）：实现与验收说明

本策略在保留 Legacy DGT 动态重建基准的同时，增加适用于美股的 SADG 模式。SADG 使用常规交易时段、
跳空/流动性过滤、ATR 总宽度、突破确认、Core + Grid 目标仓位和共享组合风控；未引入 Fibonacci。
最新配置、共享风险和 version 6 恢复说明见
[Multi-Asset Dynamic Grid](dynamic_grid_portfolio.zh-CN.md)。本文后面的旧单股票结果仅保留为历史基线。

## 1. 交付边界

本实现是 NautilusTrader 0.63.0 / Rust 1.98.0 的原生 Rust Strategy，不是独立撮合框架。
同一策略可接 BacktestEngine、SandboxExecutionClient 和 Longbridge Paper/Live execution client。
开发过程没有发送券商订单、没有提交或推送 Git，也没有修改已有 Longbridge 执行逻辑。

代码与离线测试不等于实盘认证。远端 Paper 的真实成交、断线重连、跨进程券商恢复和费用对账仍需账户验收。
默认命令仅验证配置；实盘必须同时设置 mode=Live 和传入 --run --live。
本次交付不宣称策略已证明盈利，也不建议跳过 Paper 直接部署实盘。

## 2. 文件清单

新增：

- `crates/trading/src/examples/strategies/dynamic_grid/{mod,config,engine,regime,risk,orders,analytics,strategy,tests}.rs`
- `crates/trading/tests/dynamic_grid.rs`
- `crates/backtest/src/dynamic_grid/{mod,research}.rs`
- `crates/backtest/bin/dynamic_grid.rs`
- `crates/backtest/examples/dynamic_grid.json`：AAPL 2025 Q1 回测
- `crates/backtest/examples/dynamic_grid_full_history.json`：使用输入文件全部历史
- `crates/backtest/examples/dynamic_grid_walk_forward.json`：252/63/63 个交易日
- `crates/backtest/examples/dynamic_grid_walk_forward_smoke.json`：10/5/5 日流程验证
- `crates/adapters/longbridge/bin/dynamic_grid.rs`
- `crates/adapters/longbridge/examples/dynamic_grid_{paper,sandbox}.json`
- 本文档。

修改：三个相关 crate 的 Cargo.toml、Cargo.lock、trading strategies/mod.rs、backtest src/lib.rs。
这些文件原来已有其他未提交修改；本次保留它们，不把原有 momentum / intraday / SLC 改动归入本任务。
`reports/dynamic-grid-*.json` 是本地运行产物，不是策略源代码。

## 3. 架构和当前 API

```text
已完成 Bar -> Session/Gap/Liquidity -> RegimeDetector + ATR/波动率 -> GridEngine
Tick/Bar -> TargetPosition(Core + Grid) -> OrderManager + RiskManager
                                 -> 原生 Strategy Order API
                                    -> Nautilus Risk / Execution / Account / Position
                                       -> Backtest / Sandbox / Longbridge
成交事件 -> Decimal 账本 -> PerformanceTracker + durable checkpoint
```

采用当前仓库的 StrategyCore、DataActor、nautilus_strategy!、cache/order/portfolio facade。
参考已有 grid_mm、engine_ema_cross、intraday_momentum；没有假设 Python 版 API 可以直接移植。
使用已有 ATR、DirectionalMovement、Wilder MA、SMA、Bollinger 指标；不重复建立指标或订单框架。

状态：Initializing → Recovering → WaitingForRange → GridActive。
股票适配路径可进入 BreakoutPending / Paused；重建时
GridActive → BreakoutPending → GridResetting → GridActive/WaitingForRange。
风险时进入 RiskOff，Flatten 策略先进入 RiskReducing；未知/不一致状态进入 Recovering。
停止只撤销本策略订单，不自动卖出全部账户资产。

价格、数量、资金、费用、成本和 PnL 使用 Decimal 或原生 Price/Quantity/Money。
连续指标和统计比率使用 f64。交易逻辑没有 backtest/paper/live 分支；runner 负责执行层选择。

## 4. Grid 算法

采用论文的统一几何比率：`p(i)=center*(1+spacing)^i`，i 可正可负。
例如 100、2% 的下层原始价格是 98.0392、96.1169，而不是每次乘 0.98。
买价向下、卖价向上取整到 tick；数量向下取整到 lot。相邻层坍缩、溢出或非法输入拒绝启动。

默认每侧 10 层，先挂下方限价买单；穿越由原生撮合/券商成交确认，而不是把 crossing 当成交。
某层实际买入后，为已成交且未被其他卖单预留的数量提交上一层限价卖单。
全部买入数量卖出且入口订单已终结后记录一个完整 cycle，之后才能重新使用该 pair。
部分成交可分次挂出 covered exits；无库存不卖出，整个实现为 long-only。

默认 initial_inventory_fraction=0：不为了上方 SELL 网格主动买入初始库存。
设置为 (0,1) 可用部分网格预算在当前价格建立上方层库存；随后 SELL → 等待原层 BUY → SELL。
这不是裸卖空网格，不支持已有外部持仓自动归属到策略。

间距：

- Percentage：spacing_pct。
- LegacyDgt 的 Atr：相邻层间距为 ATR/current_price*atr_multiplier。
- StockAdaptive 的 Atr：先计算总宽度 ATR*atr_multiplier，再除以 grid_levels 得到相邻层间距。
- WiderGrid：在允许重建时增加趋势间距。
- 所有模式受 min/max spacing 和成本下界约束。

设单边保守成本 c=max(maker_fee,taker_fee)+commission+slippage，
则最低间距为 `(2*c+minimum_profit_margin)/(1-c)`。
若最大允许间距仍不能覆盖成本，拒绝配置，而不是无利润空间地继续挂单。

## 5. 市场状态

仅用已完成且严格递增的 OHLC：

- ATR：Wilder 平滑；ADX：DirectionalMovement 的 DX 再做 Wilder 平滑。
- Bollinger width=(upper-lower)/middle。
- MA slope：slope_period 内 SMA 的每 Bar 相对变化。
- Realized volatility：已完成 Bar 对数收益率的标准差，未年化。
- Warm-up 未完成或状态不明确：Disabled。
- 低 ADX、低斜率、收盘位于布林区间：Range。
- 高 ADX、正/负斜率：TrendUp / TrendDown。
- ATR/price、带宽或 realized volatility 过高：HighVolatility。

阈值均在 GridConfig。指标使用有界历史重放，重启前后严格一致；不是全历史无限精度 Wilder 序列。
这是按 Ponytail 技能选择的已有指标复用方式，代价是每 Bar O(window) 计算。
Longbridge 只使用 adapter 已有的 confirmed BarWithVwap；普通实时未完成 Bar 不驱动信号。
Tick 使用最近已完成 Bar 的 regime，并拒绝未来或过期事件。
零价、交叉、零数量报价被忽略；比已处理 Tick 更早的事件不会倒退当前价格。

TrendPolicy 的 Disable、ReduceGrid、WiderGrid、LongOnly、Continue 全部可配置。
默认 TrendUp=ReduceGrid，减少外围退出；TrendDown=Disable，停止新增买入但保留退出。
限制同时应用于已有挂单与新意图，不仅约束未来创建的订单。风险减仓优先于趋势限制。

## 6. Reset、资金与风险

重建条件：首次可交易、上下突破、regime 改变、波动率明显改变、人工审计后的 risk reset。
非首次自动重建要求同时满足 minimum_reset_distance 和 minimum_reset_interval_secs，不随每根 Bar 移动中心。
人工审计后的 reset_risk() 是显式恢复操作，不受自动重建节流限制。

重建顺序：

1. 持久化 reset 意图。
2. 请求撤销旧订单。
3. 保留全部未确认订单预留，处理晚到成交。
4. 等待全部旧订单终结。
5. 保留旧库存、原成本与退出价，更新现金/已实现利润。
6. 以当前可见价格建立新中心，按当前可用预算重新计算间距和数量。

上涨重建也不会把未卖出的库存当作已回收现金。
下跌重建不会重置亏损、注入新资本或自动倍增加仓。
wallet 对应 cash + inventory 成本账本；input capital 是固定初始预算，利润通过现金/权益自然影响后续分配。
这保留论文的突破后继续运行思想，但不机械复制其无限再投资规则。

Equal / Progressive / Inverse 是每层资金权重，不是固定股数。
总预算再受 max_position、max_position_pct、max_notional、max_grid_exposure、max_asset_ratio、
max_capital_utilization、reserve_capital 和实际账户 free cash 共同限制。
所有未决买单都占用数量/资金预留；限价单按限价上界，市价单按保守价格与成本预留。

RiskManager 限制 MDD、每日亏损、浮亏、连续 reset、订单数、层数与波动率。
日损失使用 UTC 日边界，保留隔夜跳空损失。风险锁跨重启保留，不在第二天自动解锁。
Hold：撤 BUY，允许有库存的退出；Flatten：撤单确认后才提交库存覆盖的市价退出。
卖单被拒绝、成交撤销、外部修改库存、订单身份未知时 fail closed，避免自动无限重发。
reset_risk() 是显式运维接口，只在订单全终结、对账通过且当前风险允许时解锁；CLI 不自动调用。

风险限额是下单约束，不是成交价或账户最大损失保证。跳空、流动性消失、断网及券商拒单仍可能突破损失预算。

## 7. 订单生命周期和恢复

身份格式：`DG-{order_id_tag}-{grid_id}-{signed_level}-{B/S}-{sequence}`。
sequence 与 grid_id 持久化；Longbridge 已有 client_request_id/remark 映射复用该身份。
Submitted != Filled；部分成交来自当前 API 的 OrderFilled.last_qty，没有虚构 OrderPartiallyFilled 回调。

账本处理 Intent、Submitted、Accepted、PartiallyFilled、CancelPending、Unknown、Filled、Cancelled、Rejected。
(order_id,trade_id) 去重；重复确认不能倒退终态；撤单请求不能提前释放资金/库存。
每个 cycle 输出 entry_price、exit_price、quantity、gross_pnl、execution_pnl、fees、slippage、net_pnl、holding_ns。

checkpoint version 6 使用独占文件锁、临时文件写入、fsync、原子 rename、目录 fsync。
每个完成 Bar 保存指标/风险状态，新的权益高水位和日边界也立即保存，避免重启遗忘已观察到的风险基准。
提交前先保存意图和原生初始化订单；账户/原生持仓/订单依然由 Nautilus 管理。
runner 在 broker reconciliation 前恢复原生 cache，启动后重放缺失的已知成交并验证 filled_qty、库存和归属。
checkpoint 配置还绑定 runner 的环境/账户/交易者身份，Paper 状态不能直接搬到 Live 使用。
如果提交结果未知，没有换新 ID 重发的路径。没有 checkpoint 但券商存在策略外活动订单/持仓时拒绝交易。

文件损坏、配置改变、未知成交历史或券商与账本仍不一致时，需要人工对账，不能自动猜测修复。
加载时也校验订单/库存/成交计数/周期的结构性一致性，不能仅凭 JSON 可解析就恢复交易。
Nautilus 在组件停止后不再分发策略回调；runner 排空停机事件后调用 finalize_after_stop()，从原生 cache 重放晚到成交，再保存最终账本和报告。
同一 instrument 必须由本策略独占；不同进程不能仅使用不同 state_path 去同时交易同一账户/标的。
生产应额外实施部署层单实例约束和独立账户监控。

## 8. 执行成本和统计口径

回测使用原生 maker/taker fee + commission、OneTickSlippageFillModel、真实订单/成交/现金/持仓。
滑点参数分为：策略提交前的比例成本预估，以及回测撮合的一 tick 滑点概率；二者不是同一个执行模型。
本地 Sandbox 复用 PerContractFeeModel，必须显式给出 sandbox_fee_per_share；
该费用是模拟假设，不是券商报价，需按价格区间校准策略的成本下界。

当前 Longbridge 成交事件无法提供完整实际费用时，按配置的保守费率估计，报告 estimated_fee_fills。
因此远端报告不能代替券商交割单；平台费、最低佣金、税费、币种转换尚未逐项对账。

`gross_pnl` 定义为已实现成交的意图价格毛收益，`slippage` 是带符号 execution shortfall；
另输出非负 `slippage_cost` 和 `price_improvement`，避免把价格改善误称为负成本。
已实现账本满足 `realized=gross-fees-slippage`；总 `net_pnl` 还包含期末按市值计价的未实现 PnL。

报告包括用户要求的 Return、CAGR/annualized return、Sharpe、Sortino、MDD、Calmar、win rate、
profit factor、交易/网格周期数、平均持有期、费用、滑点、资金利用率、最大敞口/持仓/reset、
drawdown duration、grid efficiency/turnover/capture、方向及趋势敞口。
未定义比率为 null，不伪造无限 Sharpe。日收益统计采用 252 个交易日、零无风险利率。
Capital efficiency 定义为总收益率/时间加权资金利用率；win_rate 即完整 grid cycle 的胜率，不把每笔买入计为输赢。
Tick 极值更新 MDD/最大敞口，不把每个 Tick 全部保存在权益曲线中；曲线与时间加权指标按 Bar 采样。
无外部入提款，因此 CAGR 等价于这一资金流假设下的年化资金收益；不提供多次入金的 XIRR。

## 9. 回测、Walk-forward 与 Monte Carlo

Bar CSV 支持两种表头，timestamp_ns 必须是收盘时间：

```text
timestamp_ns,open,high,low,close,volume
timestamp_ns,session_open,session_close,open,high,low,close,volume,vwap
```

可选 Quote CSV：

```text
timestamp_ns,bid,ask,bid_size,ask_size
```

不从 OHLC 伪造真实 Tick。Bar 回测遵循 Nautilus 的 OHLC 路径假设；实盘部署前必须使用真实 Tick 检查。
Buy & Hold 使用全额资金；Fixed Grid 使用相同风控预算、固定初始中心/Percentage 间距，越界后清算停止；
Dynamic Grid 使用动态重建。三者不是等敞口组合，必须结合利用率和方向敞口解释差异。
固定网格的边界不使用未来最高/最低价。

每个 WFA fold：训练候选排序 → top-k 验证 → 冻结参数 → 独立 OOS。
所有 GridConfig 参数可通过候选配置变化，包括 sizing、ATR、ADX、slope、资金和趋势策略。
测试窗口不重叠，每段独立初始资金与 warm-up；这不是连续带仓滚动再优化，也不是把拼接结果当作可执行组合净值。
默认 252/63/63 是近似 12/3/3 月的“观测交易日数”，不是日历月。
10/5/5 日配置只验证流程，不能用于宣布有效。

敏感性矩阵仅使用第一个训练段：0.5%..3% × 5/10/15/20/30 层。
输出 Return/MDD/Sharpe 等全量指标；明显优于所有相邻点时标记 potential_overfitting。
无 cliff 标记不代表没有过拟合，尤其在大多数参数不交易时。

Monte Carlo 固定 seed：

- Trade Shuffle：完整 cycle 的现金收益打乱；不包含未售库存。
- Return Shuffle：OOS 每日权益收益打乱；改变路径 MDD，但数学上的期末复合收益不变。
- Return Bootstrap：有放回重采样，产生期末收益分布。
- 输出 MDD/Return 的 5/50/95 分位、亏损概率、触碰 ruin_equity_fraction 的路径比例。

打乱破坏序列相关性，bootstrap 不是市场预测；少量交易或大量空仓时不能据此推断稳定性。

## 10. 参数入口

`grid: {}` 使用保守默认值；未知字段拒绝解析；Decimal 建议使用字符串。

| 参数 | 默认 |
| --- | --- |
| grid_levels / position_sizing | 10 每侧 / Equal |
| spacing_mode / atr_period / atr_multiplier | Atr / 14 / 0.75 |
| strategy_mode / core_target_pct / grid_max_pct | LegacyDgt / 0.40 / 0.60 |
| min_spacing_pct / max_spacing_pct | 0.005 / 0.03 |
| capital / capital_allocation | 100000 / 0.20 |
| max_position / max_position_pct | 1000 股 / 0.20 |
| max_notional / max_grid_exposure | 20000 / 20000 |
| max_asset_ratio / max_capital_utilization / reserve_capital | 0.20 / 0.50 / 0.50 |
| max_drawdown / max_daily_loss / max_unrealized_loss | 0.10 / 0.03 / 0.08 |
| max_consecutive_resets / max_orders / max_grid_levels | 5 / 40 / 30 |
| minimum_reset_distance / minimum_reset_interval_secs | 0.01 / 300 |
| breakout_confirmation_bars / regime_confirmation_bars | 2 / 3 |
| adx_range_max / adx_trend_min | 20 / 25 |
| trend_up_policy / trend_down_policy | ReduceGrid / Disable |
| enable_dynamic_reset / enable_trend_filter / enable_volatility_filter | true / true / true |
| initial_inventory_fraction / risk_policy | 0 / Hold |
| regular_session_only / max_gap_pct / gap_recovery_bars | true / 0.08 / 5 |
| maker_fee / taker_fee / commission / slippage | 0.0008 / 0.001 / 0 / 0.0005 |
| minimum_profit_margin / order_timeout_secs / max_signal_age_secs | 0.0005 / 30 / 180 |

其余指标阈值、周期与验证规则见 config.rs。默认费率不是当前 Longbridge 费率的承诺。
默认 USD、AAPL 整股和固定 tick；用户应核对标的、交易时段、账户币种、预算与真实费表。

## 11. 可复现命令

在仓库根目录运行。测试使用 dev profile，避免本机很小的可用磁盘被 test debug symbols 占满。
核心实现不依赖 Python；以下全部是 Rust 命令。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples --lib dynamic_grid --profile dev -j 2 -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples --test dynamic_grid --profile dev -j 2 -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples --lib grid_mm --profile dev -j 2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo build --release -p nautilus-backtest -p nautilus-longbridge --no-default-features --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid --bin dynamic-grid-backtest --bin longbridge-dynamic-grid -j 2
```

历史比较与短窗口流程验证：

```bash
mkdir -p reports
target/release/dynamic-grid-backtest crates/backtest/examples/dynamic_grid.json test_data/local/intraday_momentum/common_stocks/AAPL/bars.csv reports/dynamic-grid-aapl-2025-q1.json
target/release/dynamic-grid-backtest crates/backtest/examples/dynamic_grid.json test_data/local/intraday_momentum/common_stocks/AAPL/bars.csv reports/dynamic-grid-aapl-2025-q1-walk-forward.json --walk-forward crates/backtest/examples/dynamic_grid_walk_forward_smoke.json
```

使用更多历史的 252/63/63 WFA（本地数据目录未随代码分发；需自行准备合法数据）：

```bash
target/release/dynamic-grid-backtest crates/backtest/examples/dynamic_grid_full_history.json test_data/local/intraday_momentum/common_stocks/AAPL/bars.csv reports/dynamic-grid-full-walk-forward.json --walk-forward crates/backtest/examples/dynamic_grid_walk_forward.json
```

Tick 回放：在普通回测命令最后追加实际 quotes.csv 路径。

Longbridge 配置只读验证，以及主动运行的 Paper/Sandbox 命令：

```bash
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_paper.json
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_sandbox.json
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_sandbox.json --run
target/release/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_paper.json --run
```

Sandbox 使用真实行情、当地虚拟账户；Paper 使用 Longbridge papertrading endpoint，两者不同。
按现有 Longbridge adapter 文档配置鉴权，不把 token 写入策略 JSON 或 Git。
实盘需另建配置改为 mode=Live、使用独立 state/report 文件，并显式添加 --run --live。
本地 Sandbox 的账户每次重建，因此禁止使用旧 checkpoint 假装恢复同一个模拟 broker。
策略需等待实时已完成 Bar warm-up；没有偷偷下载或使用未来历史来启动。

Paper 验收顺序：核对账户/资金/标的与费用 → 空账户、小预算运行 → 核对成交与券商订单 ID →
验证部分成交和撤单 → 停机保留库存后同一配置重启 → 演练断线/未知提交/拒单 → 对账后再考虑 Live。
验收应证明没有重复提交、没有超售、未知订单不释放预算、重启库存一致且费用能与券商流水解释。
不要通过删除 checkpoint、换新 ID 或自动解锁 RiskOff 来绕过恢复失败。

## 12. 分阶段验证与实际结果

当前 SADG 回归：dynamic_grid 单元测试 85/85 通过，原生集成测试 75/75 通过；另有 4 项显式忽略的
性能回归检查已单独运行通过。SADG 相关 crate 严格 Clippy 通过，backtest 目标在 `--no-deps -D warnings`
下通过；全 workspace/all-targets 仍被未改动的 `momentum_pullback` 既有 lint 阻挡。
最新 AAPL+MSFT 组合比较与负向研究结论见组合文档。

以下阶段计数和 AAPL 表格是早期 Legacy DGT 交付记录，不代表当前 SADG 配置：

阶段 1 检查仓库/API/Longbridge 和基线（旧 grid_mm 25 tests）；阶段 2 形成上述架构。
阶段 3–6 逐步构建并测试 grid、regime、risk、orders；阶段 7 接入原生 Strategy；
阶段 8 构建 Longbridge runner；阶段 9 原生 Bar/Quote 回测；
阶段 10 WFA/敏感性/Monte Carlo；阶段 11 恢复、执行与前视偏差集成测试；
阶段 12 本地 Sandbox 成交已验证，远端 Paper 账户验收未执行。

最终 Rust 回归：trading crate 全部 585 项单元测试通过，其中 dynamic_grid 28 项；dynamic_grid 集成测试 10 项通过。
覆盖几何取整、crossing、三种 sizing、部分成交、cycle、重建守卫、风险预算与预留、隔夜损失、趋势/波动过滤、
非空持仓 checkpoint、重复实例、损坏状态、原生 Bar/Quote 成交、费用、Sandbox、Longbridge 未连接保护、
更改未来行情不改变历史决策、更改 OOS 不改变参数选择、WFA 分区和 Monte Carlo 可复现性。
严格 Clippy 受已有 momentum_pullback 的 12 项 lint 阻挡；没有修改无关代码。
另以 --cap-lints warn 完整检查两个 runner 和其依赖，dynamic_grid 新增文件没有 lint 诊断；
不把该诊断运行等同于严格 Clippy 全绿。
本次新增文件通过 scoped rustfmt；6 份 JSON 和 3 份 Cargo manifest 解析检查通过。
本地 AAPL 输入 CSV 的 SHA-256：`a0162073ace30b4b8e9ff936f440d4207bf40890154753df5d69599872415bb9`。
两个 runner 的最终源码已通过仓库原始 release profile（opt-level=3、fat LTO、codegen-units=1）构建，
下列回测和 WFA 报告已使用最终 release 二进制重新生成。
Paper/Sandbox 配置只读验证均已通过；错误的 Paper --run --live 组合在鉴权前被拒绝，未启动联网交易。
未执行全仓 make format / make pre-commit / make pre-flight；本次没有开 PR，且工作区有大量用户原有修改。
提交 PR 前仍应按仓库要求运行这些全量检查。

### AAPL 历史结果

2025-01-02 至 2025-03-31，60 个完整交易日、23400 根 1 分钟 Bar，初始 USD 100000；seed=42。
文件：reports/dynamic-grid-aapl-2025-q1.json，包含完整权益曲线、cycle 和执行假设。

| 指标 | Buy & Hold | Traditional Fixed | Dynamic |
| --- | ---: | ---: | ---: |
| Total Return | -10.63218% | -0.69796% | +0.04582% |
| CAGR | -37.20880% | -2.85793% | +0.18982% |
| MDD | 16.45218% | 0.74293% | 0.39392% |
| Sharpe | -1.5073 | -2.2954 | 0.2834 |
| Sortino | -1.8941 | -2.4246 | 0.4236 |
| Fees (USD) | 99.78 | 44.52 | 9.38 |
| Signed slippage (USD) | 8.04 | -29.28 | -25.92 |
| Net PnL (USD) | -10632.18 | -697.96 | 45.82 |
| 平均资金利用率 | 99.8664% | 3.9749% | 1.9327% |
| Capital efficiency | -10.6464% | -17.5592% | 2.3708% |
| 最大库存敞口 (USD) | 100399.50 | 14307.84 | 5813.52 |
| 最大股数 | 402 | 64 | 24 |
| 交易执行次数 / 完整 cycles | 1 / 0 | 28 / 14 | 6 / 3 |
| 最大回撤持续时间 (秒) | 4331820 | 7187460 | 2007060 |

动态策略最终触发 Maximum consecutive resets；不是持续整个季度正常开仓。
三个 cycles 的胜率 100%、平均净收益 USD 15.27，不足以评价胜率或稳定性。
本次不支持“DGT 已证明跑赢市场”的结论：资金使用率悬殊，动态策略主要长期空仓。
USD 45.82 净收益中含 USD 25.92 的模型价格改善；必须在真实 Tick/券商执行上验证，不能把它全部当作网格 alpha。

### 短窗口 WFA 与稳健性结果

文件：reports/dynamic-grid-aapl-2025-q1-walk-forward.json。10/5/5 日划分得到 9 个独立 OOS 窗口，
共 8 个完整 cycles；30 个敏感性组合仅 5 个产生交易，0 个触发 cliff 标记。样本稀疏，不能据此排除过拟合。
独立 OOS 日收益序列的几何拼接约 -0.04922%，不是连续带仓策略的可执行净值。

1000 次、seed=42、ruin 阈值为初始权益的 50%：

| 方法 | MDD 5% / 50% / 95% | Return 5% / 50% / 95% | 亏损比例 | ruin 比例 |
| --- | --- | --- | ---: | ---: |
| Trade Shuffle | 0 / 0 / 0 | 0.08188% / 0.08188% / 0.08188% | 0% | 0% |
| Return Shuffle | 0.08337% / 0.12717% / 0.17928% | -0.04922% / -0.04922% / -0.04922% | 100% | 0% |
| Return Bootstrap | 0.04676% / 0.11836% / 0.27929% | -0.25608% / -0.03959% / 0.14533% | 64.6% | 0% |

Trade Shuffle 的乐观结果遗漏了未售库存，而 marked-return 方法包含它，二者反差是重要风险提示。
0% ruin 只是低敞口、稀疏样本在这个阈值下的模拟结果，不是未来破产概率为零。
252/63/63 的多年度配置和运行命令已提供，但本轮没有完成该长窗口研究，也没有做参数有效性声明。

## 13. 已知限制与下一阶段

1. 现支持单账户多标的组合，仍为 long-only、现货/股票、固定 tick；不支持期货保证金、多币种转换、港股可变 tick 或外部持仓归属。
2. Paper/live 线路已接入，但尚未做真实券商成交、限流、超时后 broker reconciliation 的全链路账户验收。
3. 无可靠 broker 状态时停在 Recovering，不能承诺任何断网/掉电场景都可无人值守继续。
   已经完成结账的 cycle 又收到新的非重复晚到成交时，也要求审计，不盲目改写已结账库存。
4. Longbridge 费用缺失用估计值；需补充交割单与现金流水对账后，才可评价真实净收益。
5. Bar 回测无法识别真实队列位置与逐笔路径；市场冲击、大单容量、最低费用、公司行动/股息暂未建模。
6. 本地 AAPL 数据为 Alpaca raw 价格，源数据仅保留完整 390 分钟交易日；短交易日排除带来样本选择限制。
7. 账本与权益曲线保存在进程/完整 checkpoint，长期高成交量应加归档；每次 checkpoint fsync 的延迟必须实测。
   当前保留完整审计信息和每 Bar 有界指标重放，是 Ponytail 下优先正确恢复、避免另建存储框架的选择。
8. 先完成远端 Paper、故障注入与公司行动检查，再研究更多股票、多种市场状态及真实 Tick。
   在足够 OOS 交易数、等敞口基准和稳定参数邻域出现前，不以低回撤或短期盈利宣布策略有效。

## 参考资料

[TradersPost Grid Trading Strategy Guide](https://blog.traderspost.io/article/grid-trading-strategy-guide)：
区间识别、资金预留、趋势过滤、执行成本、监控恢复及稳健性验证。

用户提供的 Chen 等论文 *Dynamic Grid Trading Strategy: From Zero Expectation to Market Outperformance*，
arXiv:2506.11921v1，已完整阅读。其 BTC/ETH 历史 IRR 含显著方向性上涨贡献，不能全部归因于网格套利。
本实现只迁移边界突破后动态重建和库存/钱包管理思想，没有使用全样本极值定网格或复制论文代码。
