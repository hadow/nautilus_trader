# Dynamic Grid 市场状态识别验证

> 历史研究归档：当前股票策略已固定为 15 分钟＋8 根确认。本文实验参数与复现命令不再作为运行入口；
> 已退役配置保存在本地 `reports/dynamic-grid-retired-regime-20260925.tar.gz`。当前用法见
> [最终方案](dynamic_grid_15m_final.zh-CN.md)。以下收益与结论仅记录当时版本。

研究日期：2026-09-24。代码基线：`8854595721`，NautilusTrader `0.63.0`。
本轮只研究已有 Dynamic Grid，不增加指标、不关闭趋势过滤、不修改 Paper/live 配置。

## 本轮实际修改

- [regime.rs](../../crates/trading/src/examples/strategies/dynamic_grid/regime.rs)：
  将原分类条件集中到私有 `classify`，同时返回分类和首个决定分类的原因，保持原判定优先级。
- [diagnostics.rs](../../crates/trading/src/examples/strategies/dynamic_grid/diagnostics.rs)：
  在已有 `diagnostics.spacing[].regime` 中记录完成 Bar 的完整指标快照和原因。
  不另写报告分类器，不改变订单、目标仓位、现金预留或风险逻辑。
- 研究配置（已归档）：
  复用已有 Rust `--ablation`，三个候选各只修改一个参数；不进行笛卡尔搜索。

新原因字段与诊断快照都允许缺失。旧检查点、旧报告不伪造原因；
已有检查点仍验证指标与已完成观测的一致性。新检查点的原因若与重放不一致则拒绝恢复。

## 实验口径

标的是 AAPL、MSFT，一个策略实例、共享账户，初始资金 100,000 美元。
数据为 `wsl_test_data/{AAPL,MSFT}/bars.csv.gz` 中完成的 1 分钟 LAST EXTERNAL Bar。
Q1 覆盖 2025-01-02 至 2025-03-31，共 46,800 根输入 Bar。

费用保持 maker 0.08%、taker 0.10%、附加 commission 0。
成交滑点使用原生 OneTickSlippageFillModel，概率 1、随机种子 42。
间距成本门槛仍为 `slippage=0.0005`、`minimum_profit_margin=0.0005`，不是另收一次成交滑点。
网格间距、层数、资金预算、组合风险、目标仓位乘数、重置和执行规则全部不变。
输入包含 0 条真实 Quote，不声称模拟了真实点差、队列或券商最低收费。

候选在读取本轮结果前固定，并原样进入后续季度验证：

| Candidate         | Override                              |
| ----------------- | ------------------------------------- |
| `baseline`        | None                                  |
| `adx_range_24`    | `adx_range_max: 20 -> 24`             |
| `slope_0_0002`    | `ma_slope_threshold: 0.001 -> 0.0002` |
| `slope_window_20` | `slope_period: 5 -> 20`               |

三项分别检验：缩小 ADX 分类空档、提高分钟级趋势灵敏度、延长斜率观察窗口。

斜率是每根信号 Bar 的均线比例变化，不是日线趋势。
默认阈值 0.001 对应每分钟 0.1%，5 根窗口约需均线累计变化超过 0.5%。
`slope_period` 也参与已有指标重放历史的容量计算：改变它会改变预热长度，
并可能轻微影响 Wilder 指标的重放初值，不应声称该候选仅改变斜率数值。

## Q1：归因与交易结果

指标预热后，两股票共记录 46,748 个带快照的间距观测；以下比例以这些观测为分母，
不是以日历时间、持仓时间或订单数为分母。

| Reason                  | Count  | Share  |
| ----------------------- | -----: | -----: |
| TREND_SLOPE_TOO_SMALL   | 18,831 | 40.28% |
| ADX_TRANSITION          | 9,658  | 20.66% |
| OUTSIDE_BOLLINGER_BANDS | 2,860  | 6.12%  |
| RANGE                   | 15,227 | 32.57% |
| TREND_UP / TREND_DOWN   | 172    | 0.37%  |

前三项依次是：ADX 达到趋势门槛但斜率不足、ADX 位于 Range 与 Trend 门槛之间、
低 ADX 且低斜率但收盘在布林带外。

`Disabled` 合计 31,349 次，约 67.06%。其中约 60.07% 是趋势斜率不足、30.81% 是 ADX 空档。
本区间没有因高低波动率条件决定的分类。这不代表这些风控可以删除。

基线两股票合计有 4,039 次日内相邻观测分类变化。将超过 90 秒的数据间隔断开后，
2,024 段连续 Range 中有 906 段不足 5 根，约 44.76%。这说明分类切换确实频繁，
但不能直接把短 Range 全部判为错误信号：本轮没有独立的真实状态标签。

下表依次为候选、净损益（美元）、收益率、最大回撤、Sharpe、费用（美元）、成交回报数、资金利用率。

| Q1 Candidate    | Net PnL   | Return   | MDD     | Sharpe | Fees     | Fills | Utilization |
| --------------- | --------: | -------: | ------: | -----: | -------: | ----: | ----------: |
| baseline        | -876.29   | -0.8763% | 2.0692% | -1.230 | 202.05   | 81    | 13.16%      |
| adx_range_24    | -480.89   | -0.4809% | 1.8669% | -0.622 | 209.25   | 85    | 14.29%      |
| slope_0_0002    | -1,990.08 | -1.9901% | 2.1362% | -6.621 | 1,061.94 | 571   | 5.87%       |
| slope_window_20 | -1,007.93 | -1.0079% | 1.9389% | -1.101 | 144.83   | 51    | 16.89%      |

这里的成交回报数对应 `number_of_trades`，不是完整买卖周期数。
净损益包括期末库存盯市，未只比较盈利平仓周期。

- ADX 上限 24 在 Q1 改善收益和 MDD，但日内分类切换增至 5,185 次；
  `REGIME_ORDER_POLICY` 撤单从 2,364 次升到 3,121 次，不能说它解决了抖动。
- 降低斜率阈值使趋势观测从 172 次增加到 5,800 次，但费用约增至原来的 5.26 倍，
  重置从 10 次增至 59 次，亏损扩大。更多趋势标签不等于更多有效信号。
- 延长斜率窗口降低费用和成交次数，但净亏损、最大资金暴露反而增加，不能仅凭交易变少宣布改进。

当前 `regime_confirmation_bars=5` 只确认新增买入资格，不延迟原始分类变化、
不合规订单撤单或目标仓位下调。因此不能把它描述为完整状态滞回。
上述对照仍保留这个行为；没有为了减少撤单而延迟已有硬风控。

## Q2：固定候选的后续季度验证

区间为 2025-04-01 至 2025-06-30，共 48,360 根 Bar。
仅将组合配置起止时间改为 `1743465600000000000`、`1751328000000000000`，
其余配置与 Q1 一致，已经进行配置逐字段核对。
四组候选均未根据 Q1 结果修改；本季度重新以 100,000 美元、空仓启动。
这不是持仓跨季度连续运行，不能将两季收益直接相加当作半年策略收益。

| Q2 Candidate    | Net PnL  | Return   | MDD     | Sharpe | Fees     | Fills | Utilization |
| --------------- | -------: | -------: | ------: | -----: | -------: | ----: | ----------: |
| baseline        | -869.41  | -0.8694% | 3.9437% | -0.612 | 404.82   | 264   | 12.28%      |
| adx_range_24    | -181.09  | -0.1811% | 3.4237% | -0.116 | 416.37   | 287   | 12.65%      |
| slope_0_0002    | -285.75  | -0.2858% | 0.8548% | -0.582 | 1,042.96 | 641   | 7.91%       |
| slope_window_20 | 1,465.54 | 1.4655%  | 3.7678% | 0.951  | 341.23   | 231   | 13.43%      |

Q2 在本轮候选固定后才运行，但无法证明此前研究从未查看该区间。
因此称为后续季度验证，不将其包装为严格未触碰的 OOS，也不根据 Q2 最优结果重新挑参。

## 结论与采用范围

1. **不增加指标。** 分类空档和高斜率门槛的影响已经可观测；目前没有证据需要增加 ER 等新指标。
2. **保留 ADX 上限 24 为固定研究候选，不替换实盘参数。** 两季都减少净亏损与 MDD，
   但仍未盈利，Q1 状态切换和撤单增加，Q2 `grid_churn` 也由 4,100 升至 5,434。
   这支持继续验证，不支持宣称提高了市场状态识别准确率或产生了稳定 Alpha。
3. **不直接采用更低斜率阈值。** 它在 Q1 更差、在 Q2 降低回撤，但两季费用均显著增加。
   同一个阈值同时影响 Range 条件和趋势条件，不能简单解释成“识别趋势更准确”。
4. **不因 Q2 盈利就采用更长斜率窗口。** Q1 净损益更差，跨阶段效果并不一致。
5. **本轮落地的是归因、恢复兼容性和可复现实验。** 当前交易规则、标的 JSON、Paper/live 配置不变，
   没有启动券商交易、没有改变硬风控，也没有实现新的状态滞回。

如果继续优化，下一项应隔离检验“软状态变更的确认机制”，同时保留硬风控即时生效，
考察撤挂、减仓、费用和库存回撤，而不是再增加一批指标或放宽所有 Disabled 条件。
该机制本轮未实现、未验证，不能把本轮收益变化归因于它。

新增快照沿用原有逐 Bar 诊断存储，会增加报告及检查点体积；
本轮没有解决长期运行历史归档、检查点增长或磁盘容量管理问题。

## 行为等价与复现

新增诊断前先运行 `--dynamic-only`；重新编译后，`--ablation` 中的 baseline 与原报告比较。
仅移除新增的 `diagnostics.spacing[].regime` 后，整个策略报告逐字段完全相等：
包括各标的和组合的全部权益点、所有完成周期、费用、库存指标、风险状态、相关性及事件计数。
这不是仅核对最终收益近似相等。

原始结果文件位于仓库本地 `reports/`，不作为源码提交：

- `dynamic-grid-regime-baseline-20260924.json`：加诊断前。
- `dynamic-grid-regime-ablation-20260924.json`：Q1 四组，包含完整 `effective_config`。
- `dynamic-grid-regime-q2-ablation-20260924.json`：Q2 四组，包含完整 `source_config` 与 `effective_config`。

从仓库根目录执行：

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-backtest -p nautilus-longbridge \
  --features nautilus-backtest/examples,nautilus-longbridge/dynamic-grid \
  --bin dynamic-grid-backtest -j2

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_portfolio.json \
  crates/backtest/examples/dynamic_grid_regime_research.json \
  reports/dynamic-grid-regime-ablation-rerun.json

# 从结果取回本轮 Q2 的完整配置，避免后续标的配置改动污染复现。
jq '.source_config' reports/dynamic-grid-regime-q2-ablation-20260924.json \
  > /tmp/dynamic-grid-regime-q2-rerun.json
target/debug/dynamic-grid-backtest --ablation \
  /tmp/dynamic-grid-regime-q2-rerun.json \
  crates/backtest/examples/dynamic_grid_regime_research.json \
  reports/dynamic-grid-regime-q2-ablation-rerun.json
```

本轮候选文件的 `settings` 供已有研究 runner 读取；`--ablation` 不执行其中的 Walk-forward 设置。
没有运行 Walk-forward、Monte Carlo 或完整参数邻域扫描，也没有指标分类准确率检验。
Q1 已在之前研究中反复查看，绝不是未见过的 OOS。

原始行情压缩文件 SHA-256：

```text
AAPL 702ac29b31a6f1ce932cdd11e379f6a91c1d79a494c8851ec3b0ab894921bc3f
MSFT 113878abd71aafbef4895d21a2cf8df72ec2843ae89927360a0423b1530c814a
```

## 验证状态

- PASS：debug 回测二进制构建。
- PASS：`nautilus-trading` 696 项通过、4 项原有性能测试忽略；`nautilus-longbridge` 42 项通过。
  合计 738 项通过，包含本轮新增的 11 项分类边界、指标优先级、诊断无副作用及旧检查点恢复测试。
- PASS：Q1/Q2 共 8 次完整季度回放；全部 `effective_config` 核对仅存在声明的单字段变化。
- PASS：加诊断前后 Q1 baseline 完整报告等价，除新增诊断字段外无差异。
- PASS：修改文件的 rustfmt、Markdown lint、`git diff --check`。
- BLOCKED：定向 Clippy 被 13 项既有错误阻塞，与 `/tmp/dynamic-grid-gap-clippy.log` 中的位置、信息完全一致。
  一项位于 `dynamic_grid/files.rs:56` 的注释标记，另 12 项位于 `momentum_pullback`；本轮修改文件无诊断。
  未压制 lint，也未顺带修改其他策略，不能报告 Clippy 全绿。
- NOT RUN：全仓测试、全仓 Clippy、真实券商订单、Paper/live 部署及严格 OOS。

实际测试命令：

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --profile dev -j2 -- --test-threads=1

CARGO_INCREMENTAL=0 cargo clippy -p nautilus-trading -p nautilus-longbridge \
  --features nautilus-trading/examples,nautilus-longbridge/dynamic-grid \
  --lib --tests -j2
```

Longbridge 本地模拟服务器测试需要绑定 localhost，沙箱内曾被权限限制，允许本地绑定后重跑全部通过。
它不连接真实券商、不提交账户订单，不能作为账户级成交验收证明。
