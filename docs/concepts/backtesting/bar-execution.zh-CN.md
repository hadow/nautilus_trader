# 基于 Bar 的执行（中文版）

本文基于同目录的英文文档 [Bar-Based Execution](bar-execution.md) 整理翻译。

Bar 数据记录一个时间区间内的开盘价、最高价、最低价、收盘价和成交量。
它不记录各价格在区间内何时出现，也不能说明最高价是否先于最低价出现。

因此，基于 Bar 的执行会模拟一条合理的区间内价格路径，
而不是重建原始成交序列。

NautilusTrader 会把每根用于执行的 Bar 转换为 L1 订单簿的一组模拟市场更新。
这些更新依次经过 Bar 的价格路径时，会与静置订单撮合。

## Bar 时间戳约定

:::warning
在执行模拟中，每根 Bar 的初始化时间戳 `ts_init` 必须表示该区间的**收盘时刻**。
这样可以防止完整 Bar 在实际形成之前就对 Strategy 可见。
:::

事件时间戳 `ts_event` 可以表示开盘时刻或收盘时刻，具体取决于数据源：

- 如果 Bar 的时间戳位于收盘时刻，将 `ts_init` 设为相同时间戳。
- 如果 Bar 的时间戳位于开盘时刻，设置 `ts_init = ts_event + interval_ns`。
  例如，一分钟 Bar 应加上 `60_000_000_000` 纳秒。

如果适配器提供 `bars_timestamp_on_close=True` 等设置，应优先使用该设置，
使存储的数据符合预期约定。

对于自定义数据，应在构造 `Bar` 对象、编码 Arrow RecordBatch、写入数据目录，
或调用 `add_data()` 前填充 `ts_event` 和 `ts_init`。

`BarDataWrangler` 使用显式时间戳字段，不提供 `ts_init_delta` 参数。
开始完整回测前，应先使用少量样本验证结果。

## 处理 Bar 数据

只有满足以下全部条件时，Bar 才会用于执行：

- 场所配置了 `bar_execution=True`。
- 场所使用 `BookType.L1_MBP`。
- Bar 使用外部聚合来源。

内部聚合的 Bar，以及发送给 L2 或 L3 场所的 Bar，仍会到达已经订阅的 Strategy，
但不会更新撮合引擎的订单簿，也不会触发撮合。

对于每根适用的 Bar，引擎会：

1. 选择该 Instrument 已配置的最细粒度 BarType。
1. 将 Bar 成交量分配到四个模拟更新中。
1. 按配置的顺序处理开盘价、最高价和最低价，最后处理收盘价。
1. 在每个模拟更新后撮合订单。
1. 将完整 Bar 分发给 Actor 和 Strategy。

因此，在 Bar 开始时已经静置的订单，可以在某个中间 OHLC 价位成交。
从 `on_bar` 提交的订单，则会在当前 Bar 的四个价位全部处理完后才到达。

## OHLC 价格模拟

引擎将 Bar 成交量平均分配给四个价位。
所有余量都会分配给收盘价，以确保模拟更新保留总成交量。

如果四分之一成交量小于 Instrument 的最小 `size_increment`，
则每个价位都会使用该最小步长。

场所的 `bar_adaptive_high_low_ordering` 选项控制区间内价格路径：

- 使用默认值 `False` 时，每根 Bar 都使用 `Open -> High -> Low -> Close`。
- 设为 `True` 时，引擎先访问离开盘价更近的极值：

  - 如果开盘价更接近最高价，使用 `Open -> High -> Low -> Close`。
  - 如果开盘价更接近最低价，使用 `Open -> Low -> High -> Close`。

自适应路径是一种确定性启发式规则，并非对真实成交顺序的重建。
其准确性取决于市场、时间区间和数据源。

一份[探索性 EUR/USD 分析](https://gist.github.com/stefansimik/d387e1d9ff784a8973feca0cde51e363)
为这种距离启发式规则提供了依据，但并未证明一个普遍适用的准确率。

如果保护性止损和止盈目标都落在同一根 Bar 内，价格路径会直接影响结果，
因为先访问的价位决定哪张订单可以先成交。

在场所上配置自适应顺序：

```python
from nautilus_trader.backtest import BacktestEngine
from nautilus_trader.config import BacktestEngineConfig
from nautilus_trader.model import AccountType
from nautilus_trader.model import Money
from nautilus_trader.model import OmsType
from nautilus_trader.model import Venue

engine = BacktestEngine(BacktestEngineConfig())
engine.add_venue(
    venue=Venue("SIM"),
    oms_type=OmsType.NETTING,
    account_type=AccountType.CASH,
    starting_balances=[Money.from_str("10_000 USDT")],
    bar_adaptive_high_low_ordering=True,
)
```

## 订单提交时序

第 N 根 Bar 的 OHLC 序列会在 `on_bar(N)` 之前运行。
如果没有延迟模型，从 `on_bar` 提交的订单会立即与第 N 根 Bar 收盘后留下的订单簿撮合。

延迟模型会推迟订单的实际到达时间。
如果只有 Bar 数据且期间没有时间事件，订单在下一个数据时间戳到达时，
会在下一根 Bar 的 OHLC 扫描之后结算，因此它看到的是该 Bar 的收盘状态。

报价 Tick、成交 Tick 或由定时器驱动的结算，
可能更早地排空命令，使其与当时的订单簿状态撮合。

```python
from nautilus_trader.execution import StaticLatencyModel

engine.add_venue(
    venue=Venue("SIM"),
    oms_type=OmsType.NETTING,
    account_type=AccountType.CASH,
    starting_balances=[Money.from_str("10_000 USDT")],
    latency_model=StaticLatencyModel(base_latency_nanos=1_000_000_000),
)
```

:::note
引擎不提供原生的“下一根 Bar 开盘成交”模式。
Strategy 可以根据已经完成的上一根 Bar 形成信号而不产生前视偏差，
但下一根 Bar 的开盘价会在该 Bar 分发前被处理。

在当前 Bar 的 `on_bar` 回调中使用该 Bar 的开盘价会引入前视偏差。
如果只有 Bar 数据，使用延迟通常会在更晚的订单簿状态结算，而不是在下一根 Bar 开盘价结算。
:::

## 内部 Bar 聚合时序

当 DataEngine 根据 Tick 聚合时间 Bar 时，定时器会在区间边界关闭 Bar。
具有完全相同时间戳的数据原本可能在该关闭定时器之后才处理。

可以在 `DataEngineConfig` 中设置 `time_bars_build_delay`，延迟该定时器：

```python
from nautilus_trader.config import BacktestEngineConfig
from nautilus_trader.config import DataEngineConfig

config = BacktestEngineConfig(
    data_engine=DataEngineConfig(
        time_bars_build_delay=1,
    ),
)
```

该值的单位是微秒。
一微秒等较小延迟可以让边界数据在 Bar 关闭前到达。
此设置只影响内部聚合的 Bar。
