# 前视与样本泄漏审计

| 检查点 | 实现与证据 | 结论 |
| --- | --- | --- |
| Sigma | 日内 profile 只在 session 完成后入 history；当前日追加不参与 previous L day 同分钟均值 | 因果；golden + future perturbation 通过 |
| RVOL | 独立历史 14 日同分钟量；不取当前日其他分钟 | 因果；不足历史按 Notebook fillna(1)；正量/零均量以 rvol_infinite 表达 |
| Previous close | 上一个已完成交易 session 的最后 bar；不使用自然日回退 | 因果；缺失分钟和未完成前一日被拒绝 |
| Vol targeting | 15 个已完成收盘价产生 14 个收益；样本标准差 ddof=1 | 因果；原 Notebook 未 shift 的算法只留在 offline audit |
| VWAP | provider minute VWAP × volume，每天累计重置 | 不使用 HLC3；live confirmed 标志与完成时间双检查 |
| Signal clock | 只在完成时刻的 session elapsed 30 分钟边界计算 | 没有当根 close 的提前成交 |
| Fill | 下一分钟 open quote 出现后才发单；Nautilus 撮合 | golden 每笔 fill.timestamp=signal.timestamp+1ns |
| EOD | 日历 timer 提前 cancel/flatten；close 后检查仓位/订单 | 不依赖不存在的 index==390 |
| 重复/迟到数据 | model 拒绝重复/缺失；wrapper 忽略已处理 confirmed bar，未更新 watchdog | 不让重复推送产生第二个信号 |
| Train/test | 原样按交易日顺序 80/20；test 自行预热 14 日；固定 Notebook 参数 | 一次 OOS 执行；之后只审计既有报告 |
| 参数搜索 | Rust sequential sweep，train Sharpe 选择并先保存 selected_config，再 test 一次 | 已提供入口；本轮未重新优化参数 |
| 公司行动 | Alpaca adjustment=raw，保留 Notebook 原定义，无自行除息回调 | 跨除息日的 raw 噪声仍是研究限制 |
| 存活偏差 | 单一 SPY，无后验个股池选择 | 不推广到任意当前幸存股票池 |
| 完整交易日筛选 | Notebook 事后只保留 390 根的 session | **未消除的样本选择偏差**；全样本结果必须带此标签 |
| 测试期信息 | 测试收益没有参与参数选择；golden 在训练期最早 30 天 | 未对本轮 OOS 调参；Notebook 作者原研究的数据试探无法追溯 |

测试入口：`crates/trading/tests/lookahead.rs`、`intraday_parity.rs`、`intraday_semantics.rs`、`intraday_native_parity.rs`。
原代码中两个前视缺陷不会被偷偷修正后继续称为“原 Notebook 同样收益”。

真实市场中仍有延迟、缺失数据、价差变化、拒单、借券和购买力限制。此审计证明当前数据路径的因果性，
不证明模型盈利能力，也不代替账户、回线恢复及完整交易时段的 paper 验收。
