# Dynamic Grid 审查与参数收敛

审查日期：2026-09-24；基线：`eedfe958db`，NautilusTrader `0.63.0`。
本轮只修复执行语义与重复配置，不增加指标，不提高现有组合风险预算，不连接券商账户。
源码目录：`crates/trading/src/examples/strategies/dynamic_grid/`。

## 已修复的问题

1. **高：目标仓位预留错误**，`position.rs::position_delta`。
   未成交卖单被当成已释放仓位，可能补买超过目标；未成交买单也被用于计算减仓。
   现在买入只扣待买预留，卖出只扣待卖预留，反方向订单必须真实成交后才能使用其结果。
2. **高：目标减仓被止盈单阻塞**，`strategy.rs::reduce_component_to`。
   高位止盈挂单使预计库存变低，减仓逻辑提前返回，实际库存没有下降。
   现在先按真实库存与待买量判断是否超目标，撤销冲突挂单并等待终态，再提交有库存覆盖的减仓单。
3. **中：失效 Quote 改写过滤状态**，`strategy.rs::on_quote`。
   以前在检查时间戳前更新价差；现在只有时间校验通过才更新，有效 Quote 仍正常处理。
4. **中：目标数量没有始终对齐 lot**，`position.rs::target_position`。
   现在对 `max_position` 先向下取整，再分配 Core 与 Grid，避免产生不可交易的目标数量。
5. **中：闲置参数仍影响启动**，`config.rs::validate`。
   固定间距现在仅在 Percentage 模式校验范围；允许 `core_target_pct = 0`，总仓位和资金限制不变。

例如目标与已成交库存都是 100 股、另有 100 股止盈卖单时，旧公式得出再买 100 股；
修复后新增买入为 0。止盈单撤单中或状态未知，都不能提前腾出库存或资金。

修复目标减仓后，下跌行情可能更早实现亏损，成交数、费用和历史收益也可能变化。
这是让执行符合既有目标仓位的正确性修复，不是已证明提高收益的参数优化。

## 参数收敛与迁移

原来的三个组合字段实际都约束同一数值：全部库存市值加未终结买单成本。
其中 `max_total_grid_exposure` 并没有只计算 Grid 仓，`max_total_equity_exposure` 也并未排除待买量。

新配置只需要一个主参数：

```json
{
  "max_total_exposure": "0.70"
}
```

- 默认主上限仍为 60%；仓库当前组合配置仍为 70%，只是删除两个同值字段。
- 旧字段继续接受，实际上限为主参数与所有显式旧字段的最小值，不静默忽略更严格的旧限制。
- 旧字段在 Rust 中改为 `Option<Decimal>`；直接构造旧字段的调用方需使用 `Some(value)`。
- 新配置省略旧字段时，不再暗含另外两个 60% 上限。迁移曾依赖这些默认上限的部分配置时，
  应把原来真正生效的最小值写入主参数，而不是直接保留较大的主参数值。
- 恢复检查点只接受**实际组合上限相同**的字段迁移；上限或其他策略参数改变仍拒绝自动恢复。
- ATR 模式可省略备用 `spacing_pct`；切换 Percentage 时必须显式核对它与上下界。
- `core_target_pct = 0` 是可用能力，不自动改变当前标的配置，也不自动清算历史 Core 库存。

没有合并现金储备、行业暴露、相关性暴露和回撤控制：它们约束不同风险。
也没有把单标的配置中的不同资金分母强行合并。继续收敛这些参数前，应先明确账户、
标的预算和 Core/Grid 的口径，再以相同数据、成本和执行条件验证行为变化。

## 验证

新增回归覆盖：双向挂单预留、数量取整、目标减仓撤单屏障、Unknown 状态预留、
重复驱动不重复减仓、有效/失效 Quote、纯 Grid 配置、旧暴露限制和等价检查点迁移。

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --tests -j2
```

券商恢复测试使用本机测试服务，需要允许回环端口；它不是模拟账户或实盘成交验收。
本轮未执行季度收益对比、参数搜索、Walk-forward 或 OOS，不能据此宣称策略收益改善。

实际验证结果：

- Trading：676 项通过，4 项显式忽略；其中新增 18 个回归用例。
- Longbridge：42 项通过，包含本机服务模拟超时、对账和成交去重的既有测试。
- debug 回测程序构建通过；未运行 release 构建。
- Clippy：未通过，13 个诊断位于未修改代码，涉及 `momentum_pullback` 和 `dynamic_grid/files.rs`。
  没有通过关闭 lint 或删除测试来掩盖它们；未完成全仓检查。
- AAPL/MSFT 短回放：2025-01-02 至 2025-01-06，2,340 根 Bar，0 条真实 Quote。
  资金、费用、滑点及标的参数沿用当前配置，只有日期缩短。
  加载 1.02 秒、事件回放 2.32 秒、收尾 0.05 秒、写报告 0.08 秒，总计 3.48 秒。
  4 笔成交、0 个完成周期，费用 13.68 美元，期末净盈亏 19.92 美元。
  该结果仅是执行链路冒烟验证，不能作为收益、网格套利或性能提升的证据。

短回放配置与结果位于本机 `/tmp/dynamic-grid-review-smoke.json` 和
`/tmp/dynamic-grid-review-smoke-report.json`，日志为 `/tmp/dynamic-grid-review-smoke.log`。
正式研究请保存固定数据及配置快照，不能依赖临时文件长期留存。

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --dynamic-only \
  /tmp/dynamic-grid-review-smoke.json /tmp/dynamic-grid-review-smoke-report.json
```

## 部署边界

仍使用同一策略核心和原有 NautilusTrader / Longbridge 执行链路，没有新增订单管理框架。
更新二进制前应正常停止旧进程并保留检查点；不能让两个进程同时管理同一账户。
可先使用小规模 Paper 回放确认目标减仓成交、费用与库存变化，再决定实盘升级。
