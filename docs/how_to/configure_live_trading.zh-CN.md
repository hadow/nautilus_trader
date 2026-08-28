# 配置实盘交易节点（中文版）

本文基于同目录的英文文档 [Configure a Live Trading Node](configure_live_trading.md)
整理翻译，用于配置连接真实市场的 `LiveNode`。

节点生命周期参见 [Live Trading 核心概念](../concepts/live.zh-CN.md)，
命令结果参见 [Execution](../concepts/execution.md#command-outcomes)，
状态恢复参见 [Execution reconciliation](../concepts/reconciliation.md)。

:::danger[不建议使用 Jupyter Notebook 运行实盘]
不要在 Jupyter Notebook 中运行实盘交易节点。节点会在调用线程上长期运行事件循环，
而 Notebook 的生命周期控制无法满足生产交易的安全要求：

- Cell 可能乱序执行，Kernel 可能崩溃，运行状态也可能消失。
- Notebook 缺少生产交易所需的可靠日志、监控和优雅关闭能力。

Jupyter 适合回测、分析和实验。实盘交易应使用独立 Python 脚本或服务运行节点。
:::

:::warning[每个进程只运行一个 LiveNode]
同一进程不支持并发运行多个 `LiveNode`，因为运行时状态没有相互隔离。
`run_async()` 也会拒绝同一事件循环中的第二个托管节点。

可以向一个节点添加多个 Strategy；如需更多节点，应使用独立进程。
详细原因参见 [Live Trading：每个进程一个 LiveNode](../concepts/live.zh-CN.md#每个进程一个-livenode)。
:::

:::warning[不要阻塞事件循环]
事件循环线程上的用户代码必须快速返回，包括 Strategy 回调、Actor 处理器和时间事件回调。
这一要求同时适用于 Python 和 Rust。

模型推理、大量计算和同步 I/O 等阻塞操作会造成漏处理成交、数据陈旧和下单延迟。
长时间任务应转移到 Executor、独立线程或独立进程。
:::

:::info[平台差异]
Windows 的信号处理与类 Unix 系统不同。在 Windows 上运行时，请阅读
[Windows 信号处理](#windows-信号处理)，了解优雅关闭与 Ctrl+C（SIGINT）支持。
:::

## `LiveNodeConfig`

`LiveNodeConfig` 保存节点核心组件的配置。
Data Client 和 Execution Client 应通过 `LiveNode.builder(...)` 注册，
不要再通过该配置上的 Client 字典注册。

配置默认值和 `Option<T>` 语义参见 [Configuration](../concepts/configuration.md)。

```python
from nautilus_trader.common import Environment
from nautilus_trader.common import LogLevel
from nautilus_trader.config import CacheConfig
from nautilus_trader.config import LiveDataEngineConfig
from nautilus_trader.config import LiveExecEngineConfig
from nautilus_trader.config import LiveNodeConfig
from nautilus_trader.config import LiveRiskEngineConfig
from nautilus_trader.config import LoggerConfig
from nautilus_trader.config import MessageBusConfig
from nautilus_trader.config import PortfolioConfig
from nautilus_trader.model import TraderId


config = LiveNodeConfig(
    environment=Environment.LIVE,
    trader_id=TraderId.from_str("MY-TRADER-001"),
    logging=LoggerConfig(stdout_level=LogLevel.INFO),
    cache=CacheConfig(),
    msgbus=MessageBusConfig(),
    data_engine=LiveDataEngineConfig(),
    risk_engine=LiveRiskEngineConfig(),
    exec_engine=LiveExecEngineConfig(),
    portfolio=PortfolioConfig(),
)
```

### 核心配置参数

<!-- markdownlint-disable MD060 -->

| 配置项                           | 默认值          | 说明                                  |
| ----------------------------- | ------------ | ----------------------------------- |
| `trader_id`                   | `TRADER-001` | 唯一 Trader ID，使用“名称-标签”格式；标签必须跨节点唯一。 |
| `instance_id`                 | `None`       | 可选的唯一实例 ID。                         |
| `timeout_connection_secs`     | `60.0`       | 客户端连接超时秒数。                          |
| `timeout_reconciliation_secs` | `30.0`       | 执行对账超时秒数。                           |
| `timeout_portfolio_secs`      | `10.0`       | Portfolio 初始化超时秒数。                  |
| `timeout_disconnection_secs`  | `10.0`       | 客户端断开超时秒数。                          |
| `delay_post_stop_secs`        | `10.0`       | Trader 停止后等待残余事件的秒数。                |
| `timeout_shutdown_secs`       | `5.0`        | 等待未完成任务关闭的超时秒数。                     |

<!-- markdownlint-enable MD060 -->

### Trader ID 标签必须唯一

:::warning
最后一个连字符之后的标签会进入生成的 Client Order ID、Order List ID 和 Position ID，
标签前面的名称不会进入这些 ID。
:::

交易同一场所账户的两个节点必须使用不同标签：

```text
MY-TRADER-001
OTHER-TRADER-001
```

这两个 Trader ID 共享标签 `001`，可能生成相同 ID，因此并不安全。

Strategy 设置 `use_uuid_client_order_ids=True` 只会消除 Client Order ID 的这类冲突。
Order List ID 和 Position ID 仍使用 Trader 标签，所以无论如何都必须保持标签唯一。

## 缓存数据库配置

Rust 原生实盘系统通过 `CacheConfig` 配置缓存行为，
通过 `RedisCacheConfig` 配置 Redis 连接。

### Rust Redis 配置

```rust
use nautilus_common::{
    cache::{CacheConfig, database::CacheDatabaseFactory},
    enums::SerializationEncoding,
};
use nautilus_infrastructure::redis::cache::RedisCacheConfig;

let config = CacheConfig {
    encoding: SerializationEncoding::MsgPack,
    timestamps_as_iso8601: true,
    buffer_interval_ms: Some(100),
    flush_on_start: false,
    ..Default::default()
};

let database = RedisCacheConfig {
    host: Some("localhost".to_string()),
    port: Some(6379),
    username: Some("nautilus".to_string()),
    password: Some("pass".to_string()),
    connection_timeout: 2,
    response_timeout: 2,
    ..Default::default()
};

let cache_database = database
    .create(trader_id, instance_id, config.clone())
    .await?;
```

构建 Rust 原生节点后、启动节点前，将数据库 Adapter 附加到节点：

```rust
let node_config = LiveNodeConfig {
    trader_id,
    ..Default::default()
};
let mut node = LiveNode::build("LiveNode".to_string(), Some(node_config))?;
node.set_cache_database(cache_database)?;
node.run().await?;
```

`exec_engine.load_cache` 启用时，节点会在执行对账之前恢复数据库，默认即为启用。

设置 `CacheConfig.flush_on_start = true` 会清空已连接的存储后端，
而不是从中恢复状态。

### Python Redis 配置

Python 通过 `LiveNodeBuilder` 注入相同的数据库配置。
节点启动时构造并拥有该 Adapter：

```python
from nautilus_trader.common import Environment
from nautilus_trader.infrastructure import RedisCacheConfig
from nautilus_trader.live import LiveNode
from nautilus_trader.model import TraderId


node = (
    LiveNode.builder("LiveNode", TraderId("TRADER-001"), Environment.LIVE)
    .with_cache_database_factory(
        RedisCacheConfig(host="localhost", port=6379),
    )
    .with_load_state(True)
    .with_save_state(True)
    .build()
)

try:
    node.run()
finally:
    node.dispose()
```

也可以传入 `PostgresCacheConfig`，使用 Postgres 作为缓存后端。
`with_cache_database_factory()` 收到其他对象时会抛出 `NotImplementedError`，
数据库连接失败则会使 `run()` 失败。

数据库后端节点必须使用 `run()`。`run_async()` 会拒绝可能阻塞宿主事件循环的缓存数据库后端。

### Redis 与 Postgres 的持久化边界

`with_load_state` 和 `with_save_state` 控制 Actor 与 Strategy 自定义状态持久化，
这个能力要求 Redis 后端。

Postgres Adapter 只保存缓存状态，不保存 Actor 或 Strategy 自定义状态：

- 注册了 Actor 或 Strategy 时，`with_load_state(True)` 会在 Trader 启动时失败。
- `with_save_state(True)` 会在节点停止或销毁时失败。

启动时，Kernel 将非空的持久化状态传给 `on_load()`。
节点停止或销毁时，Kernel 保存 `on_save()` 返回的内容。

:::warning
状态持久化不是持续检查点。Kernel 每次运行最多保存一次状态，
所以 `SIGKILL` 或进程崩溃会丢失上一次保存之后的全部变化。

必须销毁节点，让 `dispose()` 关闭后端并刷新缓冲写入。
从 `run()` 直接返回而不销毁节点，可能丢失最终状态保存。
:::

## MessageBus 配置

MessageBus 行为保存在 `MessageBusConfig` 中。
Redis 连接设置保存在实现 `MessageBusBackingFactory` 的 `RedisMessageBusConfig` 中，
由该 Factory 根据设置构建后端。

### Rust Redis MessageBus

```rust
use nautilus_common::{
    enums::SerializationEncoding,
    msgbus::{MessageBusBackingFactory, MessageBusConfig},
};
use nautilus_infrastructure::redis::msgbus::RedisMessageBusConfig;

let config = MessageBusConfig {
    encoding: SerializationEncoding::Json,
    timestamps_as_iso8601: true,
    use_instance_id: false,
    types_filter: Some(vec!["QuoteTick".to_string(), "TradeTick".to_string()]),
    stream_per_topic: false,
    autotrim_mins: Some(30),
    heartbeat_interval_secs: Some(1),
    ..Default::default()
};

let redis_config = RedisMessageBusConfig {
    connection_timeout: 2,
    response_timeout: 2,
    ..Default::default()
};

let backing = redis_config.create(trader_id, instance_id, config.clone())?;
```

### Python Redis MessageBus

Python 通过 `LiveNodeBuilder` 注入 Redis 配置：

```python
from nautilus_trader.common import Environment
from nautilus_trader.common import MessageBusConfig
from nautilus_trader.infrastructure import RedisMessageBusConfig
from nautilus_trader.live import LiveNode
from nautilus_trader.model import TraderId


trader_id = TraderId("TRADER-001")
message_bus = MessageBusConfig(
    external_streams=["external-stream"],
    stream_per_topic=False,
)
redis_config = RedisMessageBusConfig(
    host="localhost",
    port=6379,
)
node = (
    LiveNode.builder("LiveNode", trader_id, Environment.LIVE)
    .with_msgbus_config(message_bus)
    .with_external_msgbus_factory(redis_config)
    .build()
)
node.run()
```

现有代码仍可把 `RedisMessageBusFactory(redis_config)` 传给
`with_external_msgbus_factory()`。

### 外部 MessageBus 生命周期

`MessageBusConfig` 本身不会安装持久化后端，必须与 Factory 一起配置。

Factory 始终安装外部消息出口。调用 `run()` 时，节点也会消费配置的外部 Stream。
节点启动前已经存在于 Stream 中的消息不会重放。

`run_async()` 执行与 `run()` 相同的生命周期，
所以托管在调用方事件循环上的节点也会处理外部 MessageBus 入口。

直接写 Redis 的外部生产者必须提供必需的 `type` 字段。
生命周期和入口行为参见
[MessageBus backing 配置](../concepts/message_bus.md#backing-config)，
线路字段与 Python 自定义数据注册参见
[外部出口和入口](../concepts/message_bus.md#external-egress-and-ingress)。

## 多场所配置

一个节点可以连接多个 Client。下面在构建节点前，
注册 Binance Spot 和 USD-M Futures Data Client：

```python
from nautilus_trader.adapters.binance import BinanceDataClientConfig
from nautilus_trader.adapters.binance import BinanceDataClientFactory
from nautilus_trader.adapters.binance import BinanceEnvironment
from nautilus_trader.adapters.binance import BinanceProductType
from nautilus_trader.common import Environment
from nautilus_trader.live import LiveNode
from nautilus_trader.model import TraderId


node = (
    LiveNode.builder(
        "BINANCE-MULTI-CLIENT-001",
        TraderId.from_str("MULTI-VENUE-001"),
        Environment.LIVE,
    )
    .add_data_client(
        "BINANCE_SPOT",
        BinanceDataClientFactory(),
        BinanceDataClientConfig(
            product_type=BinanceProductType.SPOT,
            environment=BinanceEnvironment.LIVE,
        ),
    )
    .add_data_client(
        "BINANCE_FUTURES",
        BinanceDataClientFactory(),
        BinanceDataClientConfig(
            product_type=BinanceProductType.USD_M,
            environment=BinanceEnvironment.LIVE,
        ),
    )
    .build()
)
```

多 Client 节点的请求、订阅和订单路由规则参见
[Adapters：配置与路由](../concepts/adapters.zh-CN.md#配置与路由)。

## ExecutionEngine 配置

`LiveExecEngineConfig` 控制订单处理、执行事件和场所对账。
完整字段参见 [`LiveExecEngineConfig` API Reference](/docs/python-api-latest/live.html#nautilus_trader.live.LiveExecEngineConfig)。

### 启动对账

执行对账恢复遗漏的订单和持仓事件，使系统内部状态与场所保持一致。

<!-- markdownlint-disable MD060 -->

| 配置项                             | 默认值    | 说明                                |
| ------------------------------- | ------ | --------------------------------- |
| `reconciliation`                | `True` | 启动时执行对账，使内部状态与场所对齐。               |
| `reconciliation_lookback_mins`  | `None` | 为未缓存状态请求历史事件时向前回看的分钟数。            |
| `reconciliation_instrument_ids` | `None` | 需要对账的 Instrument ID 包含列表。         |
| `filtered_client_order_ids`     | `None` | 对账时跳过的 Client Order ID，用于场所侧重复订单。 |

<!-- markdownlint-enable MD060 -->

详细过程参见 [Execution reconciliation](../concepts/reconciliation.md)。

### 订单过滤

订单过滤控制系统处理哪些订单事件和报告，用于避免多个交易节点之间发生状态冲突。

<!-- markdownlint-disable MD060 -->

| 配置项                                | 默认值     | 说明                          |
| ---------------------------------- | ------- | --------------------------- |
| `filter_unclaimed_external_orders` | `False` | 丢弃无人认领的外部订单，避免其影响 Strategy。 |
| `filter_position_reports`          | `False` | 丢弃持仓状态报告；多个节点交易同一账户时可用。     |

<!-- markdownlint-enable MD060 -->

#### 订单标签行为

对账按来源为订单添加标签：

- `VENUE`：在场所发现、由系统外部创建的订单。
- `RECONCILIATION`：为对齐持仓差异而生成的合成订单。

启用 `filter_unclaimed_external_orders` 时，只有带 `VENUE` 标签的订单会被过滤。
`RECONCILIATION` 订单绝不会被过滤，从而保证持仓对齐可以完成。

### 持续对账

启动完成后，持续对账通过以下检查维持运行时执行状态一致：

- 检查在途订单。
- 轮询打开订单。
- 检查持仓状态。
- 审计自有订单簿。

运行时状态转换、重试协调和限制参见
[Runtime checks](../concepts/reconciliation.md#runtime-checks)。

<!-- markdownlint-disable MD060 -->

| 配置项                                  | 默认值        | 说明                                      |
| ------------------------------------ | ---------- | --------------------------------------- |
| `inflight_check_interval_ms`         | `2,000 ms` | 检查在途订单状态的间隔；设为 `0` 禁用。                  |
| `inflight_check_threshold_ms`        | `5,000 ms` | 在途订单触发场所状态查询前的等待时间；托管部署可适当降低。           |
| `inflight_check_retries`             | `5`        | 验证在途订单时允许的重试次数。                         |
| `open_check_interval_secs`           | `None`     | 场所打开订单检查间隔；`None` 或 `0.0` 禁用，建议 5–10 秒。 |
| `open_check_open_only`               | `True`     | 只查询打开订单；`False` 会获取完整历史，资源消耗较高。         |
| `open_check_lookback_mins`           | `60 min`   | 订单状态轮询回看窗口，只处理窗口内修改过的订单。                |
| `open_check_threshold_ms`            | `5,000 ms` | 根据场所差异行动前，距离最新缓存事件的最短时间。                |
| `open_check_missing_retries`         | `5`        | 对符合条件的订单执行定向“未找到”处理前的最大重试次数。            |
| `max_single_order_queries_per_cycle` | `10`       | 每轮单订单查询上限，防止耗尽 API Rate Limit。          |
| `single_order_query_delay_ms`        | `100 ms`   | 单订单查询之间的延迟，避免触发 Rate Limit。             |
| `reconciliation_startup_delay_secs`  | `10.0 s`   | 启动对账完成后，开始持续检查前的等待时间。                   |
| `own_books_audit_interval_secs`      | `None`     | 自有订单簿与公共订单簿的审计间隔。                       |
| `position_check_interval_secs`       | `None`     | 持仓一致性检查间隔；发现差异时查询缺失成交，建议 30–60 秒。       |
| `position_check_lookback_mins`       | `60 min`   | 发现持仓差异时查询成交报告的回看窗口。                     |
| `position_check_threshold_ms`        | `5,000 ms` | 根据持仓差异行动前，距离最近本地活动的最短时间。                |
| `position_check_retries`             | `3`        | 每个 Instrument/Account 差异的最大重试次数。        |

<!-- markdownlint-enable MD060 -->

`position_check_retries` 超限后，Engine 会记录错误，
并停止主动处理该差异，直到差异自行清除。

:::warning

- `open_check_lookback_mins` 不要降低到 60 分钟以下。窗口过短会让订单落在查询范围外，
  产生错误的“订单缺失”处理。
- 如果场所时间戳落后于本地时钟，应提高 `open_check_threshold_ms`，
  避免刚更新的订单被过早判定为缺失。
- 生产环境不要把 `reconciliation_startup_delay_secs` 降到 10 秒以下。
  这段时间用于让系统在启动对账后稳定下来，再开始持续检查。

:::

### 其他执行选项

<!-- markdownlint-disable MD060 -->

| 配置项                                | 默认值     | 说明                                                                |
| ---------------------------------- | ------- | ----------------------------------------------------------------- |
| `allow_overfills`                  | `False` | 允许成交数量超过订单数量，并记录警告；可用于对账与成交竞态。                                    |
| `generate_missing_orders`          | `True`  | 对账时生成 LIMIT 订单以对齐持仓差异，Strategy 为 `EXTERNAL`，标签为 `RECONCILIATION`。 |
| `snapshot_positions_interval_secs` | `None`  | 保存持仓快照的间隔秒数。                                                      |
| `debug`                            | `False` | 启用执行调试日志。                                                         |

<!-- markdownlint-enable MD060 -->

### 内存管理

长期运行或高频交易会持续积累已关闭订单、已关闭持仓和账户事件。
定期清理可以限制内存缓存的增长。

<!-- markdownlint-disable MD060 -->

| 配置项                                    | 默认值    | 说明                      |
| -------------------------------------- | ------ | ----------------------- |
| `purge_closed_orders_interval_mins`    | `None` | 清理已关闭订单的间隔，建议 10–15 分钟。 |
| `purge_closed_orders_buffer_mins`      | `None` | 订单关闭后至少保留的时间，建议 60 分钟。  |
| `purge_closed_positions_interval_mins` | `None` | 清理已关闭持仓的间隔，建议 10–15 分钟。 |
| `purge_closed_positions_buffer_mins`   | `None` | 持仓关闭后至少保留的时间，建议 60 分钟。  |
| `purge_account_events_interval_mins`   | `None` | 清理账户事件的间隔，建议 10–15 分钟。  |
| `purge_account_events_lookback_mins`   | `None` | 账户事件达到多旧才可清理，建议 60 分钟。  |

<!-- markdownlint-enable MD060 -->

设置 Interval 会启用对应清理循环；保持未设置则不调度也不删除。
每个循环最终调用 [Cache](../concepts/cache.md) 中说明的缓存 API。

## Strategy 配置

完整字段参见 [`StrategyConfig` API Reference](/docs/python-api-latest/trading.html#nautilus_trader.trading.StrategyConfig)。

### 标识

<!-- markdownlint-disable MD060 -->

| 配置项            | 默认值    | 说明                               |
| -------------- | ------ | -------------------------------- |
| `strategy_id`  | `None` | 唯一 Strategy ID。                  |
| `order_id_tag` | `None` | 附加到 Strategy 订单 ID 的唯一标签，不能含连字符。 |

<!-- markdownlint-enable MD060 -->

### 订单管理

<!-- markdownlint-disable MD060 -->

| 配置项                         | 默认值     | 说明                                                                          |
| --------------------------- | ------- | --------------------------------------------------------------------------- |
| `oms_type`                  | `None`  | 控制 Position ID 和订单处理的 [OMS 类型](../concepts/execution.md#oms-configuration)。 |
| `use_uuid_client_order_ids` | `False` | Client Order ID 使用 UUID4。                                                   |
| `external_order_claims`     | `None`  | 该 Strategy 认领外部订单及对账活动的 Instrument ID。                                      |
| `manage_contingent_orders`  | `False` | 自动管理 OTO、OCO 和 OUO 条件订单。                                                    |
| `manage_gtd_expiry`         | `False` | 由 Strategy 管理 GTD 订单到期。                                                     |

<!-- markdownlint-enable MD060 -->

运行时通过 `strategy.config` 读取这些设置；Strategy 不会把它们复制成直接属性。

Strategy ID、订单标签和 GTD 行为参见
[Strategies 核心概念](../concepts/strategies.zh-CN.md#多策略实例与订单-id)。

## Windows 信号处理

`LiveNode` 在 Rust 运行循环中处理 Ctrl+C（SIGINT），并在 Unix 上处理 SIGTERM。
Python Bridge 也会把 SIGINT 路由到同一关闭路径，使运行器和任务能够干净退出。

## 生产配置检查清单

- 使用独立脚本或服务运行实盘节点，不使用 Jupyter Notebook。
- 每个进程只运行一个 `LiveNode`，并确保回调不会阻塞事件循环。
- 为交易同一账户的每个节点配置唯一 Trader ID 标签。
- 明确选择 Redis 或 Postgres，并理解二者对自定义状态持久化的支持差异。
- 始终在 `finally` 中调用 `dispose()`，确保缓冲写入和最终状态得到刷新。
- MessageBus 配置必须同时提供对应 Factory；确认外部 Stream 不会回放旧消息。
- 多 Client 节点应明确 Client ID、Venue 路由和默认路由。
- 启动对账后再允许 Strategy 开始交易，并谨慎设置过滤规则。
- 生产环境不要把持续对账的回看窗口和启动延迟降到安全建议以下。
- 根据 API Rate Limit 调整单订单查询上限和查询间隔。
- 长期运行或高频场景应配置适当的内存清理周期和保留窗口。
- Windows 部署应单独验证 Ctrl+C、服务停止和异常退出流程。

## 相关资料

- [仓库英文 Configure a Live Trading Node](configure_live_trading.md)。
- [Live Trading 核心概念](../concepts/live.zh-CN.md)：节点生命周期、事件循环和运行监控。
- [Adapters 核心概念](../concepts/adapters.zh-CN.md)：Client 配置、路由和工具加载。
- [Strategies 核心概念](../concepts/strategies.zh-CN.md)：Strategy 配置和订单管理。
- [Execution reconciliation](../concepts/reconciliation.md)：启动及持续执行对账。
- [Message Bus](../concepts/message_bus.md)：消息总线与外部后端。
- [Cache](../concepts/cache.md)：缓存状态和清理 API。
