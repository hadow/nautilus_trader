# 使用 Rust 运行回测（中文版）

本文基于同目录的英文文档 [Run a Backtest (Rust)](run_rust_backtest.md) 整理翻译。
它保留原有依赖版本、API、配置字段和命令，重点说明如何使用 Rust 完成低层与高层两种回测流程。

NautilusTrader 提供两套 Rust 回测 API：

- `BacktestEngine`：低层 API，直接控制引擎、场所、Instrument 和内存数据。
- `BacktestNode`：高层 API，支持从数据目录以流式方式加载数据。

本文同时介绍两种用法。

回测概念、成交模型和撮合引擎行为参见 [Backtesting](../concepts/backtesting/)。
工程配置和 Feature flag 参见 [Rust：工程配置](../concepts/rust.zh-CN.md#工程配置)。

## 依赖

将以下依赖加入 `Cargo.toml`。
`streaming` 和 `nautilus-persistence` 仅在使用高层 `BacktestNode` API 时需要。

```toml
[dependencies]
nautilus-backtest = { version = "0.62", features = ["streaming"] }
nautilus-execution = "0.62"
nautilus-model = { version = "0.62", features = ["stubs"] }
nautilus-persistence = "0.62"
nautilus-trading = { version = "0.62", features = ["examples"] }

ahash = "0.8"
anyhow = "1"
tempfile = "3"
ustr = "1"
```

如果只需要低层 `BacktestEngine`，可以移除 `streaming`、
`nautilus-persistence`、`tempfile` 和 `ustr`。

## `BacktestEngine`：低层 API

低层 API 提供直接控制能力：创建引擎，添加场所和 Instrument，
把数据加载到内存，注册 Strategy，然后运行回测。

### 1. 创建引擎

```rust
use nautilus_backtest::{config::BacktestEngineConfig, engine::BacktestEngine};

let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
```

### 2. 添加场所

`SimulatedVenueConfig` 使用 `bon::Builder`。
只需设置必填字段，其他设置均使用文档中说明的默认值。

`build()` 会校验配置并返回 `ConfigResult`，因此需要传播该结果或将其解包。

```rust
use nautilus_backtest::config::SimulatedVenueConfig;
use nautilus_model::{
    enums::{AccountType, BookType, OmsType},
    identifiers::Venue,
    types::Money,
};

engine.add_venue(
    SimulatedVenueConfig::builder()
        .venue(Venue::from("SIM"))
        .oms_type(OmsType::Hedging)
        .account_type(AccountType::Margin)
        .book_type(BookType::L1_MBP)
        .starting_balances(vec![Money::from("1_000_000 USD")])
        .build()?,
)?;
```

可以继续链式调用 Setter 覆盖任意默认值，例如
`.reject_stop_orders(false)` 或 `.allow_cash_borrowing(true)`。

### 3. 添加 Instrument 和数据

```rust
use nautilus_model::instruments::{
    Instrument, InstrumentAny, stubs::audusd_sim,
};

let instrument = InstrumentAny::CurrencyPair(audusd_sim());
let instrument_id = instrument.id();
engine.add_instrument(&instrument)?;

let quotes = generate_quotes(instrument_id); // 用户自己的数据加载函数
engine.add_data(quotes, None, true, true)?;
```

### 4. 注册 Strategy 并运行

```rust
use nautilus_model::types::Quantity;
use nautilus_trading::examples::strategies::EmaCross;

let strategy = EmaCross::new(
    instrument_id,
    Quantity::from("100000"),
    10, // 快速 EMA 周期
    20, // 慢速 EMA 周期
);

engine.add_strategy(strategy)?;
engine.run(None, None, None, false)?;
```

### 运行完整示例

```bash
cargo run -p nautilus-backtest --features examples --example engine-ema-cross
```

源文件：
[`crates/backtest/examples/engine_ema_cross.rs`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/backtest/examples/engine_ema_cross.rs)

## `BacktestNode`：高层 API

高层 API 从 `ParquetDataCatalog` 加载数据，并按照可配置的分块大小进行流式处理。
使用它需要为 `nautilus-backtest` 启用 `streaming` Feature。

### 1. 将数据写入目录

```rust
use nautilus_model::instruments::{
    Instrument, InstrumentAny, stubs::audusd_sim,
};
use nautilus_persistence::backend::catalog::ParquetDataCatalog;
use tempfile::TempDir;

let instrument = InstrumentAny::CurrencyPair(audusd_sim());
let instrument_id = instrument.id();
let quotes = generate_quotes(instrument_id);

let temp_dir = TempDir::new()?;
let catalog_path = temp_dir.path().to_str()
    .context("temp dir path is not valid UTF-8")?
    .to_string();
let catalog = ParquetDataCatalog::new(
    temp_dir.path(), None, None, None, None,
);

catalog.write_instruments(vec![instrument])?;
catalog.write_to_parquet(&quotes, None, None, None)?;
```

### 2. 配置回测运行

```rust
use nautilus_backtest::config::{
    BacktestDataConfig, BacktestRunConfig, BacktestVenueConfig, NautilusDataType,
};
use nautilus_model::enums::{AccountType, BookType, OmsType};

let venue_config = BacktestVenueConfig::builder()
    .name("SIM")
    .oms_type(OmsType::Hedging)
    .account_type(AccountType::Margin)
    .book_type(BookType::L1_MBP)
    .starting_balances(vec!["1_000_000 USD".to_string()])
    .build()?;

let data_config = BacktestDataConfig::builder()
    .data_type(NautilusDataType::QuoteTick)
    .catalog_path(catalog_path)
    .instrument_id(instrument_id)
    .build()?;

let run_config = BacktestRunConfig::builder()
    .id("ema-cross-run".to_string())
    .venues(vec![venue_config])
    .data(vec![data_config])
    .chunk_size(100)
    .build()?;
```

### 3. 构建节点、添加 Strategy 并运行

```rust
use nautilus_backtest::node::BacktestNode;
use nautilus_model::types::Quantity;
use nautilus_trading::examples::strategies::EmaCross;

let mut node = BacktestNode::new(vec![run_config])?;
node.build()?;

let engine = node.get_engine_mut("ema-cross-run")
    .context("engine not found for run config ID")?;
let strategy = EmaCross::new(
    instrument_id,
    Quantity::from("100000"),
    10,
    20,
);
engine.add_strategy(strategy)?;

node.run()?;
```

### 运行完整示例

```bash
cargo run -p nautilus-backtest --features examples,streaming --example node-ema-cross
```

源文件：
[`crates/backtest/examples/node_ema_cross.rs`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/backtest/examples/node_ema_cross.rs)
