# Dynamic Grid：15 分钟斜率校准对照

> 历史研究归档：当前股票策略已固定为 15 分钟＋8 根确认。本文实验参数与复现命令不再作为运行入口；
> 已退役配置保存在本地 `reports/dynamic-grid-retired-regime-20260925.tar.gz`。当前用法见
> [最终方案](dynamic_grid_15m_final.zh-CN.md)。以下收益与结论仅记录当时版本。

研究日期：2026-09-25。结论：**保留原来的 15 分钟＋8 根确认研究基线**。
两个较低斜率阈值都增加了趋势标签，但没有同时改善收益和状态切换，不升级为主方案。
Paper/live 配置、标的资金预算、订单与券商账户均未改变。

## 本轮改动

复用原生 Rust 回测与已有 `ma_slope_threshold`，不增加指标、参数或交易逻辑分支。
新增三组研究配置（已归档），
补充参数单位中文注释和 6 个方向分类、未收盘恢复及 ATR 不变性回归用例。
按照 longbridge-quant 的验证约束，先冻结候选，再运行对照；不因标签更多而宣称识别更准确。

当前分钟信号配置中，慢分类使用 `ma_slope_threshold × regime_bar_minutes`：

| Candidate | Config value           | Effective 15m threshold |
| --------- | ---------------------- | ----------------------- |
| Baseline  | 0.001                  | 1.5%                    |
| Lower A   | 0.00006666666666666667 | 0.1%                    |
| Lower B   | 0.00013333333333333334 | 0.2%                    |

百分比指均线斜率，不是股价单根涨跌幅。既有 1.5% 门槛在这些数据中没有产生趋势标签，
因此检验缩小门槛是否有帮助。该参数同时参与 Range 的低斜率条件，不是只改变趋势分类。
这里只改阈值，不改 MA/ADX/ATR 数值计算、指标窗口或确认根数。

## 同条件结果

AAPL、MSFT，共享初始资金 $100,000，种子 42；既有 Alpaca 原始一分钟 CSV，无真实 Quote。
maker 0.08%、taker 0.10%、附加佣金 0，一个 tick 滑点概率为 1；
网格间距 3%–6%、资金和风险预算保持不变。所有候选均为 15 分钟、8 根确认、关闭日线层。

H1：2025-01-02 至 06-30，共 95,160 根 Bar，跨季度保留库存。
Q3：2025-07-01 至 09-30，共 49,140 根 Bar，各组重新空仓启动；数据缺少 7 月 3 日半日市。
两个区间已经用于此前研究，**不能称为全新 OOS，也不能相加冒充连续九个月收益**。
本轮没有根据结果继续追调阈值。

Net、Fees 为美元；MDD、Util 为百分比；Switches 为日内有效状态切换总数。

| Period | Threshold | Net     | MDD   | Sharpe | Fees   | Util  | Switches | Resets |
| ------ | --------- | ------- | ----- | ------ | ------ | ----- | -------- | ------ |
| H1     | 1.5%      | 3423.59 | 3.066 | 1.284  | 147.02 | 14.79 | 85       | 9      |
| H1     | 0.1%      | 1450.18 | 0.654 | 1.546  | 291.65 | 9.59  | 136      | 22     |
| H1     | 0.2%      | 2826.32 | 2.070 | 1.469  | 150.97 | 12.96 | 96       | 13     |
| Q3     | 1.5%      | 1449.29 | 0.414 | 3.793  | 55.65  | 12.15 | 48       | 2      |
| Q3     | 0.1%      | 1033.48 | 0.706 | 2.195  | 94.78  | 14.54 | 59       | 9      |
| Q3     | 0.2%      | 1290.56 | 0.706 | 2.714  | 67.15  | 15.42 | 51       | 5      |

H1 的 0.1% 方案确实降低了回撤，但同时降低平均库存、减少 $1,973.41 净收益，费用接近翻倍。
0.2% 方案较温和，仍减少 $597.27 净收益、增加状态切换。两者不是无代价的风险改善。
Q3 两组均收益更低、回撤更高、费用更多；未达到“减少切换并提高收益”的目标。

H1 基线没有确认的 TrendUp/TrendDown；0.1% 方案分别有 3,859/5,541 个分钟诊断点，
0.2% 方案为 830/1,310。更多趋势标签激活了既有仓位和重置政策，并不自动创造网格收益。
Q3 的 0.1% 方案目标减仓撤单从 149 增至 318 次，Grid 已实现盈亏从 $1,342.62 降至 $800.04。
这些是交易路径变化的证据，不是独立的因果归因；没有真值标签，不能报告分类准确率。
同样不能把 `regime_pnl` 的标签归属变化直接解释成新增 Alpha。

## 校验与限制

- PASS：两区间的基线完整报告与上一轮 8 根确认报告相等，包括权益、成交周期和诊断，而非只比较净收益。
- PASS：三组有效配置仅斜率阈值不同；逐点价格、ATR、分钟及 15 分钟指标数值一致，快层高低波动判定不变。
  快层原始方向标签也可能随阈值改变，但启用慢分类时不用于替代慢层方向决策。
- PASS：Dynamic Grid 模块 168 项通过、4 项既有忽略，包含本轮新增 6 项；debug build 和对应 check 成功。
- PARTIAL：交易库 Clippy 仍被无关 `momentum_pullback` 的 12 个既有错误阻断，未放宽 lint 或修改无关策略。
- NOT RUN：全 workspace、券商成交验收、新 walk-forward、Monte Carlo，以及真实 Quote/最低收费/执行延迟压力测试。

保留失败候选用于复现，不将其写入主基线。下一项独立实验应优先检验
目标仓位逐分钟取整引起的小额反复调仓，而非继续降低阈值或叠加指标；
该工作本轮未实现，必须保持风控减仓和覆盖卖出不受影响。

## 复现

使用已有 debug 二进制；构建与测试命令见[主基线文档](dynamic_grid_15m_confirmation.zh-CN.md)。

```bash
target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_daily_h1.json \
  crates/backtest/examples/dynamic_grid_15m_slope.json \
  reports/dynamic-grid-15m-slope-h1.json

target/debug/dynamic-grid-backtest --ablation \
  crates/backtest/examples/dynamic_grid_regime_q3.json \
  crates/backtest/examples/dynamic_grid_15m_slope.json \
  reports/dynamic-grid-15m-slope-q3.json
```

数据窗口文件名中的 `daily` 不代表本轮启用日线。完整报告已无损压缩并通过 `gzip -t`：

- `reports/dynamic-grid-15m-slope-h1-20260925.json.gz`。
- `reports/dynamic-grid-15m-slope-q3-20260925.json.gz`。

压缩只替换本轮生成的 JSON，可解压恢复；未删除行情、源码或交易检查点。
