# 回测 API 与重复运行（中文版）

本文基于同目录的英文文档
[Backtest APIs and Repeated Runs](apis-and-runs.md) 整理翻译。

NautilusTrader 提供低层 `BacktestEngine` API，用于直接控制回测；
也提供高层 `BacktestNode` API，用于基于数据目录进行可配置运行。

## 选择 API 层级

以下情况适合使用低层 API：

- 数据可以全部放入内存，或者准备手动流式传入多个批次。
- 需要从 Nautilus Parquet 数据目录以外的格式加载数据。
- 需要直接控制场所、Instrument、Actor、Strategy 或 Execution Algorithm。
- 需要复用已经加载的数据，只替换部分组件后重新运行。

以下情况适合使用高层 API：

- 数据已经保存在 `ParquetDataCatalog` 中。
- 数据需要自动分块加载。
- 希望使用一个配置对象描述并标识一次基于数据目录的运行。
- 希望每次独立运行都使用一个全新的引擎。

## 低层 API

低层 API 以 `BacktestEngine` 为中心。
使用 `BacktestEngineConfig` 创建引擎，然后添加场所、Instrument、组件和数据，
最后调用 `run()`：

```python
from nautilus_trader.backtest import BacktestEngine
from nautilus_trader.config import BacktestEngineConfig

engine = BacktestEngine(BacktestEngineConfig())
engine.add_venue(...)
engine.add_instrument(instrument)
engine.add_strategy(strategy)
engine.add_data(data)
engine.run()
```

### 加载数据

每次调用 `add_data()` 都会将输入复制到一个独立数据流。
引擎按回放时间戳排列每个数据流，并在运行期间按时间顺序合并所有数据流。

因此，为每个 Instrument 添加一个批次，不会反复对不断增长的累计列表排序：

```python
engine.add_data(instrument1_bars)
engine.add_data(instrument2_bars)
engine.add_data(instrument3_bars)
engine.run()
```

除非批处理工作流需要自行管理运行就绪状态，否则应保留默认的 `sort=True`。
使用 `sort=False` 调用后，引擎会被标记为尚未准备好运行。

在调用 `run()` 前应先调用 `sort_data()`，
除非后续某次 `add_data(..., sort=True)` 已经恢复就绪状态：

```python
engine.add_data(instrument1_bars, sort=False)
engine.add_data(instrument2_bars, sort=False)
engine.sort_data()
engine.run()
```

`sort_data()` 会将各自已经排序的数据流标记为可以回放。
该方法可以安全地多次调用。

引擎会复制每个输入序列。
调用 `add_data()` 后清空或修改原始 Python 列表，不会改变引擎已经加载的数据流。

### 手动流式处理批次

如果完整数据集无法放入内存，可以使用流式模式：

```python
engine.add_strategy(strategy)

for batch in data_batches:
    engine.add_data(batch)
    engine.run(streaming=True)
    engine.clear_data()

engine.end()
```

当当前数据耗尽时，`run(streaming=True)` 会暂停。
它不会结束 Trader，也不会把定时器推进到该批次之后。

最后一个批次完成后必须调用 `end()`，以便：

- 将定时器刷新到最后一次运行的边界。
- 调用停止处理器。
- 生成最终结果。

低层 API 不提供基于生成器的 `add_data_iterator()` 方法。
`BacktestNode` 可以自动分块读取数据目录；直接使用引擎时，应按上述循环手动传入数据。

## 高层 API

高层 API 以 `BacktestNode` 为中心。
每个 `BacktestRunConfig` 包含：

- 一个或多个 `BacktestVenueConfig` 对象。
- 一个或多个 `BacktestDataConfig` 对象。
- 可选的 `BacktestEngineConfig`。
- 可选的分块大小、时间边界、异常处理和资源释放设置。

应先构建节点，再通过针对具体运行的方法添加 Strategy：

```python
from nautilus_trader.config import BacktestDataConfig
from nautilus_trader.config import BacktestEngineConfig
from nautilus_trader.backtest import BacktestNode
from nautilus_trader.config import BacktestRunConfig
from nautilus_trader.config import BacktestVenueConfig
from nautilus_trader.model import AccountType
from nautilus_trader.model import BookType
from nautilus_trader.model import OmsType

venue = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.HEDGING,
    account_type=AccountType.MARGIN,
    book_type=BookType.L1_MBP,
    starting_balances=["1_000_000 USD"],
)
data = BacktestDataConfig(
    data_type="QuoteTick",
    catalog_path="/data/catalog",
    instrument_id=instrument_id,
)
config = BacktestRunConfig(
    venues=[venue],
    data=[data],
    engine=BacktestEngineConfig(),
    chunk_size=100_000,
)

node = BacktestNode([config])
node.build()
node.add_strategy_from_config(config.id, strategy_config)
results = node.run()
```

`BacktestNode` 还提供方法，向已经构建的具体运行添加 Actor 和内置 Strategy。

## 出错时关闭

设置 `BacktestEngineConfig.shutdown_on_error=True`，
可以在 Rust Logger 产生错误记录时请求正常关闭：

```python
from nautilus_trader.config import BacktestEngineConfig

config = BacktestEngineConfig(shutdown_on_error=True)
```

回测循环会观察该请求，停止 Trader 和各引擎，
并返回截至该时刻已经收集的结果，而不会中止整个进程。

即使组件过滤器抑制了错误记录，或设置了 `bypass_logging=True`，
该错误仍会请求关闭。Python 的 `logging.error(...)` 调用则不会触发此机制。

新的 Kernel 运行开始时，触发状态会被重置。
最终 `on_stop` 及命令结算行为参见[关闭语义](execution-flow.zh-CN.md#关闭语义)。

## 重复运行

`BacktestEngine.reset()` 会将交易状态和已加载组件的状态恢复到初始值。
它仍会保留已经注册的数据、Instrument、场所、Actor、Strategy 和 Execution Algorithm。

重置会清除：

- 订单、持仓和账户余额。
- 组件运行时状态。
- 引擎计数器和时间戳。

重置会保留：

- 通过 `add_data()` 添加的数据。
- Instrument 和场所配置。
- 已注册的 Actor、Strategy 和 Execution Algorithm。

Instrument 会继续保留，因为默认回测 Cache 配置使用
`drop_instruments_on_reset=False`。

### 为独立运行使用新节点

`BacktestNode` 接受一个 `BacktestRunConfig`。
如果要运行互相独立的配置，应逐个创建并释放节点：

```python
configs = [
    BacktestRunConfig(...),
    BacktestRunConfig(...),
    BacktestRunConfig(...),
]
results = []

for config in configs:
    node = BacktestNode([config])
    try:
        results.extend(node.run())
    finally:
        node.dispose()
```

如果 Strategy 不是由 Controller 提供，则每次运行前都应构建节点并注册 Strategy。

### 为参数化运行复用已加载数据

如果多次运行需要共享已经加载的数据和场所设置，可以使用 `reset()`：

```python
engine = BacktestEngine(BacktestEngineConfig())
engine.add_venue(...)
engine.add_instrument(instrument)
engine.add_data(data)

engine.add_strategy(strategy1)
engine.run()

engine.reset()
engine.run()

engine.reset()
engine.clear_strategies()
engine.add_strategy(strategy2)
engine.run()
```

替换 Strategy 实例前，应调用 `clear_strategies()`。
只有当下一次运行需要加载不同数据集时，才使用 `clear_data()`。
