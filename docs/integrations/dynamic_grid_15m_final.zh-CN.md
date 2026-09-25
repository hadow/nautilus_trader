# Dynamic Grid：最终 15 分钟方案

2026-09-25：结束周期与确认参数研究，股票策略固定为 **15 分钟分类＋8 根连续确认**。
回测、Paper 和 Longbridge Live 复用同一 Rust 核心。本次不启动券商连接或交易。

## 唯一股票执行路径

已完成一分钟 LAST Bar → 原生 `BarBuilder` 聚合 15 分钟 → 原分类器 → 8 根连续确认 → 原目标仓位与组合风控 → 原订单执行。

- 15 分钟桶以纽约 09:30 为锚点，仅消费完整收盘数据；缺分钟不补造，重复行情不重复计数。
- 目标仓位、订单政策、重置和组合资金分配共用确认状态；不是只对新增买单延迟确认。
- 分钟 ATR、跳空、流动性、突破确认、Tick 执行、订单超时与账户硬风控保持原时钟。
  分钟高低波动立即生效，覆盖库存的风险退出不等待 8 根确认。
- 8 根是约 120 分钟，指标初次暖机另计。隔夜/缺桶仍重新确认，未增加盘前数据或日线预热。
- 斜率阈值及其他经济参数未调整，未采用上一轮较低斜率候选。

## 清理范围

按 codebase-design 的精简接口原则，删除日线聚合/分类、日线历史与日历预热、日线专用诊断字段，
以及一分钟/多周期实验分支、`confirm_regime_changes` 开关和重复的分钟确认计数。
原 `LegacyDgt` 基准与通用回测能力保留，不再扩展研究。

六份实验配置已从 examples 移除，与退役日线源码一起归档到
`reports/dynamic-grid-retired-regime-20260925.tar.gz`，可用 `tar -tzf` 查看后恢复。
历史回测报告、行情和交易检查点没有删除；历史研究文档保留并标为归档。

`regime_bar_minutes = 15`、`regime_confirmation_bars = 8` 仍显式保留在各标的配置中用于审计，
但 StockAdaptive 启动校验拒绝其他取值或关闭常规交易时段。
已同步六只股票的独立配置、sandbox 和单标的示例；Paper/live 继续引用共享标的文件。
选股评分复用同一分类模块，历史不足时返回 `REGIME_CONFIRMATION_WARMUP`，不冒充已确认状态。
选股示例历史由 400 根增至已有接口支持的 1,000 根分钟线，以容纳慢指标预热与确认；缺失数据仍不补造。

## 恢复与部署

新方案继续持久化各标的未完成桶、指标历史、候选/确认状态及计数，沿用原订单幂等和券商对账屏障。
旧配置中的 `daily_regime`、`confirm_regime_changes` 已删除，读取时明确报错，不静默忽略。
一分钟/日线旧检查点不可直接作为新策略状态恢复，不能通过删除检查点来绕过持仓与挂单对账。
本次未迁移账户状态、未重启进程；如已有运行中的旧实例，部署前必须停机并审计持仓、订单及检查点。

## 运行与验证

正常运行不再需要 ablation 覆盖文件，例如：

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest --bin longbridge-dynamic-grid -j2

target/debug/dynamic-grid-backtest --dynamic-only \
  crates/backtest/examples/dynamic_grid_portfolio.json \
  reports/dynamic-grid-final-q1.json

# 仅校验并展示 Paper 配置，不连接券商、不下单
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_paper.json
```

## 实际验证记录

- PASS：debug 回测与 Longbridge runner 构建、对应 `cargo check`。
- PASS：Dynamic Grid 模块 165 项通过，4 项既有忽略；包括固定参数拒绝、缺分钟、恢复、
  独立标的路由、快波动优先级及选股预热测试。
- PASS：AAPL/MSFT 2025 H1 的 95,160 根 Bar 重放。完整报告与原 15/8 基线相等，
  包括逐笔周期、权益路径、费用、库存、状态、风险及诊断；不是只看最终收益接近。
  净收益仍为 $3,423.59、MDD 3.066%，本轮没有提高收益或重新选择参数。
  报告：`reports/dynamic-grid-final-h1-parity-20260925.json.gz`，无损压缩校验通过。
- PASS：Paper runner 离线解析共享配置，确认 15/8；未连接券商。
- PARTIAL：使用 nextest 隔离执行原生集成文件，78 项中 75 项通过。
  剩余 `comparison`、`portfolio_research`、`ablation` 三个显式配置测试均因旧 comparison 文件的
  `max_instrument_allocation = 0.20` 与共享 AAPL 分配冲突；未为了过测放宽资金上限。
  股票趋势/震荡/跳空、分钟硬回撤、部分成交、恢复和 Paper 配置一致性场景通过。
  该既有 `crates/trading/tests/dynamic_grid.rs` 被仓库忽略规则匹配；本次修改已在本机运行，未强行暂存。
- PARTIAL：Clippy 仅剩无关 `momentum_pullback` 的 12 个既有错误；本轮新增 lint 问题已修复。
- PASS：修改代码的 rustfmt、文档 Markdown 校验与 `git diff --check`。
- NOT RUN：全 workspace、真实账户验收、实盘部署；没有开展新参数实验。

验证命令：

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --profile dev -j2 examples::strategies::dynamic_grid -- --test-threads=1

CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --test dynamic_grid --cargo-profile dev --test-threads 1 --no-fail-fast

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading --features examples --lib -j2
```
