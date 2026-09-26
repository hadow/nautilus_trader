# Dynamic Grid：整股预算与受控换层验证

2026-09-26，依次完成 P0 整股分配、P1 受控换层的另一时间窗口及费用压力对照、
P2 历史范围被动观测。本轮没有修改 paper/live 配置，没有账户下单或重新优化 15 分钟 regime。
结论：整股分配修复了资金被切碎的问题，但并非所有窗口收益都改善；两个候选均不晋升默认。

## P0：整股预算分配

复用 `GridEngine::build` 和现有 `position_sizing`，新增显式研究选项 `EqualLots`。
原 `Equal` 及旧配置序列化不变，不增加另一个分配器或执行分支。

规则：

- 先按原 Equal 规则分配预算并向下取整。
- 分别汇总下方买入侧与上方种子库存侧的未分配余款，两侧不能互借。
- 在原有价格层上按近到远，每层最多补一个 lot，只有余款足够才补。
- 剩余零钱留现金；不改变层级数量、中心、价格、边界、重置规则或旧库存目标。
- StockAdaptive 低价层仍按 `max(center, buy_price)` 预算，不能越跌换算出越多计划股数。
- 数量在建网时确定，不因每根 Bar 重新取整而重复发放。真实订单仍扣除历史库存、
  待成交预留，并经过费用、仓位、现金与组合准入。

例如中心 600、网格预算 3,000、每侧 8 层、种子比例 20%、每手 1 股：
原模式全部取整为零；新模式给下方最近 4 层各 1 股，上方最近一层 1 股。
这没有扩大总预算，但会让库存更早形成，并可能扩大回撤，不能当作无风险收益修复。

只在研究配置中选择：

```json
{
  "entry_mode": "Sequential",
  "position_sizing": "EqualLots"
}
```

Sequential 仍每个标的最多一张未终结 Grid 买单，跨过的层不追补。
配置切换仍受原检查点一致性校验约束，不允许删除检查点强行上线。

## 数据与固定条件

六标的为 AAPL、MSFT、NVDA、TSLA、AMZN、META；使用现有配置引用的本地分钟 OHLCV。
初始资金 100,000 美元，随机种子 42，原生撮合的一 tick 不利滑点概率为 1。
正常 maker/taker 费率为 0.08%/0.10%，额外 commission 为零；没有最低收费账单模型。
没有真实 Quote、排队位置、市场冲击或券商网络延迟数据。

- 1 月窗口：2025-01-02 至 01-17，共 11 个交易日、25,740 根 Bar、0 条 Quote。
- 2 月窗口：2025-02-03 至 02-14，共 10 个交易日、23,400 根 Bar、0 条 Quote。
- 两个窗口各自从现金启动、在窗口内预热，不是把 1 月库存续接到 2 月。
- 参数在对照前固定，没有扫描，也没有把 EqualLots 与换层组合挑参。
- 两段都属于已研究过的历史范围，不能称为严格未触碰 OOS 或 Walk-forward。

保留用户已修改为整个 Q1 的 `dynamic_grid_entries_smoke.json`；新建独立日期配置，
不通过缩短用户原文件来运行本次短窗口。

## P0 回放结果

净收益包含期末未实现盈亏；利用率包含库存及待买预留。

| 窗口 | 分配方式  | 净收益 $ | Grid 已实现 $ | 费用 $ | 周期数 | 利用率  | MDD     |
| ---- | --------- | -------: | ------------: | -----: | -----: | ------: | ------: |
| 1 月 | Equal     | 420.97   | 24.44         | 33.94  | 9      | 11.331% | 0.6834% |
| 1 月 | EqualLots | 477.90   | 77.40         | 46.01  | 23     | 12.309% | 0.7058% |
| 2 月 | Equal     | -46.03   | 64.28         | 24.46  | 2      | 14.366% | 1.0103% |
| 2 月 | EqualLots | -76.53   | 79.50         | 31.03  | 6      | 15.815% | 1.1725% |

1 月 META 的两代网格都由 0 个有数量层变为 8 个，完成周期从 0 增到 5，
Grid 已实现利润为 25.61 美元。组合最大库存市值由 25,634.87 增至 26,892.40 美元。
旧 Sequential 的完整有效配置、权益路径、周期、诊断与风险报告和上一轮精确一致，
并核对新候选仅 `position_sizing` 不同。

2 月新分配的 Grid 已实现利润增加 15.22 美元，但期末浮亏由 110.31 增至 156.03 美元，
组合净收益反而减少 30.50 美元，最大库存市值由 19,956.41 增至 22,932.22 美元。
因此本次证据支持“恢复可交易性”，不支持“增加利用率必然增加收益”。

## P1：受控换层与费用压力

保留已冻结的 `sequential_requote_bars: 2`，不叠加 EqualLots。
相同费用的一对实验只差换层选项。另做 maker/taker 费率同时加倍的成对实验。

| 2 月候选             | 净收益 $ | Grid 已实现 $ | 费用 $ | MDD     | 换层撤单 | 总撤单请求 |
| -------------------- | -------: | ------------: | -----: | ------: | -------: | ---------: |
| Sequential           | -46.03   | 64.28         | 24.46  | 1.0103% | 0        | 26         |
| Sequential + 换层    | -46.03   | 64.28         | 24.46  | 1.0103% | 0        | 26         |
| Sequential，双倍费用 | -12.33   | 62.73         | 50.68  | 0.8742% | 0        | 22         |
| 换层，双倍费用       | -12.33   | 62.73         | 50.68  | 0.8742% | 2        | 24         |

正常费用下本窗口没有实际换层；双倍费用下 TSLA 换层 2 次，却未改善净收益。
这不足以否定所有窗口的价值，但没有支持将上一轮 1 月改善外推为稳定优势。

费用加倍同时提高了策略的成本间距下限和预留，改变了网格与成交路径。
因此双倍费用组亏得更少不能解释为“手续费越高越好”，也不是固定成交下的纯费用敏感性。
若只固定正常组已有成交、额外再扣一倍原费用，两组净收益均约为 -70.49 美元；
该计算是条件估计，不是重新撮合或实际券商最低收费模型。

订单层新增验证覆盖撤单待确认期间晚到的部分/全部成交、重复成交、立即维护卖单覆盖，
以及撤单超时后停止入场并保留预留。测试夹具必须确认新卖单，否则原 30 秒超时门槛会
正确停买；本轮补齐回报，未放宽超时阈值。
这些是离线事件顺序验收，不是带真实网络延迟的账户验收。

## P2：15 分钟历史范围被动观测

`scripts/dynamic_grid_range_observation.py` 只分析报告，不生成信号、配置或订单。
只取原生聚合已完成的 `regime_source` 对应分钟收盘价，复用现有 `ma_period=20`。
每次创建网格只读取时间戳不晚于创建时间的数据；不足 20 根时输出未预热。
窗口允许跨交易日，不能将其解释为固定的 5 小时自然时间或完整多日持仓周期。

下表比较每个窗口首次建网时 `(最高收盘 - 最低收盘) / center` 与实际网格总宽度：

| 标的 | 1 月历史范围 | 1 月网格宽度 | 2 月历史范围 | 2 月网格宽度 |
| ---- | -----------: | -----------: | -----------: | -----------: |
| AAPL | 1.362%       | 17.763%      | 1.126%       | 17.763%      |
| MSFT | 1.032%       | 17.761%      | 1.106%       | 17.760%      |
| AMZN | 1.709%       | 9.980%       | 1.186%       | 9.986%       |
| META | 2.775%       | 7.983%       | 1.960%       | 7.984%       |
| NVDA | 1.458%       | 7.987%       | 未建网       | 未建网       |
| TSLA | 2.815%       | 5.987%       | 1.487%       | 5.990%       |

这提示网格尺度与近期路径存在差异，但短窗口范围较窄并不能证明它适合作为多日边界。
1 月 NVDA 第二次建网时锚点已经不在此前 20 根完成收盘的范围内，也说明不能原样复制
历史上下界来代替围绕当前价格建网。现在不改 ATR、最低间距、突破确认或旧仓止盈。
若继续研究，应单独检验网格宽度，不同时修改 regime、层数与资金预算。

## 复现命令

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest --features examples \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_funding_jan.json \
  crates/backtest/examples/dynamic_grid_funding_research.json \
  reports/dynamic-grid-funding-jan.json

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_funding_feb.json \
  crates/backtest/examples/dynamic_grid_funding_research.json \
  reports/dynamic-grid-funding-feb.json

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_funding_feb.json \
  crates/backtest/examples/dynamic_grid_requote_stress.json \
  reports/dynamic-grid-requote-stress-feb.json

python3 scripts/dynamic_grid_range_observation.py \
  reports/dynamic-grid-funding-jan.json > reports/dynamic-grid-range-jan.json
python3 scripts/dynamic_grid_range_observation.py \
  reports/dynamic-grid-funding-feb.json > reports/dynamic-grid-range-feb.json
```

报告输出路径若已存在会被回测工具覆盖，保留上一轮结果时应改用新文件名。
以上只运行 Dynamic Grid；不增加 Fibonacci、额外 benchmark 或完整季度扫描。

## 工程验证与边界

本轮按 TDD 先复现所有层取整为零，再实现最小改动；按 longbridge-quant 约束保留
固定参数、同费用成对比较和独立研究窗口，不把结果宣称为样本外有效性。

- 核心策略仍是 Rust，复用原 Nautilus 策略、账本、组合风控与 Longbridge Adapter。
- 新增研究选项缺省不启用；保留现有 Equal 与所有 paper/live 配置。
- 不修改 checkpoint 格式，不清理订单历史，不重新分配旧代库存。
- 本轮不做实盘/模拟账户成交验收、Walk-forward、Monte Carlo 或严格 OOS。
- 新增 Python 仅负责被动报告分析，不进入任何交易决策路径。

实际验收：

- PASS：Dynamic Grid 库测试 236 项；另 4 项既有手动性能测试未运行。
- PASS：原生 `dynamic_grid` 集成测试经 nextest 隔离运行，87 项全部通过。
- PASS：Longbridge 离线库测试 55 项、runner 测试 20 项；只使用本机测试服务。
- PASS：被动分析脚本 2 项测试，覆盖未完成桶、未来价格、重复/错位时间戳与预热。
- PASS：`cargo check`、debug 回测构建、修改文件 rustfmt、Ruff、`git diff --check`。
- PASS：两份中文研究文档通过 Markdownlint，零问题。
- PARTIAL：严格 Clippy 仍被未修改的 `momentum_pullback/model.rs`、`strategy.rs` 中
  12 项既有问题阻止，Dynamic Grid 没有新诊断；未关闭 lint 或修复无关策略。
- NOT RUN：全仓与 release 构建、真实券商延迟/对账验收。

共 400 项测试通过。本轮新增 9 个 Rust 测试实例及 2 个报告分析测试。
受限磁盘下继续复用 debug 产物，不重建已清理的 nextest profile，不变更 Cargo profile。

```bash
CARGO_INCREMENTAL=0 cargo check -p nautilus-trading --features examples --lib -j2
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib --profile dev -j2 dynamic_grid -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run \
  -p nautilus-trading --features examples --test dynamic_grid \
  --cargo-profile dev --test-threads 1
CARGO_INCREMENTAL=0 cargo test -p nautilus-longbridge --features dynamic-grid \
  --lib --bin longbridge-dynamic-grid --profile dev -j2 -- --test-threads=1
.venv/bin/python -m pytest -q -c /dev/null \
  -o cache_dir=/tmp/grid-range-pytest-cache scripts/test_dynamic_grid_range_observation.py
CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading --features examples \
  --lib -j2 -- -D warnings
```

后续账户验证仍需撤单超时后的真实 broker reconciliation，不能用离线测试替代。
收益结论以冻结候选的更多独立窗口及实际费用验证为准。

## 本轮文件范围

- 修改 `config.rs`、`engine.rs`：增加 EqualLots 并复用原建网、恢复和选股调用路径。
- 修改 `dynamic_grid/tests.rs`、`multi_asset/entry_tests.rs`：预算、恢复及撤单竞态回归。
- 新增 `dynamic_grid_funding_jan.json`、`dynamic_grid_funding_feb.json`：两个独立日期窗口。
- 新增 `dynamic_grid_funding_research.json`、`dynamic_grid_requote_stress.json`：固定单因素对照。
- 新增 `scripts/dynamic_grid_range_observation.py` 及对应测试：离线因果观测。
- 新增本文，并从 Sequential 文档链接到本文。

保留工作区此前已有的 Longbridge、策略及配置修改；没有删除文件、提交 Git 或启动交易。
