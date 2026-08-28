# 基于 Trade 的执行（中文版）

本文基于同目录的英文文档 [Trade-Based Execution](trade-execution.md) 整理翻译。

场所使用默认配置 `trade_execution=True` 时，Trade Tick 会触发撮合。
一笔 Trade 证明其价格上曾发生流动性成交，
因此可以使被动方向上静置的订单成交。

如果只希望把 Trade 作为 Strategy 数据，而不希望它触发撮合，
可以设置 `trade_execution=False`：

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
    trade_execution=False,
)
```

禁用 Trade 执行后，Trade Tick 不会运行订单撮合，
也不会运行撮合引擎的维护任务，例如 GTD 到期、追踪止损激活和 Instrument 到期检查。

之后到达的报价或可执行 Bar 可以运行这些维护任务。

## 由 Trade 驱动的撮合

引擎会在本次迭代中，临时把撮合参考价格移动到 Trade 价格：

- `SELL` Trade 可以撮合静置的 BUY 订单。
- `BUY` Trade 可以撮合静置的 SELL 订单。
- `NO_AGGRESSOR` Trade 由于无法判断被动方，可以影响两个方向。

历史订单簿保持不变。
只有撮合核心的临时买价、卖价和最近成交价会在本次迭代中移动。

### 确定成交

当一笔 Trade 触发限价单成交时：

1. 如果订单簿中存在已经穿过的流动性，引擎会与这些订单簿档位成交。
1. 如果订单簿没有表示该 Trade 价格，引擎可以按订单自身限价生成一笔 Trade 驱动成交。
1. Trade 驱动成交数量上限为 `min(order.leaves_qty, trade.size)`。

使用 `liquidity_consumption=False` 时，
同一笔 Trade 的数量可以在一次迭代中支持多张订单。

使用 `liquidity_consumption=True` 时，
Trade 驱动的成交会共享一个消耗计数器，
因此总成交量不会超过尚未消耗的 Trade 数量。

例如，一笔价格为 100.00 的 `SELL` Trade 可以让限价 100.05 的 BUY LIMIT 成交。
如果订单簿中没有相应成交档位，引擎会使用 100.05，
而不是给予订单更优的 Trade 价格。

### 恢复撮合状态

一次迭代结束后，引擎会根据可用的市场基准恢复撮合参考价格：

- 使用 L2 或 L3 数据时，深度订单簿仍是独立的买卖状态来源。
- 使用 L1 报价基准时，非主动方会根据最新报价恢复。
- 只有 Trade 的 L1 数据没有报价基准可供恢复，
  因此最新 Trade 会继续定义可用的最优买卖状态。

解释不含报价的 Trade 数据流时，这一区别很重要。
连续 Trade 可以移动模拟 L1 状态；
但由报价支持的 L1 撮合不会逐步丢弃最新报价中的非主动方。

## 主动方方向

主动方是跨过买卖价差的一方：

- `SELL`：卖方击中买价，可以使静置的 BUY 订单成交。
- `BUY`：买方吃掉卖价，可以使静置的 SELL 订单成交。
- `NO_AGGRESSOR`：数据没有标识主动方。
  如果某项功能需要方向，引擎会同时考虑两个方向。

主动方为 `BUY` 的 Trade 为被动 SELL 订单提供成交证据，而不是 BUY 订单。
主动方为 `SELL` 的 Trade 为被动 BUY 订单提供成交证据，而不是 SELL 订单。

## 组合订单簿与 Trade 数据

订单簿更新建立价差和可见深度，
Trade Tick 则在更新之间提供成交证据。

当深度快照受到节流，而一笔 Trade 发生在最新快照没有包含的价格时，
组合使用两种数据会很有帮助。

但应谨慎使用：

- Trade 必须具有与静置订单相反的主动方方向，才能使其成交。
- 订单簿更新本身也可以穿过订单，无需同时出现 Trade。
- 在缺失的 Trade 价格档位成交时，会应用 Trade 驱动数量上限。
- 启用流动性消耗后，在被触发订单使用剩余深度前，
  引擎会先计算已经从 L2 或 L3 订单簿中移除的 Trade 成交量。

## 队列位置追踪

同时设置 `queue_position=True` 和 `trade_execution=True`，
可以追踪每张 LIMIT 订单前方的显示数量：

```python
venue = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.NETTING,
    account_type=AccountType.MARGIN,
    book_type=BookType.L2_MBP,
    starting_balances=["100_000 USD"],
    trade_execution=True,
    queue_position=True,
)
```

### 队列生命周期

1. LIMIT 订单被接受时，会记录相同方向、相同价格的显示数量快照。
1. 该价格上方向正确的 Trade 会减少订单前方数量。
1. 前方数量降到零后，订单获得成交资格。
1. 只有清空前方队列后剩余的 Trade 数量，才能在当前 Tick 中使订单成交。

例如：

1. 100.00 的买价档位包含 100 单位。
1. 一张数量为 50 的 BUY LIMIT 加入队列，前方有 100 单位。
1. 一笔 80 单位的 `SELL` Trade 将前方队列减少到 20。
1. 一笔 30 单位的 `SELL` Trade 清空队列，并留下 10 单位可用于成交。
1. 下一笔方向正确的 Trade 可以继续成交订单剩余数量。

### 订单簿变化

对于 L2 订单簿和聚合 L3 更新：

- DELETE 会清空该价格档位及其队列。
- UPDATE 会把前方数量限制在该档位新的显示数量以内。

对于 L3 MBO 订单簿：

- 单笔订单 DELETE 会按该订单剩余的追踪数量推进队列。
- 数量减少会按减少量推进队列。
- 数量增加时，增加后的较大订单仍位于前方。
- 价格变化会将该订单簿订单从追踪队列中移除。

修改模拟订单的价格，会在新档位重置其队列位置。
只修改数量则会保留已经取得的队列进度。

### L1 队列追踪

使用 `BookType.L1_MBP` 时，Trade Tick 会减少前方数量，
报价则提供价格移动和显示数量证据：

- 如果市场价格沿远离最优价的方向穿过订单价格，队列会被清空。
- 市场向订单价格靠近时，队列会保留。
- 价格回到之前可见的档位时，前方数量会被限制在新的显示数量以内。
- 位于 BBO 之后的订单会保持等待，直到报价到达其价格或 Trade 穿过该价格。

### 局限

- 队列追踪只适用于 `LIMIT` 订单。
- 每张模拟订单都有独立的队列估计。
- 初始估计只包括订单被接受时可见的订单簿状态。
- `NO_AGGRESSOR` Trade 会同时减少两侧队列。
  这可能比真实情况更早清空队列并使订单成交，
  因此从 Strategy 执行角度看属于乐观假设。
- 历史数据无法揭示隐藏订单或每项交易场所特有的优先级规则。
