# Rust 核心概念（中文版）

本文基于同目录的英文文档
[`rust.md`](rust.md) 和
[官网最新版 Rust 概念页](https://nautilustrader.io/docs/latest/concepts/rust/)
整理翻译。本文保留 Rust 类型、trait、crate、宏和方法的原始名称，并用中文解释它们在
NautilusTrader 中的职责与使用边界。

NautilusTrader 在 `crates/` 目录下提供完整的 Rust 实现。你可以只使用 Rust 编写 Actor、
Strategy，运行回测并连接实盘，无需 Python 运行时。

Python 包与 Rust 使用同一个领域模型。Python 用户组件通过 PyO3 运行在相同的 Rust 交易引擎上，
因此两条开发路径共享核心数据类型和执行语义。

:::warning
Rust API 仍在积极开发中。不同版本之间的方法签名和 trait 约束可能发生变化。
:::

## 系统实现路径

NautilusTrader 提供两种系统实现路径。应根据组件编写方式、部署环境和性能目标进行选择。

- **Rust**：直接使用 `crates/` 下的纯 Rust 实现，不依赖 Python。
- **Python**：在 `python/nautilus_trader/` 下编写 Python 用户组件，通过 PyO3 使用 Rust 核心。

### 能力对照

| 组件              | Rust | Python |
| --------------- | ---- | ------ |
| Strategy        | ✓    | ✓      |
| Actor           | ✓    | ✓      |
| DataEngine      | ✓    | ✓      |
| ExecutionEngine | ✓    | ✓      |
| RiskEngine      | ✓    | ✓      |
| BacktestEngine  | ✓    | ✓      |
| BacktestNode    | ✓    | ✓      |
| LiveNode        | ✓    | ✓      |
| OrderEmulator   | ✓    | ✓      |
| 撮合引擎            | ✓    | ✓      |
| Portfolio       | ✓    | ✓      |
| 账户              | ✓    | ✓      |
| Cache           | ✓    | ✓      |
| MessageBus      | ✓    | ✓      |
| 数据目录            | ✓    | ✓      |
| 技术指标            | ✓    | ✓      |
| 执行算法            | TWAP | TWAP   |
| Controller      | -    | ✓      |
| 绩效分析报告          | -    | ✓      |

:::note
`Controller` 的运行时由 Rust 实现，并为 Python 的 `Controller` 基类提供支持。
表中 Rust 一栏标记为不支持，是因为当前受支持的可导入 Controller 配置注册路径仅面向 Python。
:::

### Adapter 支持

| Adapter             | Rust | Python |
| ------------------- | ---- | ------ |
| Architect AX        | ✓    | ✓      |
| Betfair             | ✓    | ✓      |
| Binance             | ✓    | ✓      |
| BitMEX              | ✓    | ✓      |
| Blockchain          | ✓    | ✓      |
| Bybit               | ✓    | ✓      |
| Coinbase            | ✓    | ✓      |
| Databento           | ✓    | ✓      |
| Deribit             | ✓    | ✓      |
| Derive              | ✓    | ✓      |
| dYdX                | ✓    | ✓      |
| Hyperliquid         | ✓    | ✓      |
| Interactive Brokers | ✓    | ✓      |
| Kraken              | ✓    | ✓      |
| Lighter             | ✓    | ✓      |
| OKX                 | ✓    | ✓      |
| Polymarket          | ✓    | ✓      |
| Sandbox             | ✓    | ✓      |
| Tardis              | ✓    | ✓      |

### 如何选择

- **Rust 路径**不需要 Python 运行时，并能使用全部核心交易功能。它适合延迟敏感型部署，
  以及希望采用编译型语言的团队。
- **Python 路径**保留 Python 的开发体验。Actor 和 Strategy 等用户组件仍由 Rust 核心完成
  数据处理与执行。

选择 Rust 并不意味着使用另一套交易引擎。两条路径的主要差异是用户组件和应用组合层使用的语言，
而不是底层领域模型或核心引擎。

## 工程配置

NautilusTrader 的 Rust crate 已发布到
[crates.io](https://crates.io/crates/nautilus-backtest)。可以在项目的 `Cargo.toml` 中添加：

```toml
[dependencies]
nautilus-backtest = "0.62"
nautilus-common = "0.62"
nautilus-execution = "0.62"
nautilus-model = { version = "0.62", features = ["stubs"] }
nautilus-trading = { version = "0.62", features = ["examples"] }

anyhow = "1"
log = "0.4"
```

实盘交易还需要添加 `nautilus-live` 和对应交易场所的 adapter：

```toml
[dependencies]
nautilus-live = "0.62"
nautilus-okx = "0.62"
```

依赖版本应与实际使用的 NautilusTrader release 保持一致。不要在同一程序中混用来自不同版本的
Nautilus crate，否则相同名称的领域类型也可能因为 crate 版本不同而无法互操作。

如需跟踪最新的 `develop` 分支，应让所有 Nautilus 依赖指向同一个 Git 来源：

```toml
[dependencies]
nautilus-backtest = { git = "https://github.com/nautechsystems/nautilus_trader.git", branch = "develop" }
nautilus-common = { git = "https://github.com/nautechsystems/nautilus_trader.git", branch = "develop" }
nautilus-execution = { git = "https://github.com/nautechsystems/nautilus_trader.git", branch = "develop" }
nautilus-model = { git = "https://github.com/nautechsystems/nautilus_trader.git", branch = "develop", features = ["stubs"] }
nautilus-trading = { git = "https://github.com/nautechsystems/nautilus_trader.git", branch = "develop", features = ["examples"] }
```

当前仓库标注的最低支持 Rust 版本（MSRV）是 **1.98.0**。

### Feature flag

| Flag             | Crate               | 作用                                            |
| ---------------- | ------------------- | --------------------------------------------- |
| `high-precision` | `nautilus-model`    | 使用 16 位定点精度，默认是 9 位；加密资产通常需要启用。               |
| `stubs`          | `nautilus-model`    | 提供测试工具桩，例如 `audusd_sim`。                      |
| `examples`       | `nautilus-trading`  | 提供 `EmaCross`、`GridMarketMaker` 等示例 Strategy。 |
| `streaming`      | `nautilus-backtest` | 允许 `BacktestNode` 从数据目录流式读取数据。                |
| `defi`           | `nautilus-model`    | 提供 DeFi 数据类型，并隐含启用 `high-precision`。          |

:::tip
标准 9 位精度可以处理多数传统金融工具。加密资产价格可能包含较多小数位，
例如 `0.00000001`，此时应启用 `high-precision`。
:::

### 内存分配器

`nautilus` CLI 和官方 Python wheel 使用
[mimalloc](https://crates.io/crates/mimalloc) 处理 Rust 内存分配。
Rust 二进制需要自行选择全局分配器；如需保持一致，可添加：

```toml
[dependencies]
mimalloc = "0.1"
```

```rust
use mimalloc::MiMalloc;
use nautilus_common::logging::headers::register_allocator_mimalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    register_allocator_mimalloc();
}
```

声明 `GLOBAL` 才会真正选择 mimalloc。应在构造 Nautilus 节点之前，于 `main` 开始处调用
`register_allocator_mimalloc`，这样版本日志会显示 `allocator: mimalloc <version>`。

注册函数只更新版本头元数据，并不会切换分配器。真正决定分配器的是 `#[global_allocator]`。

默认系统分配器也能工作，但回测吞吐量可能明显下降。在 Windows 上，分配开销甚至可能占据
热点循环运行时间的一半。背景说明参见[中文版架构文档](architecture.zh-CN.md#内存分配)。

## Actor

Actor 接收市场数据、自定义数据或信号，以及系统事件，但不负责管理订单。

Rust Actor 需要实现 `DataActor` trait，并使用 `nautilus_actor!` 宏将结构体中的
`DataActorCore` 字段接入运行时契约。Actor 类型本身需要实现或派生 `Debug`。

宏负责生成原生运行时接线。一般用户代码通过 `DataActor` 的门面方法完成数据订阅、
缓存访问和时钟访问，不需要直接操作内部 core。

### Handler 方法

在 `DataActor` trait 上覆盖相应 handler，即可接收对应的数据或事件。
所有 handler 默认都是空实现，因此只需覆盖实际需要的方法。

| Handler                | 接收内容                  |
| ---------------------- | --------------------- |
| `on_start`             | Actor 启动事件。           |
| `on_stop`              | Actor 停止事件。           |
| `on_quote`             | `QuoteTick`           |
| `on_trade`             | `TradeTick`           |
| `on_bar`               | `Bar`                 |
| `on_book_deltas`       | `OrderBookDeltas`     |
| `on_book`              | 按指定间隔生成的 `OrderBook`。 |
| `on_instrument`        | `InstrumentAny`       |
| `on_mark_price`        | `MarkPriceUpdate`     |
| `on_index_price`       | `IndexPriceUpdate`    |
| `on_funding_rate`      | `FundingRateUpdate`   |
| `on_option_greeks`     | `OptionGreeks`        |
| `on_option_chain`      | `OptionChainSlice`    |
| `on_instrument_status` | `InstrumentStatus`    |
| `on_time_event`        | `TimeEvent`           |

分步实现方法参见
[Write an Actor (Rust)](../how_to/write_rust_actor.md)。完整示例参见
[`BookImbalanceActor`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/trading/src/examples/actors/imbalance)。

## Strategy

Strategy 在 Actor 的数据处理能力之上增加订单管理能力。

Strategy 需要实现 `DataActor` 以处理数据，并使用 `nautilus_strategy!` 宏把
`StrategyCore` 字段接入 Strategy 运行时契约。`StrategyCore` 保存运行时状态，
普通策略逻辑通过 `self` 上的门面方法访问这些能力。

运行时注册依赖宏生成的原生接线，但日常策略代码应使用 `Strategy` 和 `self` 上的门面方法。
Strategy 还可以覆盖 `on_order_filled`、`on_order_canceled` 等订单事件 handler。

### 订单管理

`Strategy` trait 通过门面提供以下订单方法：

| 方法                    | 操作             |
| --------------------- | -------------- |
| `submit_order`        | 向交易场所提交新订单。    |
| `submit_order_list`   | 提交一组有关联关系的订单。  |
| `modify_order`        | 修改价格、数量或触发价格。  |
| `cancel_order`        | 撤销指定订单。        |
| `cancel_orders`       | 撤销满足过滤条件的一组订单。 |
| `cancel_all_orders`   | 撤销某一金融工具的所有订单。 |
| `close_position`      | 使用市价单关闭一个持仓。   |
| `close_all_positions` | 关闭全部未平持仓。      |

通过 `self.order()` 获得的 `OrderApi` 用于构造订单和订单列表：

- `generate_client_order_id`
- `generate_order_list_id`
- `market`
- `limit`
- `stop_market`
- `stop_limit`
- `market_to_limit`
- `market_if_touched`
- `limit_if_touched`
- `trailing_stop_market`
- `trailing_stop_limit`
- `bracket`
- `create_list`

### Core 接线宏

Rust Actor、Strategy 和执行算法都把运行时 core 保存为结构体字段。
对应宏告诉 trait 这个字段位于何处，并生成所需的运行时接线。

| 宏                                              | Core 字段                  | 生成内容                  |
| ---------------------------------------------- | ------------------------ | --------------------- |
| `nautilus_actor!(Type)`                        | `DataActorCore`          | Actor 运行时接线。          |
| `nautilus_strategy!(Type)`                     | `StrategyCore`           | 运行时接线和 `Strategy` 实现。 |
| `nautilus_execution_algorithm!(Type, { ... })` | `ExecutionAlgorithmCore` | 运行时接线和执行算法实现。         |

宏默认寻找名为 `core` 的字段。如字段名称不同，应把字段名称作为第二个参数传入。

这些宏不会让 Actor、Strategy 或 `StrategyCore` 自动解引用到运行时内部对象。
执行算法宏还接收 `on_order()` 实现块，因为该方法定义算法必须提供的订单处理逻辑。

### 原生 trait

普通组件代码应优先使用以下门面方法：

- `actor_id()`
- `trader_id()`
- `is_registered()`
- `config()`
- `strategy_id()`
- `clock()`
- `cache()`
- `order()`
- `portfolio()`

`DataActorNative`、`StrategyNative` 和 `ExecutionAlgorithmNative` 提供门面层以下的原生访问。
它们面向引擎、运行时接线和明确的延迟敏感型 Rust 代码，不是普通可移植组件的默认接口。

| 组件编写路径            | 是否使用原生 trait | 常规 API                       |
| ----------------- | ------------ | ---------------------------- |
| 原生 Rust 二进制       | 仅在确有需要时      | `Strategy` 和 `DataActor` 门面。 |
| 从 Python 启动的 Rust | 仅在确有需要时      | 与原生 Rust 相同。                 |
| 使用 Python 编写的组件   | 不使用          | 仅使用门面。                       |

原生 trait 会暴露借用的 core 状态、`Rc<RefCell<_>>` 和运行时引用。
只有当同一二进制中的 Rust 代码明确接受这些借用约束，并确实需要低层访问时才应使用。

引擎、运行时、注册、PyO3 和 testkit 代码可以按需导入这些 trait。
普通 Actor、Strategy、执行算法逻辑以及 Python 组件不应依赖它们，因为这些类型不会跨越 Python 边界。

`ExecutionAlgorithmCore` 拥有一个 `DataActorCore`，但不会自动解引用到它。
普通执行算法逻辑应使用 `id()`、`actor_id()`、`trader_id()`、`clock()` 和 `cache()`。

只有需要原生执行算法状态时才使用 `ExecutionAlgorithmNative`。选择能够完成任务的最小原生句柄，
并尽量缩短每次借用的作用域。

普通策略应通过 `order()` 构造订单。只有原生代码确实需要直接可变借用订单工厂时，
才使用 `order_factory()`。

#### `DataActorNative` 方法

| 原生方法          | 返回类型                     | 适用场景           |
| ------------- | ------------------------ | -------------- |
| `core()`      | `&DataActorCore`         | 读取 Actor 内部状态。 |
| `core_mut()`  | `&mut DataActorCore`     | 修改 Actor 内部状态。 |
| `clock_mut()` | `RefMut<'_, dyn Clock>`  | 需要时钟的可变借用。     |
| `clock_rc()`  | `Rc<RefCell<dyn Clock>>` | 需要保存或传递共享时钟。   |
| `cache_ref()` | `Ref<'_, Cache>`         | 需要短期读取实时缓存。    |
| `cache_rc()`  | `Rc<RefCell<Cache>>`     | 需要修改、保存或传递缓存。  |

#### `StrategyNative` 方法

| 原生方法                  | 返回类型                        | 适用场景               |
| --------------------- | --------------------------- | ------------------ |
| `strategy_core()`     | `&StrategyCore`             | 读取 Strategy 内部状态。  |
| `strategy_core_mut()` | `&mut StrategyCore`         | 修改 Strategy 内部状态。  |
| `order_factory()`     | `RefMut<'_, OrderFactory>`  | 需要直接可变借用订单工厂。      |
| `order_factory_rc()`  | `Rc<RefCell<OrderFactory>>` | 需要保存或传递订单工厂。       |
| `portfolio_rc()`      | `Rc<RefCell<Portfolio>>`    | 需要保存或传递 Portfolio。 |

#### `ExecutionAlgorithmNative` 方法

| 原生方法                        | 返回类型                          | 适用场景        |
| --------------------------- | ----------------------------- | ----------- |
| `exec_algorithm_core()`     | `&ExecutionAlgorithmCore`     | 读取执行算法内部状态。 |
| `exec_algorithm_core_mut()` | `&mut ExecutionAlgorithmCore` | 修改执行算法内部状态。 |

分步实现方法参见
[Write a Strategy (Rust)](../how_to/write_rust_strategy.md)。完整示例参见
[`EmaCross`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/trading/src/examples/strategies/ema_cross)
和
[`GridMarketMaker`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/trading/src/examples/strategies/grid_mm)。

### 运行 Rust 组件

Rust Strategy 和 Actor 有两种运行路径。下面使用 Strategy 说明；捆绑 Actor 的对应方法是
纯 Rust 的 `add_actor` 和 Python 的 `add_builtin_actor`。

#### 纯 Rust

使用 Rust 编写 Strategy 和 `main` 函数，然后通过 `cargo build` 构建独立二进制。
这条路径不需要 Python 运行时。

```rust
let strategy = GridMarketMaker::new(config);
node.add_strategy(strategy)?;
node.run().await?;
```

完整流程参见
[Run Live Trading (Rust)](../how_to/run_rust_live_trading.md)。

#### 从 Python 运行内置示例

可以向 `add_builtin_strategy` 传入类型名称和配置，从 Python 注册一个内置示例 Strategy。

这条路径用于让 Rust 与 Python 文档、示例和测试共享同一份捆绑示例代码。
它不是添加自定义原生 Strategy 的通用扩展机制；自定义原生组件应使用纯 Rust 路径。

```python
from nautilus_trader.trading import GridMarketMakerConfig

config = GridMarketMakerConfig(
    instrument_id=InstrumentId.from_str("BTC-USDT-SWAP.OKX"),
    max_position=Quantity.from_str("10.0"),
    trade_size=Quantity.from_str("0.1"),
    num_levels=5,
    grid_step_bps=15,
)

node.add_builtin_strategy("GridMarketMaker", config)
```

内置 Strategy 配置如下：

| Config                       | Strategy               |
| ---------------------------- | ---------------------- |
| `CompositeMarketMakerConfig` | `CompositeMarketMaker` |
| `DeltaNeutralVolConfig`      | `DeltaNeutralVol`      |
| `EmaCrossConfig`             | `EmaCross`             |
| `ExecTesterConfig`           | `ExecTester`           |
| `GridMarketMakerConfig`      | `GridMarketMaker`      |
| `HurstVpinDirectionalConfig` | `HurstVpinDirectional` |

`add_builtin_actor` 对 Actor 采用相同的“仅限捆绑组件”规则。

| Config                     | Actor                |
| -------------------------- | -------------------- |
| `BookImbalanceActorConfig` | `BookImbalanceActor` |
| `DataTesterConfig`         | `DataTester`         |

## 回测

Rust 提供两级回测 API。完整的注释式流程参见
[Run a Backtest (Rust)](../how_to/run_rust_backtest.md)。

### `BacktestEngine`：低层 API

低层 API 允许直接控制回测引擎。调用方负责构造引擎、添加交易场所和金融工具、加载数据、
注册 Strategy，然后启动回测。

运行完整示例：

```bash
cargo run -p nautilus-backtest --features examples --example engine-ema-cross
```

源码：
[`crates/backtest/examples/engine_ema_cross.rs`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/backtest/examples/engine_ema_cross.rs)

### `BacktestNode`：高层 API

高层 API 从 `ParquetDataCatalog` 加载数据，并支持按可配置的块大小流式读取。
使用它需要为 `nautilus-backtest` 启用 `streaming` feature。

运行完整示例：

```bash
cargo run -p nautilus-backtest --features examples,streaming --example node-ema-cross
```

源码：
[`crates/backtest/examples/node_ema_cross.rs`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/backtest/examples/node_ema_cross.rs)

两级 API 使用相同的领域模型与核心组件。`BacktestEngine` 适合需要手动控制组件装配的场景；
`BacktestNode` 适合配置驱动、数据目录和批量运行工作流。

## 实盘交易

完整流程参见
[Run Live Trading (Rust)](../how_to/run_rust_live_trading.md)。

`LiveNode` 通过 adapter client 连接真实交易场所和数据源。Builder 模式负责配置数据与执行客户端，
随后由 `run()` 启动异步事件循环。每个 adapter 都提供自己的 factory 和 config 类型。

| Adapter             | 示例目录                                            |
| ------------------- | ----------------------------------------------- |
| Architect AX        | `crates/adapters/architect_ax/examples/`        |
| Betfair             | `crates/adapters/betfair/examples/`             |
| Binance             | `crates/adapters/binance/examples/`             |
| BitMEX              | `crates/adapters/bitmex/examples/`              |
| Blockchain          | `crates/adapters/blockchain/examples/`          |
| Bybit               | `crates/adapters/bybit/examples/`               |
| Coinbase            | `crates/adapters/coinbase/examples/`            |
| Databento           | `crates/adapters/databento/examples/`           |
| Deribit             | `crates/adapters/deribit/examples/`             |
| Derive              | `crates/adapters/derive/examples/`              |
| dYdX                | `crates/adapters/dydx/examples/`                |
| Hyperliquid         | `crates/adapters/hyperliquid/examples/`         |
| Interactive Brokers | `crates/adapters/interactive_brokers/examples/` |
| Kraken              | `crates/adapters/kraken/examples/`              |
| Lighter             | `crates/adapters/lighter/examples/`             |
| OKX                 | `crates/adapters/okx/examples/`                 |
| Polymarket          | `crates/adapters/polymarket/examples/`          |
| Sandbox             | `crates/adapters/sandbox/examples/`             |
| Tardis              | `crates/adapters/tardis/examples/`              |

多数 adapter 包含 `node_data_tester.rs` 和 `node_exec_tester.rs` 示例，
用于针对真实交易场所验证数据请求、实时推送和订单执行。

:::danger
实盘交易涉及真实资金风险。运行执行示例前，应确认账户、环境、交易场所、订单数量和风控配置。
:::

## 核心认识

- Rust 与 Python 路径共享交易核心，主要区别在用户组件和应用组合层。
- Actor 负责事件和数据处理；Strategy 在 Actor 基础上增加订单管理。
- 宏负责把组件 core 接入运行时，但普通逻辑应使用稳定的门面方法。
- 原生 trait 面向内部接线和明确的低延迟需求，不应成为普通策略代码的默认依赖。
- `BacktestEngine` 提供手动装配，`BacktestNode` 提供配置驱动和流式数据工作流。
- `LiveNode` 使用 adapter factory 连接真实数据与执行客户端。
- 所有 Nautilus crate 应保持版本和来源一致，避免领域类型不兼容。

## 延伸阅读

- [Python](python.md)：Python 所有权、运行时和公共 API 边界。
- [Write an Actor (Rust)](../how_to/write_rust_actor.md)：Rust Actor 分步实现。
- [Write a Strategy (Rust)](../how_to/write_rust_strategy.md)：Rust Strategy 分步实现。
- [Run a Backtest (Rust)](../how_to/run_rust_backtest.md)：`BacktestEngine` 和 `BacktestNode`。
- [Run Live Trading (Rust)](../how_to/run_rust_live_trading.md)：`LiveNode` 与场所连接。
- [Architecture 中文版](architecture.zh-CN.md)：系统设计以及数据与执行流。
- [Actors](actors.md)：适用于 Rust 和 Python 的 Actor 概念。
- [Strategies](strategies.md)：Strategy 概念和 handler 参考。
- [Events](events/)：事件类型与 handler 分发。
- [Backtesting](backtesting/)：回测概念与撮合引擎行为。
