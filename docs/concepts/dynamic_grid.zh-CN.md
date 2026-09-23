# Dynamic Grid 策略代码与设计讲解

本文面向希望读懂、研究和维护当前 Rust 动态网格策略的开发者。
阅读顺序是：先理解一笔买卖如何赚钱，再理解代码如何管理库存，最后理解组合风控和真实执行。
示例价格用于讲解计算，不是回测结果或参数推荐。

本文核对的源码基线为 `6042c60fb5`，NautilusTrader workspace 版本为 `0.63.0`。
核心源码目录是 `crates/trading/src/examples/strategies/dynamic_grid/`。
下文用“文件名 + 函数名”定位实现，避免行号随代码变动失效。
部署和历史验收记录分别见[策略集成说明](../integrations/dynamic_grid.zh-CN.md)和
[组合集成说明](../integrations/dynamic_grid_portfolio.zh-CN.md)。

按问题快速阅读：

- 先学网格：[获利机制](#1-先理解策略靠什么获得收益)、[价格和数量计算](#5-网格价格怎样算出来)、[一次完整交易](#6-从挂买单到完成一个周期)。
- 跟踪代码：[行情处理](#4-一根行情到底经过哪些函数)、[目标仓位](#7-core--grid-的目标仓位模型)、[动态重置](#9-动态重置怎样避免变成不停追价)。
- 理解约束：[Gap](#10-gap交易时段和流动性)、[共享资金](#11-多股票怎样共享一笔钱)、[过滤和硬风控](#12-哪些是过滤哪些是真正的硬风控)。
- 对照实际：[当前配置](#15-json-到运行参数三个不同的资金层次)、[报告口径](#16-怎样正确阅读收益报告)、[源码练习](#18-推荐的源码阅读路线与练习)。

## 1. 先理解策略靠什么获得收益

网格把一个价格区间拆成若干相邻买卖对。价格回落时买入，反弹至更高一层时卖出，
扣除费用后形成一个完成周期。它利用的是实际发生的往返波动。

例如在 98.03 买入 10 股，随后在 100.00 卖出 10 股：

```text
实际买卖价差收益 = (100.00 - 98.03) × 10 = 19.70
周期净收益       = 19.70 - 买入费用 - 卖出费用
```

价格也可能从 98.03 一直跌至 90。此时没有完成这个盈利周期，但账户已经承担库存浮亏。
所以必须同时看“已完成周期赚了多少”和“尚未卖出的股票亏了多少”。
完成周期胜率很高，可以与组合整体亏损同时发生。

Dynamic Grid 的核心变化是：价格有效离开旧区间后，撤销旧挂单并在新的价格附近建立新网格。
重置让网格继续适应当前价格，但不会消除旧库存的成本、损失或风险，也不会自动带来额外资金。

### 两种实现模式

[config.rs::StrategyMode][config] 定义两种模式，它们共用网格、订单账本和执行链路。

| 模式            | 主要行为                                               | 适合怎样理解                                |
| --------------- | ------------------------------------------------------ | ------------------------------------------- |
| `LegacyDgt`     | 保留原有库存模型及动态重置。                           | 作为原始 DGT 工程基准，仍包含本仓库的风控。 |
| `StockAdaptive` | 增加 Core/Grid 目标、股票时段、Gap、流动性和突破确认。 | 当前股票适配模式，简称 SADG。               |

`DynamicGridStrategy` 在 [mod.rs][module] 中实际是 `MultiAssetGridStrategy` 的别名。
不是单股票和多股票各维护一份策略实现。
当前代码没有 Fibonacci 网格；网格价格采用几何分布。

## 2. 读代码前需要的基础概念

| 概念            | 当前实现中的含义                                                |
| --------------- | --------------------------------------------------------------- |
| Bar             | 一根已经完成的 OHLCV K 线，供指标更新使用。                     |
| Quote Tick      | 一次买卖报价，包含 bid/ask 及数量，可用于价差检查和 Tick 驱动。 |
| Trade Tick      | 一次市场成交观测；不是本策略自己的成交回报。                    |
| Tick size       | 最小价格单位，例如 0.01 美元；与 Tick 行情不是同一概念。        |
| Lot size        | 最小交易数量单位；当前股票示例为 1 股。                         |
| Inventory       | 已实际成交、仍未卖出的库存。                                    |
| Reservation     | 尚未终结订单预留的现金或库存。                                  |
| Grid generation | 一代网格的身份，重建后递增，用于区分旧订单和新订单。            |
| Grid cycle      | 一个入场批次完成买入、卖出和费用结算后的周期。                  |
| Regime          | 根据已完成行情计算出的市场分类。                                |
| Reconciliation  | 将本地状态与原生缓存、券商订单、成交和持仓逐项对账。            |

价格、股数、现金、费用、PnL 使用 `Decimal` 或 Nautilus 的 `Price`、`Quantity`、`Money`。
ATR、ADX、相关系数等连续指标使用 `f64`。
前者要求精确记账，后者用于统计估计，两者的职责不同。

## 3. 从文件结构认识职责边界

可以把策略分成三个问题：市场允许参与吗、网格希望怎样交易、账户允许交易多少。

```text
Nautilus 数据回调
    ↓ 按 InstrumentId 分发
单股票 GridStrategyEngine
    ├─ stock.rs：时段、Gap、流动性、突破确认
    ├─ regime.rs：ATR、ADX、均线、市场分类
    ├─ engine.rs：固定一代网格的价格几何
    ├─ position.rs：Core 目标与 Grid 库存上限
    └─ risk.rs：单股票资金、仓位和亏损约束
    ↓ 订单意图
MultiAssetGridStrategy + PortfolioRiskManager
    ↓ 共享账户资金准入
Nautilus Strategy → 原生 Risk / Execution → 模拟撮合或 Longbridge
    ↓ 订单与成交回报
orders.rs 账本 → 库存 / 周期 / analytics.rs 报告 / 检查点
```

| 文件                          | 主要对象或入口                                | 阅读时关注的问题                   |
| ----------------------------- | --------------------------------------------- | ---------------------------------- |
| [config.rs][config]           | `GridConfig`、`validate`、`cost_floor`        | 参数含义、默认值和非法组合。       |
| [engine.rs][engine]           | `GridEngine`、`build`、`spacing`、`can_reset` | 层级如何生成，何时允许换中心。     |
| [regime.rs][regime]           | `RegimeDetector::update`、`permits_order`     | 市场分类和方向限制。               |
| [stock.rs][stock]             | `StockMarketState`、`observe_bar`、`gate`     | 股票市场额外的入场条件。           |
| [position.rs][position]       | `target_position`、`position_delta`           | 想持有多少、已有多少、还差多少。   |
| [orders.rs][orders]           | `GridOrder`、`InventoryLot`、`OrderManager`   | 订单、库存和周期怎样关联。         |
| [strategy.rs][strategy]       | `GridStrategyEngine::drive`                   | 把各模块按真实执行顺序串起来。     |
| [multi_asset.rs][multi]       | `MultiAssetGridStrategy`、`dispatch`、`gate`  | 多股票事件分发与最后一次资金检查。 |
| [portfolio.rs][portfolio]     | `PortfolioRiskManager::capacity`              | 现金、行业、相关性和组合上限。     |
| [analytics.rs][analytics]     | `PerformanceTracker::finish`                  | 指标究竟统计什么。                 |
| [diagnostics.rs][diagnostics] | `GridDiagnostics`                             | 为什么没成交、为什么重置或停买。   |
| [files.rs][files]             | `load_portfolio_config`                       | JSON 如何加载为真正的运行配置。    |

策略自己的 `OrderManager` 负责网格批次归属、预留和收益核算。
Nautilus 继续负责原生订单状态、账户、持仓、风控和执行。
理解为“在原生交易基础设施之上记录策略业务语义”，比理解为另一套券商系统更准确。

## 4. 一根行情到底经过哪些函数

### 已完成 Bar 的处理

从 [multi_asset.rs::on_bar][multi] 开始，按标的找到独立引擎，再调用
[strategy.rs::completed_bar][strategy]：

1. 核对 `bar_type`；拒绝未来时间，忽略已处理或更早的信号 Bar。
1. 股票模式下检查常规交易时段；处理真实开盘价相对前一 session 收盘的 Gap。
1. 更新指标和 `RegimeSnapshot`，记录市场分类连续稳定的 Bar 数。
1. 更新组合相关性所需的收盘观测，以及当前网格的突破确认计数。
1. Bar 执行模式直接调用 `drive`；Tick 模式保存信号，随后由 Tick 调用 `drive`。
1. 递减 Gap 恢复计数，记录权益、间距和阻塞原因。

实时 Longbridge 路径使用已确认的 `BarWithVwap` 自定义数据，经过 `on_data` 进入同一个
`completed_bar`。Tick 不会让未完成 K 线提前进入 ATR 或 ADX。

### `drive` 的真实决策顺序

读 [strategy.rs::drive][strategy] 时，按下面顺序比从头记所有变量更有效：

1. 更新价格观测；`Initializing`、`Recovering`、`Stopped` 不产生新交易。
1. 对账库存、读取资金快照，更新单标的风险；处理订单未知或撤单超时。
1. 若单标的或组合触发硬风险，按对应策略处理撤单、保留止盈或减仓。
1. 撤销不再符合趋势方向策略的挂单。
1. 股票模式先更新目标仓位并处理超额库存，再检查时段、Gap 和流动性。
1. 检查信号预热、新鲜度和 regime 稳定性，得到 `can_buy`。
1. 检查突破确认和 reset 条件；重置时等待旧订单全部终结。
1. 不允许买入时撤买单、维护允许的卖单，然后等待。
1. 没有有效网格时创建新一代；股票模式按需要补 Core，再维护 Grid 出场。
1. 遍历空闲层级，经过数量和成本检查、单标的风险、组合风险，才创建并提交买单。

因此 `GridActive` 不代表每层都必然有订单，`can_buy` 也不代表已经获准提交。
某层可能数量为零、已有库存、预算不足，或者在最终账户检查中被缩量。

## 5. 网格价格怎样算出来

### 几何网格是一组买卖对

[engine.rs::GridEngine::build][engine] 使用统一比率 `r = 1 + spacing`：

```text
第 i 个下方买价 = center / r^i
对应卖价       = center / r^(i-1)
第 i 个上方买价 = center × r^(i-1)
对应卖价       = center × r^i
```

中心为 100、间距为 2%、最小价位 0.01 时：

| 层索引 | 买入限价 | 对应出场价 |
| ------ | -------: | ---------: |
| `-1`   |    98.03 |     100.00 |
| `-2`   |    96.11 |      98.04 |
| `+1`   |   100.00 |     102.00 |
| `+2`   |   102.00 |     104.04 |

买价向下、卖价向上按 tick 取整，所以 `-1` 的买价和 `-2` 的卖价可能相差一个 tick。
下层原始值是 `100 / 1.02 = 98.0392…`，并非 `100 × 0.98 = 98`。
`grid_levels = 3` 表示每侧三组，共六组相邻买卖对，不是全网格只有三条价线。

上下两侧的 `GridLevel.side` 都是买入库存的方向。
正层的存在不代表允许裸卖空；卖单必须有真实库存覆盖。

### 每层股数怎样分配

`engine.rs::weight` 先给每层资金权重，`build` 再把每层金额除以买价并向下取整：

```text
第 i 层金额 = 本代网格预算 × weight(i) / 所有层权重之和
计划股数    = floor_lot(第 i 层金额 × 该侧预算比例 / 买价)
```

`Equal` 每层资金相同，因此价格不同会导致股数不同；`Progressive` 的权重为 1、2、3……，
`Inverse` 则反向分配。它们在预先限定的总预算内分配，不是每亏一次就把订单翻倍。
`VolatilityAdjusted` 先按等资金建层，再在风险数量计算中乘以
`min(position_volatility_target / 当前ATR比例, 1)`，只会缩量，不会因低波动超过原计划放大。

如果每层金额不足以买到一整股，该层计划数量为零，`drive` 会跳过。
层数增加可能让网格更密，也可能让更多层因整股限制失去可交易数量。

### 正层库存和 Core 是两个东西

`initial_inventory_fraction` 将网格预算的一部分分给正层。
某代正层首次参与时，可以用市价买入准备后续止盈的库存，代码称为 seed。
同代同层有过买单历史后，不再重复首次 seed；后续按该层限价参与。

这些库存归属 `PositionComponent::Grid`，会被普通网格卖出。
`core_target_pct` 对应的 Core 库存则单独记账，不通过普通网格止盈卖出。

### ATR 怎样变成间距

ATR 衡量以价格单位表示的波动幅度。真实波幅包含当根高低差及相对前收盘的跳变，
当前实现复用 Nautilus 的 Wilder 平滑 ATR。

[engine.rs::spacing_components][engine] 中的原始间距为：

```text
Percentage：   raw = spacing_pct
LegacyDgt：    raw = ATR / price × atr_multiplier
StockAdaptive：raw = ATR / price × atr_multiplier / grid_levels

raw       = raw × 趋势间距乘数
effective = min(max(raw, min_spacing_pct, cost_floor), max_spacing_pct)
```

在股票模式中，`ATR × atr_multiplier` 是再分配到每侧层间距的宽度尺度。
最终仍由几何网格生成边界，不是精确的 `center ± ATR × multiplier` 线性区间。
clamp 和 tick 取整也会改变实际宽度。

例如价格 100、ATR 0.30、乘数 8、每侧 3 层，原始间距为 0.8%。
若下限为 3%，实际间距仍是 3%。继续把乘数从 8 改成 7，网格可能完全不变。
这解释了为什么“调整 ATR 参数”有时不影响回测。

一代网格创建后，中心和间距冻结。随后每根 Bar 算出的候选间距只用于判断是否需要重建，
不会立即移动已有挂单。诊断字段 `raw`、`effective`、`active` 分别记录原始、受限候选和当前冻结间距。

### 为什么要有交易成本下限

[config.rs::cost_floor][config] 计算：

```text
c = max(maker_fee, taker_fee) + commission + slippage
cost_floor = (2 × c + minimum_profit_margin) / (1 - c)
```

分母考虑卖出金额比买入金额更高，相应出场成本也更高。
用当前示例中的成本参数，`c = 0.0015`，最低理论间距约 0.3505%。
这是一道事前经济性门槛；它不预测下一次真实成交价格。
如果允许的最大间距都覆盖不了成本，配置校验失败。

## 6. 从挂买单到完成一个周期

以 `-1` 层计划买 10 股、买价 98.03、卖价 100 为例。

1. `drive` 确认该层没有活动订单或剩余库存，创建入场意图。
1. `submit` 再做组合检查，把意图转换成原生限价单，先持久化，再发送。
1. 券商接受订单后只改变状态，库存仍是零。
1. 如果只成交 4 股，`apply_fill → OrderManager::fill` 扣除这 4 股金额与费用，增加 4 股库存。
1. `exits` 可以为未被其他卖单预留的 4 股挂出 100 的卖单，不能卖计划中的全部 10 股。
1. 后续买单再成交 6 股，可以再为新增的未预留库存挂出场单。
1. 真实买入数量全部卖完，且入口订单已经终结，`complete_cycle` 才记录一个完整周期。
1. 同代同层不再被占用后，后续 `drive` 可以重新挂买单。

如果入口剩余 6 股最终撤销，已买的 4 股全部卖出后也能形成完整周期。
“完整”指该实际入场批次已结清，不要求最初计划的 10 股全部成交。

### 三层状态不要混在一起

| 状态层                   | 例子                                          | 解决的问题                     |
| ------------------------ | --------------------------------------------- | ------------------------------ |
| 策略状态 `StrategyState` | `GridActive`、`GridResetting`、`RiskOff`      | 当前引擎应该走哪条控制流程。   |
| 层级状态 `LevelStatus`   | `Pending`、`Active`、`Filled`、`Completed`    | 该层最近的生命周期记录。       |
| 订单状态 `OrderPhase`    | `PartiallyFilled`、`CancelPending`、`Unknown` | 订单是否仍可能改变现金和库存。 |

是否允许再次开仓，真正依据 `slot_busy` 和订单/库存账本。
不能仅凭 `GridLevel.status` 推断该层已没有资金占用。

### 穿越不是成交

`GridEngine::crossed` 检测一个价格变化跨过哪些层，并按向下从高到低、向上从低到高排序。
`drive` 当前用它输出 `GRID_LEVEL_CROSSED` 日志。
订单成交来自原生撮合或券商回报，不由这条日志产生。

如果价格从 100 跳到 90，不能根据跨过 98、96、94 就写入三笔成交。
真实存在的挂单由执行层决定是否成交、成交价格和数量；当时没有挂单的层不能补写历史成交。
当前循环还会跳过 `level.price >= 当前价` 的普通新买单，
因此放开 Gap 过滤也不会自动追补所有已跨过的买入层。

## 7. Core + Grid 的目标仓位模型

[position.rs::target_position][position] 先计算单股票分配金额，再按 regime 乘数生成目标：

```text
allocation = min(单股票权益 × grid.capital_allocation, max_notional)
core = allocation × core_target_pct × core_factor / price
grid = allocation × grid_max_pct × grid_factor / price
```

数量向下按 lot 取整，再受股数、金额、仓位比例等上限约束。
同一个总量上限下，计算先保留 Core，再削减 Grid。

这里的两个数字用途不同：Core 是按需补齐的持仓目标；Grid 是跨所有代次的库存上限，
不是立即市价买满的任务。实际 Grid 数量仍由各层限价成交逐步形成。

`position_delta` 把待成交订单也算进去：

```text
预计仓位 = 当前库存 + 待买数量 - 待卖数量
仓位差额 = 目标数量 - 预计仓位
```

例如目标 100、当前 60、待买 25、待卖 5，差额为 20。
但待卖数量尚未释放现金，最终买入仍必须经过共享现金预留检查。

### regime 怎样改变目标

假设分配金额 10,000、价格 100，使用代码默认的 40% Core、60% Grid 和默认趋势乘数：

| 市场状态    | Core 目标 | Grid 上限 | 合计   |
| ----------- | --------: | --------: | -----: |
| `Range`     |     40 股 |     60 股 | 100 股 |
| `TrendUp`   |     50 股 |     30 股 |  80 股 |
| `TrendDown` |     20 股 |      0 股 |  20 股 |

这只是解释公式的例子；当前仓库 AAPL/MSFT JSON 的 Core 为 1%、Grid 为 90%。
上升时普通网格止盈不会卖掉 Core，但目标减仓和其他风险流程仍可影响持仓。

`reduce_to_target` 先处理 Grid 组件，再处理 Core。
有冲突买单或限价卖单时先请求撤销，等确认后才提交覆盖库存的市价减仓单。
它允许以亏损价格降低风险，并不要求每笔交易都赚钱。

需要正视现状：`Disabled` 和 `LowVolatility` 在目标公式中仍使用与 `Range` 相同的乘数。
它们会阻止新增买入，但不会仅因这个分类就降低目标仓位。
这就是“停买以后还会继续承受浮亏”的一个代码原因。

## 8. 市场状态识别怎样工作

[regime.rs::RegimeDetector::update][regime] 只接受严格递增的完成 Bar。
ATR、方向运动、ADX、均线、布林带和收益波动窗口全部预热后，才进行有效分类。
当前实现保存有界历史，每次用这些完成观测重算指标；恢复时也重放这段历史验证快照。
这便于保持恢复一致性，但每次更新有与窗口长度相关的计算成本。

基础指标的作用是：

- ATR：估计每根 Bar 的价格波动尺度，用于间距、突破距离和仓位约束。
- ADX：估计方向性运动的强度，单独不能决定上涨还是下跌。
- 均线与斜率：通过 SMA 或 EMA 判断方向；斜率为窗口内每根 Bar 的相对变化。
- 价格相对均线偏离：可选的趋势确认条件，由 `require_price_ma_confirmation` 控制。
- 布林带宽度：上轨与下轨距离除以中轨，用于过度波动检查。
- Realized volatility：最近完成 Bar 对数收益的标准差，这里没有年化。

分类按优先级执行，前面的条件命中后不再继续：

| 顺序 | 条件概要                                             | 分类             |
| ---- | ---------------------------------------------------- | ---------------- |
| 1    | 指标没有全部预热。                                   | `Disabled`       |
| 2    | 波动过滤开启，ATR/价格、收益波动或布林带宽任一过高。 | `HighVolatility` |
| 3    | 波动过滤开启，ATR/价格过低。                         | `LowVolatility`  |
| 4    | 趋势过滤关闭。                                       | `Range`          |
| 5    | ADX 足够高、斜率为正且通过可选价格确认。             | `TrendUp`        |
| 6    | ADX 足够高、斜率为负且通过可选价格确认。             | `TrendDown`      |
| 7    | ADX 较低、斜率较小，收盘位于布林带内。               | `Range`          |
| 8    | 其余情况，例如方向强度落在两个阈值之间。             | `Disabled`       |

股票模式还要求分类连续稳定 `regime_confirmation_bars` 根后才允许新增买入。
`HighVolatility` 当前是缩小目标并禁止新增网格入场；代码并没有实现“高波动仍持续买入宽网格”的通用模式。

`TrendPolicy` 是分类之后的订单规则：`Disable` 停买、`Continue` 正常参与、
`WiderGrid` 在允许重建时加宽间距、`LongOnly` 在上涨时限制普通止盈卖出。
`ReduceGrid` 限制逆趋势方向的层数：上涨时限制卖出层，下跌时限制买入层。
这些规则同时用于现有挂单和新订单，不只是创建网格时检查一次。

## 9. 动态重置怎样避免变成不停追价

[engine.rs::can_reset][engine] 同时检查最短时间、相对旧中心的位移和 ATR 距离。
股票模式另在 [stock.rs::observe_breakout][stock] 中要求连续完成收盘价在边界外，
且越界距离达到 `minimum_reset_atr_multiple × ATR`。
回到边界内会重置连续确认计数。

```text
价格越界
    ↓ 连续收盘确认 + ATR 距离
有效突破
    ↓ 单标的风险、regime、时间和中心位移条件
记录 reset_reason，进入 GridResetting
    ↓ 撤销旧网格活动订单
等待全部终结，撤单失败/未知结果继续占用预留
    ↓ 有新鲜信号与可用重置预算
记录完成重置，清空旧几何；保留真实库存与账本
    ↓ 允许买入时，以当前价建立下一代网格
```

重置原因依次优先记录为：上破、下破、regime 变化、候选间距明显变化。
后两种区间内重置也受最短间隔和最小距离限制，不会每根 Bar 都搬动中心。
下破且处于 `TrendDown` 时，股票路径暂停买入，不机械构造更低网格继续补仓。

撤单对账结束不一定立即得到新网格；如果此时不允许买入，引擎继续等待。
旧批次的原始止盈目标保存在 `InventoryLot.target`，重置不会把它改成新网格目标。
遗留库存与新代库存一起占用风险额度。

`maximum_resets_per_day` 按 UTC 日计数；`max_consecutive_resets` 约束连续重置。
当前 `apply_fill` 发现新完成的盈利周期时，会把连续计数归零；每天重置额度与它独立。
重置只是更换交易价格计划，没有“重置亏损”或“自动注资”的含义。

## 10. Gap、交易时段和流动性

### 最新 Gap 逻辑

[stock.rs::observe_bar][stock] 在新 session 第一根可用完成 Bar 中，
用该 Bar 的真实 open 对比上一 session 最后观测的 close：

```text
change  = open - previous_close             # 带符号价格差
gap_pct = abs(open / previous_close - 1)    # 绝对百分比
gap_atr = abs(change) / prior_atr           # ATR 有效时
```

仅向下跳空，且超过任一启用的幅度阈值，才启动 `gap_recovery_bars` 的观察期。
对应阈值为零表示关闭该项比较。

| 情况                            | Gap 层行为            | 其余路径                                 |
| ------------------------------- | --------------------- | ---------------------------------------- |
| 向上跳空                        | 不启动新的 Gap 暂停。 | 已有库存按原有止盈、目标和趋势规则处理。 |
| 向下跳空，但未超过任一启用阈值  | 不启动新的 Gap 暂停。 | 可以继续参与符合条件的网格交易。         |
| 向下跳空，超过百分比或 ATR 阈值 | 暂停新增买入。        | 仍走已有库存退出和目标减仓流程。         |

“未超过百分比阈值”不等于“普通低开”。例如前收盘 100、前一根分钟 ATR 为 0.20，
低开 1 美元只有 1%，但已经达到 5 ATR；如果 ATR 阈值为 3，仍会暂停。
此处 ATR 的周期单位取决于信号 Bar，当前分钟配置不是日线 ATR。

Gap 放行只意味着这一道门没有拦截。`HighVolatility`、`Disabled`、趋势策略、
突破确认或组合额度仍可能阻止交易。代码没有读取新闻判断“利空是否严重”，
这里的“严重”仅指配置的价格幅度条件。
也没有专门的“低开立刻抄底单”；后续仍需原有网格价格、reset 和订单条件成立。

### 时段与成交时间

常规交易判定使用 `America/New_York` 时区，执行时段为 09:30 至 16:00 前。
为了接受收盘数据，完成 Bar 的时段检查包含 16:00，但这不开放 16:00 后新增买入。
当前按钟点判断，没有完整的假日、半日市日历。

Gap 识别发生在首根可用 Bar 完成后，不是在开盘前预测。
如果当日缺少真正的第一根 Bar，得到的也是第一根观测 open 与上一 close 的差值。
识别后提交的新订单不能倒填到已过去的开盘价；已有挂单的实际成交由执行层决定。

### 流动性检查

`gate` 依次检查时段、最低股价、Gap 恢复、滚动成交额、买卖价差。
平均成交额使用完成 Bar 的 `close × volume` 的滚动均值，
并不等于逐笔成交金额的精确总和或日均成交额。

spread 使用真实 Quote 的 `(ask - bid) / midpoint × 10000`，单位为 bps。
没有 Quote 时，价差字段为空，当前规则不会凭空估算价差，也不会仅因缺报价就拒绝交易。
因此只有 Bar 的回测不能检验真实 spread 过滤效果。

## 11. 多股票怎样共享一笔钱

`MultiAssetGridStrategy` 内部按 `InstrumentId` 保存独立 `GridStrategyEngine`。
每只股票有自己的指标、网格、批次、订单序列、局部现金核算、风险和报告。
组合级 `PortfolioRiskManager` 统一掌握共享现金和风险额度。

例如组合资金 100,000，两只股票各分配 30%：每只股票初始化局部核算资金为 30,000，
另外 40,000 保留在组合现金中。局部核算不代表两个可独立透支的真实账户。

组合按下面方式汇总，避免把初始资金重复相加：

```text
组合现金 = 初始组合资金 + 各股票局部现金相对初始分配的变化之和
组合权益 = 组合现金 + 全部已成交库存的当前市值
```

[portfolio.rs::capacity][portfolio] 把以下剩余额度取最小值，换算成股数，再按 lot 向下取整：

- 组合现金减去全部待买预留及最低现金保留。
- 券商可用资金减去待买预留。
- 单笔订单金额额度。
- 组合总暴露、行业暴露、相关簇暴露的剩余额度。
- 当前股票预算与最大仓位额度。

假设还有 10,000 美元可以买入，AAPL 和 MSFT 各希望买 8,000 美元。
AAPL 意图进入账本后先占预留，MSFT 的最终检查最多只会看到剩余 2,000 美元，
并进一步扣除成本、按整股取整。并发信号不会让每只股票重复花同一笔钱。

风险判断返回 `Allow`、`Reduce`、`Defer` 或 `Reject`。
临时额度不足通常是 `Defer`，不意味着经纪商拒单；风险已经锁定则可以是 `Reject`。
持仓股票的估值行情过期时，其他股票也会暂缓增仓，因为组合权益无法可靠估计。

### 行业、相关性和预算调整

行业来自配置 `sector`，缺失时统一归入 `Unknown`，不是忽略行业风险。
相关性使用对齐的已完成 UTC 日收益区间；不使用当日尚未结束的收益。
高相关股票构成连通簇：A 与 B、B 与 C 高相关时，可能把 A/B/C 一起限额。
历史不足无法计算时，代码以相关系数 1 处理，早期预算可能因此更受限制。

`reallocate` 根据 regime、波动和盈亏缩减或恢复新增仓位预算，并有最小时间与变更幅度限制。
它不自动把盈利股票的预算扩到初始分配以上，也不等于立即执行跨股票调仓。
当前没有基于 SPY/QQQ 的 rolling beta 目标引擎。

## 12. 哪些是过滤，哪些是真正的硬风控

| 机制         | 典型原因                               | 怎样恢复                         |
| ------------ | -------------------------------------- | -------------------------------- |
| 市场准入     | 预热、`Disabled`、低波动、信号过期。   | 后续新鲜完成行情满足条件。       |
| 股票临时门槛 | 时段、Gap 恢复、流动性。               | 时段或行情恢复，计数结束。       |
| 重置屏障     | 旧单尚未终结、信号尚不新鲜。           | 撤单对账及行情条件满足。         |
| 锁存风险     | 最大回撤、日亏损、超仓、未知订单结果。 | 显式风险重置并通过当前状态验证。 |

[risk.rs::RiskManager::observe][risk] 把实际库存和未终结买单一起纳入风险。
回撤基于权益高水位，日损失基于 UTC 日初权益；浮亏和隔夜损失参与这些判断。
锁存后 `risk_off_reason` 保留首个原因，不会因为下一根 Bar 恢复为 `Range` 自动清除。

`Hold` 保留库存及允许的止盈退出，并不承诺在亏损时快速去库存。
`Flatten` 的设计目标是先撤单、再平仓；但当前调用链还有一个需要区分的实现边界：
`drive → exits(flatten=true) → exit_candidates` 只选择 Grid 批次。
因此不能把现有 `Flatten` 路径理解为已经保证同时清空 Core 和 Grid。
Core 的常规减仓由 `reduce_to_target` 单独处理。这是阅读源码发现的行为边界，本教程没有修改它。

同理，暂停和停买并不等于持仓风险消失。理解退出政策，必须同时看目标仓位、
`risk_policy`、覆盖卖单价格以及实际剩余库存。

## 13. 订单生命周期、幂等性和撤单

[orders.rs::OrderPhase][orders] 的正常路径可以是：

```text
Intent → Submitted → Accepted → PartiallyFilled → Filled
                         ↓
                    CancelPending → Cancelled
                         ↓
                       Unknown → 对账

其他终态：Rejected、Expired
```

`Intent` 是已创建的业务意图，不是已经买到股票；`Accepted` 也不是成交。
`CancelPending` 和 `Unknown` 的剩余数量仍占现金或库存预留。
撤单失败回报会进入未知状态并触发组合保护，不能立即再挂一笔相同卖单。

订单身份实际采用 `DG-{namespace}-{grid_id}-{level}-B/S-{sequence}`。
namespace 包含策略和标的，序号随检查点保存；原生缓存还会拒绝重复的客户端订单身份。
这是稳定身份加本地去重与恢复对账，不代表可以假设所有券商请求天然具备 exactly-once 语义。

成交使用 `(client_order_id, trade_id)` 联合去重。
重复成交回报不会再次扣现金；延迟 `Accepted` 不会把已部分成交订单退回未成交状态。
已正常接受的长期挂单也不会仅因超过 30 秒就被判定超时；
`timed_out` 关注提交结果、撤销结果和未知状态。

`LiveLedger` 是从完整订单/批次账本派生的活跃索引。
重复查询走索引，相关变更使其失效后再重建；周期只重算事件关联批次。
这保留审计历史，同时减少每个行情事件扫描全部历史的开销。

## 14. 重启为什么不能只读一个仓位数字

券商显示“持有 100 股”，但不能单凭这个数字知道其中多少是 Core、多少来自旧网格、
每个批次对应什么止盈价、哪些数量已经被卖单预留。
这些策略归属由检查点保存，实际订单和持仓必须与券商状态一致。

[multi_asset.rs::checkpoint / persist][multi] 用一个 version 6 检查点保存整个组合：
每股票 `GridState`、订单/批次、指标历史、风险、报告，以及原生订单和持仓。
写入 `.next`、同步文件、原子替换，再同步目录；独占 `.lock` 防止同一检查点被并行使用。
派生 `LiveLedger` 不持久化，恢复后重新构建。

恢复流程是：

1. 校验检查点版本、配置、环境/账户身份和完整标的集合。
1. 恢复原生 cache；Paper/Live 启动使用原生执行引擎的券商 reconciliation。
1. 各股票重算指标并检查网格几何，重放成交且去重。
1. 核对订单归属、累计成交和多头库存数量。
1. 全组合通过恢复屏障后才允许新增交易；启动后还有一分钟入场等待窗口。

未知活动订单、无法解释的仓位和缺失的持久化意图不会被当作“空仓”继续运行。
断连进入保护后，单纯收到重连事件不会自动解除买入限制。
同一检查点的锁也不能防止两个进程使用不同检查点操作同一券商账户。

## 15. JSON 到运行参数：三个不同的资金层次

当前入口是 [dynamic_grid_portfolio.json][portfolio-json]，引用：

```text
dynamic_grid_portfolio.json
    ├─ instuments/AAPL.json
    └─ instuments/MSFT.json
```

目录实际拼写是 `instuments`。`load_portfolio_config` 按组合文件所在目录解析这些引用；
行情 CSV 路径沿用相对运行工作目录的语义，二者不同。

| 配置位置                           | 含义                                        | 当前 AAPL 示例                                 |
| ---------------------------------- | ------------------------------------------- | ---------------------------------------------- |
| `portfolio.capital`                | 整个账户研究资金。                          | 100,000 美元。                                 |
| `strategy.capital_allocation`      | 单股票初始分配占组合的比例。                | 30%。                                          |
| `strategy.grid.capital`            | 构造时由上述两者相乘覆盖。                  | 运行时为 30,000，不能按 JSON 的 100,000 理解。 |
| `strategy.grid.capital_allocation` | 在该股票局部权益中参与网格/目标计算的比例。 | 100%。                                         |
| `strategy.max_position_pct`        | 单股票相对组合权益的上限。                  | 40%，还受组合最大分配 30% 等约束。             |
| `strategy.grid.max_position_pct`   | 单股票相对局部权益的上限。                  | 100%，仍受金额等限制。                         |

[AAPL.json][aapl-json] 和 [MSFT.json][msft-json] 当前每侧均 3 层、ATR 模式、3%–6% 间距、
1% Core、90% Grid、40% 网格种子库存。AAPL/MSFT 的 ATR 乘数分别为 8/7，
最短重置间隔分别为 9,000/900 秒，突破确认分别为 5/2 根。
这些是源码核对时的示例值，不是 `GridConfig::default()`，也不代表运行中的进程已热更新。

例如 AAPL 局部权益 30,000，`max_notional = 20,000`，
`target_position` 的基础金额先被压到 20,000，再分配 Core/Grid。
因此“股票预算 30%”不等于实际将买满账户的 30%。
行业上限、未成交预留、整股取整、旧库存和行情过滤还会继续缩小可用空间。

Paper 的 [dynamic_grid_paper.json][paper-json] 引用同一组合配置，
把 `AAPL.SIM` 映射到 `AAPL.US.LONGBRIDGE` 等券商标识。
[dynamic_grid_config.rs::AppConfig::strategy][live-config] 会为实时路径设置
`confirmed_custom_bars = true`、`tick_execution = true`。
复用的是经济参数和核心决策，数据触发方式由 runner 适配。

当前没有参数热加载。修改 JSON 后要重新加载配置；已有检查点还会校验恢复配置是否兼容。
即使当前用 ATR 模式，`spacing_pct` 也必须位于 min/max 范围内，这是现有强校验行为。

## 16. 怎样正确阅读收益报告

### 周期收益和账户收益

[orders.rs::complete_cycle][orders] 使用两种口径：

```text
gross_pnl     = 决策参考卖出金额 - 决策参考买入金额
execution_pnl = 实际卖出成交金额 - 实际买入成交金额
net_pnl       = gross_pnl - signed_slippage - fees
              = execution_pnl - fees
```

实际成交价差已经包含成交价格偏离，不能再扣一次滑点。
`slippage` 是有符号执行损耗，负值表示改善；`slippage_cost` 只累计不利部分，
`price_improvement` 只累计有利部分。

[analytics.rs::finish][analytics] 中，账户 `net_pnl = 最终权益 - 初始资金`，
且 `unrealized_pnl = net_pnl - realized_pnl`。
报告 `gross_pnl` 汇总的是已完成周期的参考价收益；`fees` 包含未平库存入场费。
它们不是同一统计集合，不能用报告顶层 `gross_pnl - fees - slippage` 代替账户净收益。

以此前提供的报告为算术例子：已实现 684.39、未实现 -2,866.63，
两者相加正好是 -2,182.24。完成周期胜率 100% 不会改变这项亏损。
这里引用已有数字讲解口径，并非本教程重新运行了回测。

### 常见指标误读

| 字段                    | 当前代码的统计含义                                                  |
| ----------------------- | ------------------------------------------------------------------- |
| `number_of_trades`      | 去重后的成交回报笔数，不是完整周期数。                              |
| `number_of_grid_cycles` | 已完成的 Grid 批次，不包含尚未平仓的亏损库存。                      |
| `win_rate`              | 完成库存周期的胜率；另有 Grid 专用胜率。                            |
| `profit_factor = null`  | 没有可用的亏损周期分母，不能解释为无限盈利能力。                    |
| `grid_fill_rate`        | 有过成交的 Grid 订单数 / 账本中的 Grid 订单数，包含入场和出场。     |
| `capital_utilization`   | 库存加待买预留占权益的比例，按记录点间的自然时间加权。              |
| `inventory_exposure`    | 库存市值 / 初始资金的时间加权值，与利用率不同。                     |
| `grid_capture_ratio`    | 完成 Grid 周期净收益 / 对应参考价毛收益，不是捕捉了全市场多少波动。 |
| `false_reset_count`     | 重置前没有入口订单成交的次数，不是价格后来回到旧区间的次数。        |
| `grid_churn`            | 记录的撤单请求数量，不是成交数量。                                  |

资金利用率的时间加权包括记录点之间的隔夜和周末间隔，不是只平均开盘时段。
Sharpe/Sortino 来自日权益收益，年化使用 252；CAGR 使用实际自然时间跨度。
组合指标基于合并权益曲线计算，不能平均两只股票的 Sharpe。

### Gap PnL 和 Disabled PnL 不是独立的扣款

`gap_pnl` 记录识别 Gap 时的库存数量乘以开盘价差，`gap_loss` 累计其中负向部分。
它是权益变化的归因，不应再加到 `net_pnl` 上扣一次。
由于记录发生在首根完成 Bar 回调时，如果此前已有成交，它使用的库存可能不同于前夜收盘库存；
当前字段不能冒充精确的逐持仓隔夜归因。

regime 归因把两个相邻权益点的变化归到前一个点的分类，组合再汇总各股票归因。
因此 `Disabled PnL` 很差，表示对应标签时段承受了损失；
不能仅凭这个数字证明全部损失都由 Disabled 过滤器造成。
需要结合停买时段、成交、持仓和 Gap 记录判断因果。

## 17. 回测、Sandbox、Paper、Live 如何共用核心

| 环境            | 行情来源                  | 执行方式                                   |
| --------------- | ------------------------- | ------------------------------------------ |
| 历史 Bar 回测   | 完成 Bar CSV。            | 原生 BacktestEngine 的 Bar 撮合。          |
| 历史 Quote 回测 | 完成 Bar + 每标的 Quote。 | Bar 更新信号，Quote 驱动，原生流动性消耗。 |
| Sandbox         | Longbridge 实时行情。     | 本地 `SandboxExecutionClient`。            |
| Paper           | Longbridge 实时行情。     | Longbridge 模拟交易账户。                  |
| Live            | Longbridge 实时行情。     | Longbridge 实盘账户。                      |

[portfolio.rs::run_portfolio_backtest_with_progress][backtest] 构造同一个 `MultiAssetGridStrategy`。
无 Quote 时采用 Bar 执行；存在 Quote 时要求每只股票都有 Quote。
同时间的数据稳定排序为先 Bar、后 Quote，再按标的排序，使共享现金的处理顺序可复现。

当前回测的 `OneTickSlippageFillModel` 使用固定随机种子与单 tick 滑点概率，
手续费通过原生 instrument 的 maker/taker 参数设置。
配置中的比例 `slippage` 主要用于预算和成本门槛，不能把它理解为每笔回测必定扣该比例。
Bar 模型无法还原真实开盘竞价、盘口排队、逐笔路径和市场冲击；
相同核心逻辑也不意味着 Bar 回测与真实券商有相同成交结果。

[Longbridge runner][live-runner] 的默认调用只打印已校验的有效配置。
`--check-paper` 是只读连通性检查；`--run` 才启动交易，Live 还要求配置与 `--live` 同时确认。
本教程没有执行这些交易入口。

研究入口已有比较、消融、Walk-forward 和 Monte Carlo。
`--dynamic-only` 按配置选择实际模式，“dynamic” 报告名字不保证是 `LegacyDgt`。
Walk-forward 的训练、验证、测试分区独立回放，不等于连续带仓在线优化。
费用/滑点扰动保留历史成交路径，不会重新模拟改变费用后的下单反馈。
已有研究工具和历史报告不代表当前参数已经通过新的样本外验证。

## 18. 推荐的源码阅读路线与练习

按以下顺序，每轮只带一个问题进入代码：

1. `engine.rs::build`：用中心 100、间距 2% 手算前两层，核对取整。
1. `orders.rs::entry / fill / exit_quantity / complete_cycle`：追踪买 10、先成交 4 的例子。
1. `strategy.rs::drive`：找到创建网格、挂单和提前返回的位置。
1. `regime.rs::update` 与 `position.rs::target_position`：区分市场分类、目标和入场许可。
1. `stock.rs::observe_bar / gate`：区分 1% 低开和 5 ATR 低开，检查当前分钟 ATR 单位。
1. `multi_asset.rs::gate` 与 `portfolio.rs::capacity`：模拟两只股票争用同一现金额度。
1. `strategy.rs::recover` 与 `multi_asset.rs::persist`：解释为什么未知订单必须阻止恢复。
1. `analytics.rs::finish`：用已实现加未实现核对账户净收益，再检查各统计分母。

以下现有测试可以作为可执行教材，源码分别位于
[核心测试][unit-tests]、[组合测试][portfolio-tests]、[原生集成测试][integration-tests]：

- `geometric_levels_and_rounding`：几何网格与 tick 取整。
- `partial_fills_covered_exits_cycle_and_reentry`：部分成交、覆盖卖单和重新入场。
- `cancel_failure_retains_reservations_and_late_fill`：撤单失败与延迟成交。
- `simultaneous_buys_reserve_shared_capacity_before_any_fill`：先预留后成交。
- `upward_and_ordinary_downward_gaps_do_not_pause_entries`：位于 `stock.rs`，验证方向性 Gap 规则。
- `sadg_cancels_entries_at_close_and_does_not_fill_an_invented_gap_path`：真实开盘与禁止虚构路径。
- `future_bars_do_not_change_prefix_decisions`：未来数据不能改变历史前缀决策。

在仓库根目录可按测试名运行一个例子：

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib partial_fills_covered_exits_cycle_and_reentry --profile dev -j 2
```

仅回测当前配置所选 Dynamic Grid 的命令为：

```bash
CARGO_INCREMENTAL=0 cargo run -p nautilus-backtest --features examples \
  --bin dynamic-grid-backtest --profile dev -j 2 -- \
  --dynamic-only crates/backtest/examples/dynamic_grid_portfolio.json \
  reports/dynamic-grid-learning.json
```

运行前确认各标的 `bars_path` 数据存在、`reports` 目录存在，且输出文件可覆盖或尚不存在。
该命令会执行所配置时间范围的回测，耗时取决于数据量与本机构建缓存。
本次只新增解释文档，没有运行上述回测，也没有改变策略或配置。

## 19. 读懂以后应能回答的问题

- 为什么 ADX 满足条件仍可能无法创建网格？检查其余分类、预热、稳定性和风险准入。
- 为什么调了 ATR 乘数却看不到变化？比较 `raw`、`effective`、`active`，确认是否被下限截断。
- 为什么股票上涨后仍有 Core？普通 `exits` 只处理 Grid 批次。
- 为什么停止买入仍会亏损？库存没有消失，Hold 和未降低的目标仍保留方向风险。
- 为什么重置后还有旧成本？旧 `InventoryLot` 继续存在，其数量仍计入共享风险。
- 为什么所有完成周期都盈利，账户却亏损？亏损可能留在未完成库存或 Core 再平衡中。
- 为什么放行高开并不保证立即卖出？仍取决于订单、库存覆盖、趋势策略和真实成交条件。
- 为什么组合还留有现金却拒绝买单？行业、相关簇、标的额度、陈旧估值都可能更早成为约束。

这些问题都能沿“行情 → 状态 → 几何 → 库存 → 预留 → 最终准入 → 成交 → 账本”的链路定位。
研究参数时，把变化对应到这条链上的具体行为，再用包含未平库存的账户收益验证效果。

[config]: ../../crates/trading/src/examples/strategies/dynamic_grid/config.rs
[module]: ../../crates/trading/src/examples/strategies/dynamic_grid/mod.rs
[engine]: ../../crates/trading/src/examples/strategies/dynamic_grid/engine.rs
[regime]: ../../crates/trading/src/examples/strategies/dynamic_grid/regime.rs
[risk]: ../../crates/trading/src/examples/strategies/dynamic_grid/risk.rs
[stock]: ../../crates/trading/src/examples/strategies/dynamic_grid/stock.rs
[position]: ../../crates/trading/src/examples/strategies/dynamic_grid/position.rs
[orders]: ../../crates/trading/src/examples/strategies/dynamic_grid/orders.rs
[strategy]: ../../crates/trading/src/examples/strategies/dynamic_grid/strategy.rs
[multi]: ../../crates/trading/src/examples/strategies/dynamic_grid/multi_asset.rs
[portfolio]: ../../crates/trading/src/examples/strategies/dynamic_grid/portfolio.rs
[analytics]: ../../crates/trading/src/examples/strategies/dynamic_grid/analytics.rs
[diagnostics]: ../../crates/trading/src/examples/strategies/dynamic_grid/diagnostics.rs
[files]: ../../crates/trading/src/examples/strategies/dynamic_grid/files.rs
[portfolio-json]: ../../crates/backtest/examples/dynamic_grid_portfolio.json
[aapl-json]: ../../crates/backtest/examples/instuments/AAPL.json
[msft-json]: ../../crates/backtest/examples/instuments/MSFT.json
[paper-json]: ../../crates/adapters/longbridge/examples/dynamic_grid_paper.json
[live-config]: ../../crates/adapters/longbridge/bin/dynamic_grid_config.rs
[live-runner]: ../../crates/adapters/longbridge/bin/dynamic_grid.rs
[backtest]: ../../crates/backtest/src/dynamic_grid/portfolio.rs
[unit-tests]: ../../crates/trading/src/examples/strategies/dynamic_grid/tests.rs
[portfolio-tests]: ../../crates/trading/src/examples/strategies/dynamic_grid/portfolio_tests.rs
[integration-tests]: ../../crates/trading/tests/dynamic_grid.rs
