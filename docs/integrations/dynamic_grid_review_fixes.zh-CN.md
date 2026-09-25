# Dynamic Grid 深度 review 修复记录

2026-09-25。范围是已有 Rust 核心的安全修复与冗余计算收敛，不进行参数搜索。
保留 15 分钟分类、8 根确认、原风险阈值及独立标的配置；未连接券商或部署账户。

## 按实施顺序落地

1. **Flatten 覆盖 Core 与 Grid。** 正常网格止盈仍只管理 Grid；强制清仓同时遍历 Core。
   两类退出共用现有未预留库存查询；部分成交、CancelPending、Unknown 不释放尚存卖单预留。
2. **消除可验证的现金双重预留。** 单标的与组合统一复用 `reservation_overlap`。
   仅当原生 CashAccount 计算账户状态，且分标的锁款之和与当前账户锁款一致时，抵扣同标的、同币种、
   不超过本地 pending 的重叠部分。之后仍统一扣除全部本地 pending。
   券商汇总冻结额不能证明订单归属；Longbridge 的未知冻结/待结算款仍不释放。
3. **缩小目标减仓的撤单范围。** 先撤超出目标的待买量，保留现有止盈。
   实际库存超过目标时，先使用未被预留的库存，再只撤足够释放减仓数量的限价卖单。
   优先等待已有撤单请求，再选择最远层；市价减仓的未成交余量计入预留，不重复下单。
4. **网格计划使用目标仓预算。** StockAdaptive 用 `grid target × 当前价` 生成计划，
   不再独立重复计算一套未受目标约束的网格资金。
   低价层按不低于中心价的基准换算数量，避免计划股数超过同一份目标预算。
   旧库存与待买订单仍由原目标差额、单标的及组合准入扣除；LegacyDgt 几何数量算法不变。
5. **保留恢复屏障，减少持久化复制。** 写盘借用策略账本与分析历史，不再克隆整本历史。
   Bar 完成后由组合 dispatch 统一记录并落盘，取消一次重复写盘。
   发单/撤单前持久化、临时文件、文件及目录 fsync、原子 rename 均保留。

另外修复两项执行/统计边界：

- Tick 模式启用 spread 门槛时必须有新鲜 Quote；TradeTick 不刷新 Quote 的时间。
  watchdog 也检查报价过期并请求撤买单；已有库存退出仍可进行。
  Bar-only 模式没有虚构 Quote，因此其 spread、盘口深度与排队验收仍不成立。
- Gap PnL 用上一时段最后权益记录中的库存乘开盘价差，不使用开盘撮合后的库存。
  开盘买入不追溯承担隔夜损失，开盘卖出也不抹掉已发生的隔夜损失。
  此项是按观测库存的归因，不是可与 Net PnL 相加的独立现金流；晚到成交/收盘缺数据仍需逐笔审计。

## 本轮文件

均位于 `crates/trading/src/examples/strategies/dynamic_grid/`：

- `strategy.rs`：清仓、选择性减仓、统一计划预算、报价门槛、Gap 归因、Bar 写盘收敛。
- `risk.rs`：共享的可验证冻结资金重叠计算。
- `engine.rs`：StockAdaptive 计划数量与目标股数预算一致。
- `stock.rs`：Quote 独立时钟与过期门槛。
- `multi_asset.rs`：组合现金容量、借用式检查点序列化。
- `multi_asset/tests.rs`：原生账户、命令路由、库存、恢复及逐项回归。
- `analytics.rs`：明确 Gap PnL 归因口径。

仓库已有其他未提交改动不属于本轮新增修复，没有提交或重置它们。

## 验证

逐项先运行失败复现，再修复并运行 Dynamic Grid 模块测试。
已通过：176 项，0 失败，4 项既有性能测试默认忽略。
新覆盖包括 Core-only/混合清仓、部分成交、资金重叠验证、仅超额买单撤销、
网格计划目标上限、无 Quote 入场拒绝、报价过期/恢复、开盘买卖的 Gap 归因及完整检查点读写。

- PASS：debug 的 `dynamic-grid-backtest`、`longbridge-dynamic-grid` 构建及对应 `cargo check`。
- PASS：Paper runner 离线解析，两只股票均为 15/8、Tick execution；未连接券商。
- PARTIAL：原生集成 75/78 通过。另 3 项仍因旧 comparison 配置最大标的分配 20%，
  与共享 AAPL 的 30% 冲突而失败；未为了通过测试放宽风险阈值。
- PARTIAL：Clippy 被无关 `momentum_pullback` 的既有 12 项错误阻挡；未报告本轮 Dynamic Grid 生产代码错误。
- PASS：目标文件 rustfmt、Markdown 校验、`git diff --check`。

### 固定 H1 回放

AAPL/MSFT、2025 H1、初始资金 $100,000、95,160 根 Bar、0 条 Quote。
与 `dynamic-grid-final-h1-parity-20260925.json.gz` 比较：配置相等，两只股票每个权益点的时间、
市场价格与 regime 路径相等。没有修改费用、滑点、资金预算、随机种子或挑选参数。

- 净收益：$3,423.59 → $3,173.88；总收益：3.42359% → 3.17388%。
- MDD：3.066% → 3.177%；Sharpe：1.284 → 1.223；Sortino：2.663 → 2.453。
- 资金利用率：14.786% → 14.352%；费用：$147.02 → $131.03。
- 完成网格周期：29 → 33；成交次数：78 → 86。
- 总撤单：1,151 → 289；目标减仓撤单：970 → 57；其中卖单撤销：611 → 0。
- 修复后加载 1.93 秒、事件回放 103.48 秒、收尾 1.70 秒、写报告 6.84 秒，总计 113.94 秒。
  这是一次本机测量，不是同机空闲重复基准，不能据此宣称性能提升倍数。

**结论：执行冗余减少，但本样本的收益、回撤和资金利用率没有改善。**
数量预算及撤单行为同时变化，未进行单因素消融，不能将收益差归因于某个独立修复。
Gap PnL 同时受归因纠正及库存路径变化影响，不能把其变化解释为已降低隔夜风险。
Bar 撮合仍含模型化价格改善，未验证真实 spread、深度、队列及最低收费。
本轮不以提高回测收益为由撤销正确性修复，也不自动部署实盘。

报告：`reports/dynamic-grid-review-fixed-h1-20260925.json.gz`，已进行 gzip 完整性校验。

### 强制清仓复现

沿用 review 的 1,560 根 AAPL Bar 受限复现配置，强制触发 Maximum drawdown。
修复前买入 8 股 Core 后仍持有 8 股；修复后产生一次 8 股 Core 平仓周期，期末库存为 0。
总计 2 次成交、费用 $3.90、净盈亏 -$3.82。
这是安全行为验证，不是推荐参数或收益实验。

报告：`reports/dynamic-grid-review-flatten-fixed-20260925.json.gz`。

### 可重跑命令

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib --profile dev -j2 examples::strategies::dynamic_grid -- --test-threads=1

CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --test dynamic_grid --cargo-profile dev --test-threads 1 --no-fail-fast

CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest --bin longbridge-dynamic-grid -j2

CARGO_INCREMENTAL=0 cargo check -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest --bin longbridge-dynamic-grid -j2

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading --features examples --lib -j2

target/debug/dynamic-grid-backtest --dynamic-only \
  crates/backtest/examples/dynamic_grid_daily_h1.json reports/dynamic-grid-review-rerun.json
```

没有运行全 workspace 验证；构建期间剩余空间约 1 GiB，没有清理用户数据或其他构建缓存。

## 尚未完成及有意保留的边界

- **PARTIAL：Longbridge 冻结款去重。** 需要与账户/订单一致的券商证据，不能把冻结或 settling 总额直接加回。
  当前已解决原生计算账户的重复扣减；broker-reported 账户继续保守预留。
- **PARTIAL：持久化规模。** 消除了历史克隆和一次重复写入，但仍全量序列化历史、复制原生订单快照。
  尚未改成增量 journal，也未宣称固定内存或常数写盘耗时。
- **保留：15/8 状态规则与硬风险锁存。** 未改斜率单位/阈值、隔夜重新确认，未自动解除
  Unknown、断连、对账失败或亏损风险。状态原因分层及安全自动恢复尚未重设计。
- **NOT RUN：真实 Quote/账单费用校准、券商成交验收及实盘部署。** 本轮不使用账户资金验证。
- **NOT RUN：新增 Walk-forward、OOS、Monte Carlo 或参数调优。** 固定配置回放是回归验证，不能称为新样本外证据。

旧检查点的账本/指标格式保留；新 Quote 时间缺失时不凭旧 spread 恢复 Tick 入场，需等待新 Quote。
旧网格的数量计划不会被静默重写，新的预算规则在下次合法建网格时生效。
升级前仍须按原流程停机、核对账户与检查点；不得通过删除检查点绕过对账。
