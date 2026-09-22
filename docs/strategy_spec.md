# Notebook 日内动量数学规格

唯一策略数学来源：用户的 `intraday trading strategy final.ipynb`，68 个 cells，作者署名
Aleksandar Milosavljevic。论文 `ssrn-4824172.pdf`（43 页，版本 2025-09-22）用于解释研究背景，
不以论文不同参数覆盖 Notebook。迁移对象为 cells 56、57、25、64 的 RVOL + refined stop + dynamic sizing。

## 特征与信号

输入为 SPY 常规交易时段一分钟 OHLCV 和 **provider minute VWAP**。
Notebook 时间戳为分钟起点；收盘价在该时间加一分钟后才可知。内部边界使用 UTC Unix nanoseconds，
由显式 NY 交易日历提供开闭市时刻；生产模式接受日历中的提前收市，不以自然日替代交易日。

1. 每日 first open 为 O，previous complete session last close 为 P。
2. 每分钟 `move=abs(close/O-1)`；sigma 为之前 L 个交易日相同分钟的 move 均值，默认 L=14。
3. `upper=max(O,P)*(1+VM*sigma)`；`lower=min(O,P)*(1-VM*sigma)`；VM=1。
4. `AVWAP=sum(provider_vwap*volume)/sum(volume)`，每天重置；零累计量时未定义。
5. RVOL 为当前分钟量除以之前 **14** 天同分钟均量；该窗口独立于 L。Notebook 缺失时填 1。
6. 信号仅在半小时决策点计算：突破 upper 且 RVOL>=1 为 Long；跌破 lower 且 RVOL>=1 为 Short。
7. refined stop 标志：`long_stop=close<max(upper,AVWAP)`，`short_stop=close>min(lower,AVWAP)`。
8. **Notebook 的实际优先级**：非零 entry 优先；否则只要 long_stop 或 short_stop 任一成立就 Flat；
   否则保留之前状态。这不同于“按持仓方向判断止损”，也不等同于“入场必须站上 VWAP”。保留并测试。
9. 不添加任何趋势、regime、SLC、EMA 或其他 alpha 过滤器。

## 仓位与执行

日波动率为 14 个日收益的样本标准差（ddof=1），不足时 leverage=1；
`leverage=min(4,0.03/vol)`，零波动为上限。生产模式只使用截至前一交易日收益。
`quantity=floor(AUM*leverage/day_open/lot)*lot`；价格、量、资金、成本使用 Decimal / Nautilus 领域类型。
生产安全上限只约束执行，不篡改数学信号。

用户要求的因果回测：T close 产生目标，下一根 bar 的 open 才可成交；实际账户记录真实 fill。
所有非 live 模式都不创建 broker 执行路由。paper 用已有 Nautilus sandbox；dry-run 只记录理论订单。
日终按日历提前 cancel、flatten，并验证 flat；不能依赖新 bar 才退出。

## 兼容与生产边界

Notebook 实际代码不满足其 T+1 注释，也含当日波动率泄漏；原样收益只作为单独 compatibility 审计，
绝不能进入 live。生产差异见 `notebook_issues.md`。特征和 entry/stop 优先级忠实保留；执行时间、
已知信息边界及日终清算显式纠正。

Notebook compatibility 按 09:30 标签的 index=0、10:00 标签的 index=30 决策，后者实际完成于 10:01。
生产 session semantics 在 10:00 完成时决策（09:59 起点 bar），直至 15:30；两者必须分开报告。

## 验证与研究

原始 Notebook 引用 `spy_data.csv`，提供的 ipynb 只有少量 head/tail、指标与图，没有完整行情。
用户随后授权读取 Notebook 中的 Alpaca API_KEY/SECRET_KEY。离线下载器取得与原请求同样的
1,985,051 行，筛选 2,484 个完整交易日；原代码的 train/test 指标已复现。凭证只用于内存中的
认证请求，不执行下载 cell，也不复制凭证到仓库。原 CSV 丢失，无法证明源文件逐字节一致。

Golden 比较 sigma、bands、AVWAP、RVOL、entry/stop/position；生产 reference ledger 与 Nautilus
比较每次 fill、quantity、PnL、日收益和权益。未来数据扰动测试覆盖 sigma、volume、volatility、订单时间。
80/20 按 session 顺序拆分；只有 train 搜索参数，OOS 一次评估。短样本不足 90 日窗口时必须报告不足，
不能把没有交易的空样本当作最优结果。
