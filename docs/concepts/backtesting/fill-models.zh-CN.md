# 成交模型（中文版）

本文基于同目录的英文文档 [Fill Models](fill-models.md) 整理翻译。

历史数据无法显示一张模拟订单原本会如何与其他市场参与者交互。
成交模型用于控制 NautilusTrader 对限价单成交资格、单 Tick 滑点，
以及可选模拟流动性所作的假设。

## 不同订单簿类型的行为

使用 L2 或 L3 数据时，历史订单簿会提供价格档位和数量。
撮合引擎会逐档撮合，而 `prob_fill_on_limit` 可以模拟触及限价时订单是否成交。

`prob_slippage` 不适用于 L2 或 L3，因为订单簿本身已经决定价格冲击。

使用 L1 订单簿时，包括根据报价、成交或 Bar 更新的订单簿：

- `prob_fill_on_limit` 控制市场触及限价单价格时，订单是否成交。
- 无论订单类型或流动性方向如何，每次成交都会评估 `prob_slippage`。
- 如果滑点随机抽取成功，成交价会向对订单不利的方向移动一个 Tick。
- 成交模型可以提供模拟 L2 订单簿，表示最优买卖价之外的流动性。

例如，使用 `prob_slippage=0.5` 时，每一笔 BUY 成交都有 50% 概率向上移动一个 Tick。
如果一次运行必须复现成交模型的随机结果，应设置 `random_seed`。

如果场所没有指定成交模型，系统会使用 `DefaultFillModel`，
其 `prob_fill_on_limit=1.0`、`prob_slippage=0.0`。

因此，默认模型会把被触及的限价单视为具有成交资格，
而且 L1 成交默认不会产生概率性的单 Tick 滑点。

这不会禁用撮合引擎针对符合条件的市场型订单所使用的独立残余成交规则。

:::warning
成交后，历史订单簿数据仍然保持不可变。
使用 `liquidity_consumption=False` 时，同一显示数量可以在一次迭代中支持多张模拟订单。

设置 `liquidity_consumption=True` 后，系统会在新数据到达前追踪每个价位已经消耗的数量。
参见[订单簿不可变性](fill-prices-and-matching.zh-CN.md#订单簿不可变性)。
:::

## 可用模型

<!-- markdownlint-disable MD060 -->

| 模型                           | 流动性行为                         |
| ---------------------------- | ----------------------------- |
| `DefaultFillModel`           | 使用撮合引擎中记录的订单簿。                |
| `BestPriceFillModel`         | 在最优买价和卖价提供无限数量。               |
| `OneTickSlippageFillModel`   | 在最优价格之外一个 Tick 的位置提供无限数量。     |
| `ProbabilisticFillModel`     | 在最优价格和差一个 Tick 的价格之间选择。       |
| `TwoTierFillModel`           | 最优价放置 10 单位，其余放在差一个 Tick 的价位。 |
| `ThreeTierFillModel`         | 在三个档位分别放置 50、30 和 20 单位。      |
| `LimitOrderPartialFillModel` | 最优价放置 5 单位，其余放在差一个 Tick 的价位。  |
| `SizeAwareFillModel`         | 订单数量达到 10 单位时改变订单簿形状。         |
| `CompetitionAwareFillModel`  | 在最优价公开 1,000 单位的可配置比例。        |
| `VolumeSensitiveFillModel`   | 在最优价放置其内部成交量的 25%。            |
| `MarketHoursFillModel`       | 使用正常模拟价差，或扩大一个 Tick 的模拟价差。    |

<!-- markdownlint-enable MD060 -->

分层模型中的数量是以 Instrument 数量单位表示的模型常量。
使用分层模型前，应确认这些数值适合该 Instrument 的规模。

`CompetitionAwareFillModel` 接受 `[0.0, 1.0]` 范围内的 `liquidity_factor`，
默认值为 `0.3`，并确保计算出的数量至少为一个 Instrument 数量单位。

当前 Python Binding 不公开 `VolumeSensitiveFillModel` 或
`MarketHoursFillModel` 的状态 Setter。

因此从 Python 使用时，它们会保留初始值：最近成交量为 1,000 单位，
并处于正常流动性模式。

## 配置

将内置模型对象直接传给 `BacktestVenueConfig`：

```python
from nautilus_trader.config import BacktestVenueConfig
from nautilus_trader.execution import DefaultFillModel
from nautilus_trader.model import AccountType
from nautilus_trader.model import BookType
from nautilus_trader.model import OmsType

venue = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.NETTING,
    account_type=AccountType.CASH,
    book_type=BookType.L1_MBP,
    starting_balances=["100_000 USD"],
    fill_model=DefaultFillModel(
        prob_fill_on_limit=0.2,
        prob_slippage=0.5,
        random_seed=42,
    ),
)
```

模拟订单簿模型使用相同的构造参数：

```python
from nautilus_trader.execution import ThreeTierFillModel

venue = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.NETTING,
    account_type=AccountType.CASH,
    book_type=BookType.L1_MBP,
    starting_balances=["100_000 USD"],
    fill_model=ThreeTierFillModel(
        prob_fill_on_limit=1.0,
        prob_slippage=0.0,
        random_seed=42,
    ),
)
```

当前高层场所配置接受内置成交模型，
但不能通过导入路径配置对象加载成交模型。

低层 `BacktestEngine.add_venue()` 方法还可以接受自定义 Python 对象。
该对象必须实现：

- `is_limit_filled() -> bool`
- `is_slipped() -> bool`

还可以选择实现：

- `fill_limit_inside_spread() -> bool`
- `get_orderbook_for_fill_simulation(instrument, order, best_bid, best_ask) -> OrderBook | None`

继承 `nautilus_trader.execution.FillModel` 可以获得这些方法的默认实现。
这种自定义对象协议只适用于低层引擎。

## 概率参数

### `prob_fill_on_limit`（默认值：`1.0`）

该参数控制市场价格仅触及但没有穿过限价单价格时，订单是否成交：

- `0.0`：触及时永不成交。
- `0.5`：平均在一半符合条件的触价中成交。
- `1.0`：触及时总是成交。

穿过限价是另一种独立的撮合条件。
显式队列成交量追踪参见[队列位置追踪](trade-execution.zh-CN.md#队列位置追踪)。

### `prob_slippage`（默认值：`0.0`）

对于 L1 订单簿，该参数控制每笔成交是否向不利方向移动一个 Tick：

- `0.0`：不增加模型滑点。
- `0.5`：平均一半成交增加一个 Tick。
- `1.0`：每笔成交都增加一个 Tick。

该随机抽取同时适用于 Maker 和 Taker 成交，
但不适用于 L2 或 L3 订单簿。

## 模拟订单簿

在确定成交前，撮合引擎会向模型请求一个可选的模拟订单簿。
如果模型返回订单簿，引擎会与其中的价格档位撮合；
如果返回 `None`，引擎使用历史订单簿。

每档 `liquidity_consumption` 追踪不适用于模型生成的模拟订单簿。
自定义模型必须在其返回的订单簿中自行表达所需的流动性消耗行为。
