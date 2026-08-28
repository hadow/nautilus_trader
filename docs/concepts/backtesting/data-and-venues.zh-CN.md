# 回测数据与场所（中文版）

本文基于同目录的英文文档
[Backtest Data and Venues](data-and-venues.md) 整理翻译。

## 数据

历史数据会推进回测时钟、更新模拟市场状态，并触发 Strategy 回调。
场所的 `book_type` 决定哪些数据可以更新撮合订单簿，
因此场所配置必须与可用数据匹配。

订单簿数据比报价、成交或 K 线提供更多执行细节，
但即使是记录下来的订单簿，也无法显示模拟订单原本会如何改变市场。

NautilusTrader 支持以下数据，按细节粒度从高到低排列：

```mermaid
flowchart LR
    L3["L3 订单簿<br/>逐笔委托"]
    L2["L2 订单簿<br/>逐价聚合"]
    L1["L1 报价<br/>最优买卖价"]
    T["成交"]
    B["K 线"]

    L3 --> L2 --> L1 --> T --> B

    style L3 fill:#2d5a3d,color:#fff
    style L2 fill:#3d6a4d,color:#fff
    style L1 fill:#4d7a5d,color:#fff
    style T fill:#5d8a6d,color:#fff
    style B fill:#6d9a7d,color:#fff
```

粒度越高，能够看到的历史队列和深度信息越多；
粒度越低，执行模拟所需的假设就越多。

- **L3 订单簿数据（market-by-order）：** 记录每个价格档位的单笔订单。
- **L2 订单簿数据（market-by-price）：** 记录每个价格档位的聚合数量。
- **L1 报价 Tick（market-by-price）：** 记录最优买卖价格和数量。
- **Trade Tick：** 记录已经发生的成交。
- **Bar：** 记录固定时间段内聚合的价格和成交量。

### 选择数据：成本与准确性

在 Strategy 开发早期，K 线数据可能已经足够，
而且通常比 Tick 或订单簿数据更便宜、更容易获得。

但 K 线无法确定区间内价格的先后顺序、价差、深度或队列位置。
因此，对执行行为敏感的 Strategy 需要使用粒度更高的数据进行验证。

:::tip
条件允许时，可以先用 K 线检验核心信号。
如果结果依赖价差、精确的区间内顺序、紧密止盈止损或队列位置，
则应在依赖结果前改用报价、成交或深度数据验证。
:::

## 场所

初始化回测场所时，必须从以下选项中指定用于执行处理的内部订单 `book_type`：

- `L1_MBP`：Level 1 market-by-price，默认值，只维护订单簿最优档位。
- `L2_MBP`：Level 2 market-by-price，维护订单簿深度，每个价格档位聚合为一笔订单。
- `L3_MBO`：Level 3 market-by-order，维护订单簿深度，按数据提供的形式追踪每一笔订单。

`book_type` 决定哪些数据能够更新订单簿状态并驱动撮合。
不适用于所选订单簿的数据会在订单簿和价格更新路径中被忽略，
但外层回测时钟仍会继续推进。

Strategy 仍会通过 DataEngine 接收已订阅的数据。

精度校验取决于撮合路径。
例如，如果某根 Bar 被 L2 或 L3 场所忽略，处理会在可执行 Bar 的精度校验前返回。

<!-- markdownlint-disable MD060 -->

| 数据类型               | L1_MBP | L2_MBP | L3_MBO |
| ------------------ | ------ | ------ | ------ |
| `QuoteTick`        | 更新订单簿  | *忽略*   | *忽略*   |
| `TradeTick`        | 触发撮合   | 触发撮合   | 触发撮合   |
| `Bar`              | 更新订单簿  | *忽略*   | *忽略*   |
| `OrderBookDelta`   | *忽略*   | 更新订单簿  | 更新订单簿  |
| `OrderBookDeltas`  | *忽略*   | 更新订单簿  | 更新订单簿  |
| `OrderBookDepth10` | 更新订单簿  | 更新订单簿  | 更新订单簿  |

<!-- markdownlint-enable MD060 -->

:::note
数据粒度必须与指定的订单 `book_type` 匹配。
NautilusTrader 无法根据报价、成交或 K 线等低粒度数据生成 L2 或 L3 高粒度数据。
:::

:::warning
如果场所 `book_type` 使用 `L2_MBP` 或 `L3_MBO`，报价和 K 线不会更新订单簿。
必须提供订单簿增量数据，否则订单可能看起来始终无法成交。
:::

:::warning
使用默认的 `L1_MBP` 时，撮合引擎会忽略订单簿增量。
如果订阅订单簿增量，应将场所 `book_type` 设为 `L2_MBP` 或 `L3_MBO`。

这一规则也适用于 Sandbox 执行，因为其中的撮合引擎使用相同的 `book_type` 配置。
:::
