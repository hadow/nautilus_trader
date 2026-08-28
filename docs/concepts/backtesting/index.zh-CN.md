# NautilusTrader Backtesting 核心概念（中文版）

本文基于同目录的英文文档 [Backtesting](index.md) 整理翻译，
作为 NautilusTrader 回测概念系列的中文入口。

回测使用历史数据模拟交易，并复用实盘交易中的核心系统组件：内置引擎、`Cache`、
[MessageBus（中文版）](../message_bus.zh-CN.md)、`Portfolio`、
[Actor（中文版）](../actors.zh-CN.md)、[Strategy（中文版）](../strategies.zh-CN.md)、
[Execution Algorithm](../execution.md) 和用户自定义模块。

`BacktestEngine` 处理历史数据流。
当数据流耗尽后，引擎生成结果和性能指标，供后续分析。

NautilusTrader 提供两个层级的回测 API：

<!-- markdownlint-disable MD060 -->

| API 层级 | 适用场景                              |
| ------ | --------------------------------- |
| 高层     | 使用 `BacktestNode`、配置对象、数据目录和批量运行。 |
| 低层     | 直接控制 `BacktestEngine`，并手动配置组件。    |

<!-- markdownlint-enable MD060 -->

本节各页面介绍当前 Rust 回测引擎及其 Python 包根级 API。

## 阅读顺序

自动生成的侧边栏可能按字母顺序排列页面。
如果希望从头到尾理解回测系统，建议按以下顺序阅读：

<!-- markdownlint-disable MD060 -->

| 步骤  | 页面                                           | 用途                       |
| --- | -------------------------------------------- | ------------------------ |
| 1   | [API 与重复运行](apis-and-runs.zh-CN.md)          | 选择 API 层级、加载数据和批量运行。     |
| 2   | [数据与场所](data-and-venues.zh-CN.md)            | 使数据粒度与场所 `book_type` 匹配。 |
| 3   | [执行流程](execution-flow.zh-CN.md)              | 理解处理顺序、定时器和 Trade ID。    |
| 4   | [成交价格与撮合](fill-prices-and-matching.zh-CN.md) | 理解确定性的撮合行为。              |
| 5   | [基于 Trade 的执行](trade-execution.zh-CN.md)     | 使用 Trade Tick、主动方方向和队列。  |
| 6   | [基于 Bar 的执行](bar-execution.zh-CN.md)         | 使用 K 线、OHLC 顺序和 Bar 时序。  |
| 7   | [成交模型](fill-models.zh-CN.md)                 | 配置滑点和概率成交。               |
| 8   | [账户与保证金](accounts-and-margin.zh-CN.md)       | 配置资金费、余额和保证金模型。          |

<!-- markdownlint-enable MD060 -->

## 相关指南

- [Strategies（中文版）](../strategies.zh-CN.md)：开发用于回测的 Strategy。
- [Visualization](../visualization.md)：根据回测结果生成分析图表。
- [Reports](../reports.md)：分析回测性能数据。
