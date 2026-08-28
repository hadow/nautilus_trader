# 成交价格与撮合（中文版）

本文基于同目录的英文文档
[Fill Prices and Matching](fill-prices-and-matching.md) 整理翻译。

回测撮合引擎将历史订单簿和成交数据视为不可变数据。
模拟成交不会修改历史订单簿，这可以保持回放市场不变，
但必须显式决定同一显示流动性是否可以让多张模拟订单成交。

引擎提供两个控制项：

- `liquidity_consumption=True`：追踪每个价格档位已经消耗的显示数量。
- 为成交模型设置固定 `random_seed`：使该模型的概率决策可以重复。
  它不会配置模型之外的随机行为或执行顺序。

## 成交价格的确定

成交价格取决于订单类型、流动性方向、订单簿类型，
以及触发撮合时的市场状态。

### L2 和 L3 订单簿

使用深度数据时，市场型订单会依次撮合已经穿过的订单簿档位。
限价型订单作为 Taker 时使用穿过的订单簿价格，
作为 Maker 时则使用自身限价。

<!-- markdownlint-disable MD060 -->

| 订单类型                   | 成交行为                              |
| ---------------------- | --------------------------------- |
| `MARKET`               | 作为 Taker 依次撮合已经穿过的订单簿档位。          |
| `MARKET_TO_LIMIT`      | 依次撮合订单簿，剩余数量以第一笔成交价静置。            |
| `LIMIT`                | 作为 Taker 时使用穿过的档位，作为 Maker 时使用限价。 |
| `STOP_MARKET`          | 触发后依次撮合穿过的档位。                     |
| `STOP_LIMIT`           | 触发后使用限价型规则。                       |
| `MARKET_IF_TOUCHED`    | 触发后依次撮合穿过的档位。                     |
| `LIMIT_IF_TOUCHED`     | 触发后使用限价型规则。                       |
| `TRAILING_STOP_MARKET` | 激活并触发后依次撮合穿过的档位。                  |
| `TRAILING_STOP_LIMIT`  | 激活并触发后使用限价型规则。                    |

<!-- markdownlint-enable MD060 -->

如果已经穿过的可用数量小于订单剩余数量，深度订单可以部分成交。

### L1 订单簿

使用 L1 订单簿时，历史市场只提供最优买价和卖价：

<!-- markdownlint-disable MD060 -->

| 订单类别                                                                | 成交行为                               |
| ------------------------------------------------------------------- | ---------------------------------- |
| `MARKET`、`MARKET_IF_TOUCHED`、`STOP_MARKET` 和 `TRAILING_STOP_MARKET` | 先使用市场价或触发价规则，剩余数量再以差一个 Tick 的价格成交。 |
| `MARKET_TO_LIMIT`                                                   | 使用最优对手价，剩余数量以第一笔成交价静置。             |
| 限价型 Taker                                                           | 使用已经穿过的最优报价，但不超过限价边界。              |
| 限价型 Maker                                                           | 被 Trade 或市场移动撮合时使用订单自身限价。          |

<!-- markdownlint-enable MD060 -->

当符合条件的市场型订单耗尽 L1 显示数量后，会应用单 Tick 残余成交规则。
如果残余成交会越过配置的价格保护边界，价格保护可以阻止该成交。

这条确定性的残余规则与成交模型中的概率滑点彼此独立。

由 Trade 驱动的撮合还有一条额外规则：
如果一笔 Trade 证明某个不在订单簿中的价格存在成交流动性，
引擎会按订单限价成交，并将数量限制在该 Trade 的数量以内。

参见[基于 Trade 的执行](trade-execution.zh-CN.md#由-trade-驱动的撮合)。

成交模型可以改变成交价，或者提供模拟深度订单簿。
参见[成交模型](fill-models.zh-CN.md)。

### Bar 触发的市场型订单成交

对于 `STOP_MARKET`、`MARKET_IF_TOUCHED` 和 `TRAILING_STOP_MARKET`，
基于 Bar 的执行会区分跳空与区间内价格移动。

如果 Bar 开盘价已经越过触发价，订单会按模拟市场价格成交。
例如，触发价为 100 的 SELL 止损单，在下一根 Bar 以 90 开盘时可以成交在 90。

如果 Bar 开盘时尚未越过触发价，而之后的最高价、最低价或收盘价穿过触发价，
引擎会使用触发价。例如：

1. 一张 SELL 止损单的触发价为 100。
1. Bar 以 102 开盘。
1. 最低价达到 98。
1. 订单以 100 成交。

该规则假设价格连续穿过触发价。
如果 Strategy 需要更精确的跳空和路径行为，应使用报价、成交或订单簿数据。

## 价格保护

价格保护限制 `MARKET` 和 `STOP_MARKET` 订单能够在订单簿中穿过的距离。
保护偏移量使用 Instrument 最小价格步长的倍数表示：

```python
from nautilus_trader.config import BacktestVenueConfig
from nautilus_trader.model import AccountType
from nautilus_trader.model import BookType
from nautilus_trader.model import OmsType

venue = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.NETTING,
    account_type=AccountType.MARGIN,
    book_type=BookType.L2_MBP,
    starting_balances=["100_000 USDT"],
    price_protection_points=100,
)
```

引擎在成交时计算保护边界：

- BUY：`ask + (points * price_increment)`
- SELL：`bid - (points * price_increment)`

对于最小价格步长为 0.01 的 Instrument，100 点允许 BUY 订单最高成交在当前卖价之上 1.00。
超出边界的档位会被排除，因此订单可能只完成部分成交。

市场订单在处理时获得价格保护边界。
止损市价单则在触发时，根据当时的买价或卖价获得边界。

将 `price_protection_points=0` 设为 0 可以禁用价格保护。

## 订单簿不可变性

模拟成交永远不会扣减历史订单簿。
默认情况下，每次撮合迭代都可以使用完整的历史显示数量。

可以启用流动性消耗追踪：

```python
from nautilus_trader.config import BacktestVenueConfig
from nautilus_trader.model import AccountType
from nautilus_trader.model import BookType
from nautilus_trader.model import OmsType

venue = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.NETTING,
    account_type=AccountType.CASH,
    book_type=BookType.L1_MBP,
    starting_balances=["100_000 USD"],
    liquidity_consumption=True,
)
```

使用 `liquidity_consumption=True` 时，
引擎会记录每个价格档位的原始数量和已消耗数量。
可用数量为 `original_size - consumed`。

该档位收到新的订单簿更新后，记录会重置为新的显示数量。

### L1 被动成交

当 L1 市场价格穿过一张被动限价单时：

<!-- markdownlint-disable MD060 -->

| `liquidity_consumption` | 剩余数量的处理方式                |
| ----------------------- | ------------------------ |
| `False`                 | 订单全部剩余数量均按自身限价成交。        |
| `True`                  | 只成交尚未消耗的显示数量，剩余数量继续保持开启。 |

<!-- markdownlint-enable MD060 -->

例如：

1. 卖价为 100.10，显示数量为 50。
1. 一张数量 1,000、限价 100.05 的 BUY LIMIT 静置。
1. 新卖价变为 100.00，显示数量为 30。
1. 启用流动性消耗后，成交 30，剩余 970 保持开启。
1. 后续更新可以提供新的可用数量。

### Trade 流动性消耗

一笔 Trade 可以证明当前订单簿中不存在的某个价格曾具有可执行数量。
启用消耗追踪后，由 Trade 驱动的多笔成交会共享这部分数量，
而不会让每张订单都使用完整 Trade 数量。

对于 L2 和 L3 订单簿，触发撮合的 Trade 可能已经消耗了显示深度。
引擎会先扣除这部分成交量，再让被触发的订单使用剩余档位数量。

如果更新的订单簿已经反映该 Trade，引擎会跳过这项调整。

成交模型返回的模拟订单簿不使用逐档消耗追踪。
模型必须自行表达其流动性假设。

### 局限

流动性消耗追踪估算的是可用数量，不是订单优先级。
如果要追踪显示队列，可以配合订单簿和成交数据设置 `queue_position=True`；
如果只需概率近似，可以使用 `prob_fill_on_limit`。

由 Trade 驱动的成交也具有机会性：一笔成交只能证明流动性在某一瞬间存在，
不能证明历史成交之后仍有相同流动性可用。

## 精度要求

价格和数量必须使用 Instrument 配置的 `price_precision` 和 `size_precision`。
精度不匹配的结果取决于输入进入撮合引擎的路径：

<!-- markdownlint-disable MD060 -->

| 输入          | 校验字段            | 不匹配时的结果           |
| ----------- | --------------- | ----------------- |
| `QuoteTick` | 买卖价格和数量         | 记录警告并跳过该 Tick。    |
| `TradeTick` | 价格和数量           | 记录警告并跳过该 Tick。    |
| 可执行 `Bar`   | 开盘、最高、最低、收盘和成交量 | 记录警告并跳过该 Bar。     |
| 新订单         | 数量、显示数量、价格和触发价  | 拒绝订单。             |
| 订单更新        | 数量、价格和触发价       | 拒绝修改。             |
| 生成的成交       | 成交价和成交量         | 兼容时规范化，否则警告并跳过成交。 |

<!-- markdownlint-enable MD060 -->

连续发生 20 次市场数据精度不匹配后，引擎会记录一条错误。
使用 `shutdown_on_error=True` 时，该错误可以请求正常关闭回测。

一条有效的报价、成交或可执行 Bar 会重置连续不匹配计数。

`Bar.volume` 必须使用 Instrument 的数量单位和数量精度。
创建 Bar 前，应转换数据提供商特有的计价成交量字段。

使用 Instrument 工厂方法构造兼容值：

```python
price = instrument.make_price(raw_price)
quantity = instrument.make_qty(raw_quantity)
```

还应确认 Instrument 定义与数据源匹配，
并确保自定义加载器保留源数据精度。
