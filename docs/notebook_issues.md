# Notebook 问题与迁移决策

以下为读取实际 cells 后的发现，不是对注释的推测。

| 问题 | Notebook 实际行为 | 因果解释 / Rust 处理 | 对回测的影响 |
| --- | --- | --- | --- |
| EOD off-by-one（21/57） | index 为 0..389，但退出条件为 390 | compatibility 保留此诊断；生产使用日历 timer 提前退出并核验 | 原代码漏收最后清仓费用，最后仓位隐式按天重置 |
| T+1 未实现（25） | exec_minute_index 仅写入未使用；当前 position 乘当前 open→next open 收益 | 兼容审计单列原收益；Nautilus 用完成事件后下一 open | 原 PnL 在知道 close 前已获得该分钟收益，可能明显高估 |
| Volatility lookahead（25） | rolling std 没有 shift，直接用当前 date，包括当日最后 close | 生产使用前 14 个已完成日收益；兼容审计仅离线保留 | 当天所有仓位依赖未来收盘 |
| 止损不看持仓方向（21/57） | long_stop OR short_stop，且 entry 覆盖 stop | 明确保留实际事件优先级；独立测试 | RVOL 未通过时即使自身方向 stop 未破，也可能 Flat |
| VWAP 不是入场过滤（57） | 只 breakout + RVOL；优先于 VWAP stop | 移除旧 Rust 额外 VWAP entry 条件 | 否则属于擅自改变 alpha |
| 逐分钟再平衡式 PnL（25） | compound 每分钟 signed return，再乘日 leverage | 单独原收益审计；生产用持有固定股数的实际 fills/现金 ledger | 与固定 shares 的成交 PnL 不会逐美元一致 |
| 同步费用口径（25） | 只计算 intraday position diff 与 initial entry，没加 EOD flatten | 生产所有真实成交收费，包括反手两侧与 EOD | 成本及交易数不同 |
| 30 分钟标签（14/57） | 10:00 start 标签 close 到 10:01 才可知 | 区分 notebook label 与 production completion semantics | 默认生产决策与 notebook 标签相差一分钟 |
| 半日市/缺失日删除（13） | 仅保留恰好 390 行的日子 | compatibility 复现筛选；生产用显式日历和完整性检查 | 训练样本和历史窗口不同，删除条件事后才能知道 |
| 窗口独立（56/25） | sigma lookback 可变；RVOL 和 daily vol 固定 14 | 分别建配置，默认忠实 14 | 旧 Rust 统一窗口不等价 |
| OOS 重置历史（16/64） | test 单独 generate，前 lookback 日无信号 | 兼容 split 重置；生产可延续已知训练末尾历史，须标注 | OOS 交易起始日期与收益不同 |
| 输入价格（15） | 使用供应商 bar VWAP | 保留 minute VWAP/turnover，缺失时拒绝 strict parity | OHLC typical price 不可当作相同公式 |
| 原始 CSV 未提供 | ipynb 未嵌入 spy_data.csv | 经用户授权，用 Notebook 凭证按原请求下载 Alpaca；取得同样 1,985,051 行 | 原代码 train/test 指标已复现；供应商历史修订仍可能存在，不能宣称逐字节等同遗失 CSV |
| Notebook 凭证 | 数据下载 cell 含硬编码凭证 | 只在离线下载器内存中读取 API_KEY/SECRET_KEY；不执行整份 Notebook，不落盘凭证；golden 只执行选定纯计算函数 | 凭证不应进入版本库；原凭证宜在所属账户撤销/轮换 |

论文第 9、13、15 页分别说明半小时执行、VWAP stop 与前 14 日波动率；Notebook 的实现与其文字意图
有差异。因此“复现原输出”和“可交易因果实现”不是同一个验收项，两者分别报告。
