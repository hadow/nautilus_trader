# 使用 Rust 编写 Actor（中文版）

本文基于同目录的英文文档 [Write an Actor (Rust)](write_rust_actor.md) 整理翻译。

Actor 接收市场数据、自定义数据、Signal 和系统事件，但不管理订单。
本文逐步构建一个 `SpreadMonitor`：它订阅报价，并记录买卖价差。

Actor、Trait 和 Handler 分发的背景知识参见
[Actors（中文版）](../concepts/actors.zh-CN.md)和
[Rust（中文版）](../concepts/rust.zh-CN.md)。

## 定义结构体

Actor 拥有一个 `DataActorCore`，以及自身需要的其他状态。
Core 保存 Actor 的运行时状态。

用户代码通常通过 `DataActor` 门面方法访问这些状态，例如：

- `clock()`
- `cache()`
- `config()`
- `actor_id()`
- `trader_id()`
- 各种订阅方法。

```rust
use nautilus_common::{nautilus_actor, actor::{DataActor, DataActorConfig, DataActorCore}};
use nautilus_model::{data::QuoteTick, identifiers::{ActorId, InstrumentId}};

pub struct SpreadMonitor {
    core: DataActorCore,
    instrument_id: InstrumentId,
}
```

## 实现构造器

创建包含 Actor ID 的 `DataActorConfig`，然后传给 `DataActorCore::new`。

配置字段使用带默认值的 `Option`，
因此除 Actor ID 外，其余字段都可以通过 `..Default::default()` 填充。

```rust
impl SpreadMonitor {
    pub fn new(instrument_id: InstrumentId) -> Self {
        let config = DataActorConfig {
            actor_id: Some(ActorId::from("SPREAD_MON-001")),
            ..Default::default()
        };
        Self {
            core: DataActorCore::new(config),
            instrument_id,
        }
    }
}
```

## 连接 Core 并实现 Debug

`nautilus_actor!` 宏将 Actor 的 `DataActorCore` 字段连接到运行时约定。
默认情况下，它会委托给名为 `core` 的字段；如果字段名称不同，应传入第二个参数。

普通回调不会调用宏生成的原生访问器。
应在 `self` 上使用 `DataActor` 门面方法。

运行时注册使用通用的 `Actor` 和 `Component` 实现。
宏负责生成原生运行时接线；`Debug` 可以手动实现，也可以使用派生实现。

```rust
nautilus_actor!(SpreadMonitor);

impl std::fmt::Debug for SpreadMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpreadMonitor").finish()
    }
}
```

## 实现 DataActor Trait

重写 Handler 方法以接收数据。
所有 Handler 都提供默认的空操作实现，因此只需重写实际需要的方法。
每个 Handler 都返回 `anyhow::Result<()>`。

```rust
impl DataActor for SpreadMonitor {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_quotes(self.instrument_id, None, None);
        Ok(())
    }

    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let spread = quote.ask_price.as_f64() - quote.bid_price.as_f64();
        log::info!("Spread: {spread:.5}");
        Ok(())
    }
}
```

通过 `DataActor` Trait，可以直接在 `self` 上调用 `subscribe_quotes`。
全部可用 Handler 参见 [Rust：Handler 方法](../concepts/rust.zh-CN.md#handler-方法)。

## 原生运行时访问

默认应使用公开的 `DataActor` 门面。
只有确实需要门面方法无法提供的原生专用访问路径时，才添加 `DataActorNative`。

门面提供以下只读属性：

- `config()`
- `actor_id()`
- `trader_id()`
- `is_registered()`

[Rust：原生 Trait](../concepts/rust.zh-CN.md#原生-trait)介绍原生 Trait 的适用性矩阵，
以及以下方法表：

- [`DataActorNative` 方法](../concepts/rust.zh-CN.md#dataactornative-方法)

这些类型不会跨越 Python 边界，因此需要可移植的 Actor 时，
应使用以下门面方法：

- `clock()`
- `cache()`

## 注册 Actor

向 `BacktestEngine` 注册：

```rust
let actor = SpreadMonitor::new(instrument_id);
engine.add_actor(actor)?;
```

向 `LiveNode` 注册：

```rust
let actor = SpreadMonitor::new(instrument_id);
node.add_actor(actor)?;
```

## Guard 安全

系统向 Actor 分发消息时，会从注册表获取一个短生命周期的 `ActorRef` Guard。
用户不需要直接管理这些 Guard。

如果需要在回调中访问其他 Actor，应遵守以下规则：

- 每次都按 ID 查找 Actor，不要缓存 `ActorRef`。
- 在作用域结束前释放 Guard，绝不能将其保存在字段中。
- 绝不能跨 `.await` 点持有 Guard。

`DataActorCore` 上的订阅方法会正确处理 Guard：
它们捕获 Actor ID，并在回调闭包中执行查找。

完整线程和注册表模型参见
[Runtime invariants](../developer_guide/rust.md#runtime-invariants)。

## 完整示例

更完整的 Actor 示例参见
[`BookImbalanceActor`](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/trading/src/examples/actors/imbalance)。
该 Actor 会追踪每个 Instrument 的状态，并在停止时输出摘要。
