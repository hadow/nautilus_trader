# Longbridge

Longbridge is an experimental Rust-native adapter backed by the official
[`longbridge`](https://crates.io/crates/longbridge) Rust SDK. The adapter keeps the SDK's transport
ownership intact: `QuoteContext` supplies public market data, while `TradeContext` supplies order,
execution, account, position and private push APIs.

## Capabilities

| Capability                | Status    | Notes                                                                                   |
| ------------------------- | --------- | --------------------------------------------------------------------------------------- |
| Top‑of‑book quotes        | Supported | Derived from the best levels of the depth stream.                                       |
| 10‑level depth            | Supported | Published as `OrderBookDepth10` snapshots.                                              |
| Trades                    | Supported | The SDK does not expose aggressor side, so it is reported as unknown.                   |
| External bars             | Supported | Only Longbridge candlestick periods and `LAST` price bars.                              |
| Instrument definitions    | Supported | Static security metadata plus explicitly configured exact price increments.             |
| Historical bars           | Supported | Up to 1,000 unadjusted Longbridge candlesticks per request.                             |
| Submit orders             | Supported | Market, limit, market‑if‑touched and limit‑if‑touched.                                  |
| Modify and cancel         | Supported | A venue order ID is required.                                                           |
| Account balances          | Supported | Currency cash records and account‑level margin requirements are aggregated by currency. |
| Order/fill reconciliation | Supported | Today's and historical endpoints are merged and deduplicated.                           |
| Stock positions           | Supported | Positive, negative and flat quantities map to net positions.                            |
| Private order push        | Supported | Each notification refreshes authoritative order and execution records.                  |

The SDK execution record does not expose commission or liquidity side. Fill reports therefore use
zero commission in the order currency and `NO_LIQUIDITY_SIDE`; downstream accounting should replace
these values from a broker statement if exact fee reconciliation is required.

## Confirmed minute bars

Dynamic Grid consumes confirmed one-minute `BarWithVwap` events. If an SDK-confirmed candle
arrives up to one second before its interval ends on the local clock, the adapter queues it until
that boundary instead of discarding it. The event loop continues receiving quotes and trades;
the timer rechecks the clock before dispatch. Larger clock differences are rejected with the
symbol, source start, interval end and receive timestamp in the diagnostic.

Only one confirmed candle per bar subscription can wait at a time. Duplicate and older confirmations
are suppressed within the subscription; unsubscribe, disconnect and stop discard pending candles.
The bar keeps its interval-end `ts_event`, while `ts_init` records initialization at dispatch.
Source price and VWAP precision and the strict historical parser are unchanged. This does not
promote unconfirmed updates to confirmed data or change the ordinary streaming `Bar` channel.

## API limits

The adapter applies process-wide Longbridge limits across every client instance:

- quote calls are limited to 10 per rolling second and five concurrent requests;
- only one quote connection can be held in a process;
- a quote connection reserves at most 500 unique symbols, with multiple data types for one symbol
  counting once;
- trade calls are limited to 30 per rolling 30 seconds and start at least 20 milliseconds apart.

Subscription slots are conservatively retained until the data client is reset, preventing an
asynchronous unsubscribe followed by a subscribe from briefly exceeding 500 server-side symbols.
The guards cannot coordinate separate operating-system processes; do not run multiple nodes with
the same Longbridge account unless an external process supervisor enforces the account-wide limits.
See the [official Longbridge rate limits](https://open.longbridge.com/docs#rate-limit).

## Equity adapter comparison

The in-tree adapters expose three materially different forms of equity access. Only Interactive
Brokers and Longbridge route orders for cash equities. Databento supplies equity data but does not
execute orders. Architect AX and Hyperliquid expose equity-linked perpetual derivatives, not shares
in the underlying companies.

| Adapter             | Exposure                        | Market data                          | Execution            | Instrument definitions                          |
| ------------------- | ------------------------------- | ------------------------------------ | -------------------- | ----------------------------------------------- |
| Interactive Brokers | Cash equities and other assets  | Live and historical                  | Broker‑routed orders | Loaded from TWS or IB Gateway contract details  |
| Databento           | US cash‑equity datasets         | Rich live and historical schemas     | Not available        | Decoded from definition records                 |
| Longbridge          | Cash equities available to user | Live and historical bars, live ticks | Broker‑routed orders | Loaded from static security metadata and config |
| Architect AX        | Equity‑linked perpetuals        | Derivative order book and trades     | Derivative orders    | Loaded from AX                                  |
| Hyperliquid         | HIP‑3 equity‑linked perpetuals  | Derivative order book and trades     | Derivative orders    | Loaded from Hyperliquid                         |

### Example coverage comparison

| Adapter             | Rust examples                       | Python examples                            | Equity‑relevant gap                          |
| ------------------- | ----------------------------------- | ------------------------------------------ | -------------------------------------------- |
| Interactive Brokers | Data and execution testers          | Testers, contract and historical downloads | Requires a running TWS or IB Gateway         |
| Databento           | Data tester                         | Data tester and historical workflows       | No execution client                          |
| Longbridge          | Data, paper‑execution and grid      | Data and paper‑execution testers           | Historical quotes and trades are unavailable |
| Architect AX        | Data and execution testers          | Testers and strategy examples              | Trades equity‑linked derivatives, not stock  |
| Hyperliquid         | Data, execution and outcome testers | Data, execution and outcome testers        | Trades equity‑linked derivatives, not stock  |

Interactive Brokers is the closest in-tree comparison because both adapters combine stock market
data, account state, positions, reconciliation, and execution. Its instrument provider is broader
and can resolve contract metadata dynamically. Longbridge has a simpler direct OAuth connection
and official SDK contexts. Longbridge static security metadata supplies the symbol, currency and
board lot; the application supplies the exact price increment which the OpenAPI response omits.

Databento is complementary to Longbridge rather than interchangeable with it. It can provide
high-quality US equity definitions and historical data, but symbols and venue identity must be
mapped deliberately before using those definitions with `*.LONGBRIDGE` execution instruments.

## OAuth 2.0 authentication

The adapter uses the OAuth 2.0 authorization-code flow recommended for new Longbridge integrations.
Register a public OAuth client whose redirect URI matches the local callback port:

```bash
curl -X POST https://openapi.longbridge.com/oauth2/register \
  -H "Content-Type: application/json" \
  -d '{
    "redirect_uris": ["http://localhost:60355/callback"],
    "token_endpoint_auth_method": "none",
    "grant_types": ["authorization_code", "refresh_token"],
    "response_types": ["code"],
    "client_name": "NautilusTrader Longbridge adapter"
  }'
```

Set the returned public client ID or pass it to both client configurations:

```bash
export LONGBRIDGE_OAUTH_CLIENT_ID="..."
```

On the first connection, the adapter logs the authorization URL and waits on
`http://localhost:60355/callback`. Open the URL in a browser and approve access. The official SDK
stores the resulting token under `~/.longbridge/openapi/tokens/<client_id>`, refreshes it
automatically and reuses it on subsequent runs. The adapter does not accept legacy app secrets or
static access tokens.

The `LONGPORT_OAUTH_CLIENT_ID` alias is accepted for prefix compatibility. An explicit
`oauth_client_id` takes precedence over the environment. Set `oauth_callback_port` only when the
same port is registered in the client's redirect URI.

Paper trading is selected independently in the execution configuration:

```python
from nautilus_trader.adapters.longbridge import LongbridgeDataClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeDataClientFactory
from nautilus_trader.adapters.longbridge import LongbridgeExecClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeExecutionClientFactory


data_config = LongbridgeDataClientConfig(
    oauth_client_id="...",
    enable_overnight=True,
    instrument_price_increments={
        "AAPL.US.LONGBRIDGE": "0.01",
        "700.HK.LONGBRIDGE": "0.001",
    },
)
exec_config = LongbridgeExecClientConfig(
    oauth_client_id="...",
    papertrading=True,
    outside_rth=False,
)
```

Importing `nautilus_trader.adapters.longbridge` registers both factory and configuration extractors
with the PyO3 runtime registry.

## Symbols and instruments

Longbridge wire symbols keep their market suffix, for example `AAPL.US`, `700.HK`, `600519.SH`,
`000568.SZ` and `D05.SG`. Nautilus instrument IDs append the adapter venue:

```text
AAPL.US.LONGBRIDGE
700.HK.LONGBRIDGE
```

The Longbridge static-security response includes currency and board lot size, but not the minimum
price increment. Configure `instrument_price_increments` with fully qualified Nautilus instrument
IDs and exact decimal strings. Its keys define the instruments loaded on connection and returned by
instrument requests. The adapter rejects malformed IDs, non-Longbridge venues, zero or negative
increments, and unsupported non-equity boards; it never infers a tick size from recent prices.

For example:

```python
data_config = LongbridgeDataClientConfig(
    instrument_price_increments={
        "AAPL.US.LONGBRIDGE": "0.01",
        "700.HK.LONGBRIDGE": "0.001",
    },
)
```

Definitions use the exact configured price increment and the broker-reported currency and board
lot. Board lot is recorded as `lot_size`, not as `min_quantity`, because odd-lot eligibility can
differ by market and order side.

## Historical bars

`RequestBars` supports the same external `LAST` intervals as live candlesticks. Requests use
unadjusted prices and all enabled Longbridge trading sessions. Returned bars are sorted,
deduplicated, and filtered to the inclusive UTC `start` and `end` bounds. Requests with an `end`
use a backward offset query so the provider-side count is anchored to that boundary.

The Longbridge endpoint returns at most 1,000 candlesticks, so `limit` must be between 1 and 1,000;
an omitted limit requests 1,000. Historical quote ticks, trades and order-book data are not exposed
by this adapter.

## Execution semantics

The execution client uses netting OMS semantics and accepts `CASH` or `MARGIN` account types.
Post-only and reduce-only orders are denied locally because the mapped Longbridge stock order API
does not preserve those instructions. GTD orders pass the Nautilus expiration date to the SDK.

The client distinguishes three outcomes:

- local validation failure: emits `OrderDenied` before a network request;
- authoritative Longbridge OpenAPI rejection: emits the corresponding rejection event;
- transport or protocol failure after dispatch: logs an ambiguous outcome and leaves the order for
  reconciliation, rather than emitting a false terminal event.

`client_order_id` is sent as both Longbridge's idempotency key and order remark. A broker session can
then associate private updates with locally submitted orders; external orders remain valid
reconciliation reports without a fabricated client order ID.

An order query can recover a missing venue order ID by matching the exact client ID in the broker
remark. Multiple matches, conflicting symbol/side identities, and an absent result fail closed;
absence is not treated as proof of rejection. Restored native orders provide the identity map after
restart. Concurrent explicit queries for the same client ID share one in-flight query, including
time spent waiting for a rate-limit permit. Transport failures and the broker's internal-error
response do not trigger automatic resubmission. The broker's idempotency cache lasts only ten
minutes; it does not replace durable
strategy state. See [submit-order semantics](https://open.longbridge.com/docs/trade/order/submit).

Private pushes and explicit queries deliver order snapshots and their executions together through
Nautilus `OrderWithFills`. Native execution handles fill deduplication. When the execution sum differs
from the cumulative order quantity, reconciliation waits for another query instead of guessing a
fill. Keep periodic native reconciliation enabled: API snapshots are not transactionally consistent.

Trade calls share a process-wide rolling quota and one in-flight permit, held until 25 ms after the
response. Dispatch spacing alone is insufficient: network jitter can bunch concurrent requests at
the broker and trigger `429003`. This does not coordinate separate processes or retry mutations.
Quote padding such as `12.970` is normalized without rounding genuine sub-cent prices.

## Dynamic Grid paper acceptance

Build the debug runner and perform a read-only check first:

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-longbridge --features dynamic-grid \
  --bin longbridge-dynamic-grid -j 2
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_paper.json --check-paper
```

The check uses OAuth with `papertrading=true`, the shared trade rate limiter, and native account,
position and order parsers. It reports counts without printing credentials or balances. It does not
submit/cancel orders, acquire the strategy checkpoint lock, or claim that execution is accepted.
`fresh_account_candidate=false` means existing positions or active orders need investigation before
starting a fresh strategy. Even `true` does not validate a checkpoint or authorize trading.

The snapshot also reports `startup_blockers` for positions/orders outside the configured universe,
broker inventory/orders without a checkpoint, and missing portfolio-currency cash records.
A configured symbol is not proof of strategy ownership. The cash check compares configured capital
with native free cash only; it never adds frozen cash, settling cash, financing or another currency.
Longbridge reports these as separate [account fields](https://open.longbridge.com/docs/trade/asset/account),
not per-order reservation evidence. `reservation_overlap_verification=NOT_PERFORMED` means the
check cannot remove the strategy's conservative pending-order reservations.

`--run` now enforces these blockers before building a Paper/Live node. It first locks and validates
the local checkpoint, then queries the account. A fresh start additionally requires free cash to
cover the configured capital and no unattributed frozen cash, unless the Paper-only waiver below
is explicitly enabled. Restoring a valid checkpoint does
not require funding the initial budget again: native reconciliation and order risk checks must
still run, and lack of spare cash must not by itself prevent recovery for inventory reduction.
Do not create a dummy checkpoint to evade these checks.

The read-only command remains diagnostic, not a replacement for native startup reconciliation or
an order permission. An empty blocker list or exit code zero does not mean readiness to trade.
The queries are sequential snapshots, not an atomic account view; order counts cover the existing
`today_orders` query, not an independently verified full-history audit. Checkpoint existence alone
does not validate its contents. No checkpoint is created, imported, deleted or unlocked by this check.

For a separately prepared `mode=Live` configuration, `--check-live` performs the same read-only
queries without the SDK's Paper-only account guard; it cannot be combined with `--run` or `--live`. It does not place
orders. Trading still requires both `mode=Live` and `--run --live`. A bare `--live`, duplicate flags,
or a check flag for the wrong mode is rejected before connecting. The repository Paper example
is not converted to a live configuration by any of these commands.

Run the offline regressions separately from any account session:

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-longbridge --features dynamic-grid \
  --lib --bin longbridge-dynamic-grid --profile dev -j 2 -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p nautilus-trading --features examples \
  --lib dynamic_grid --profile dev -j 2 -- --test-threads=1
```

The adapter regression sends a submission through the SDK to a local HTTP fixture, receives an
ambiguous server error, then queries the same client identity without a venue ID. It feeds the real
broker-shaped partial execution into the native engine twice and verifies one applied fill, followed
by a partial cancellation. This tests production parsing and reconciliation, not real broker fills.
Other regressions cover concurrent rate permits, no mutation retries, and retained grid reservations
after confirmation timeout.

For a bounded adapter execution check, the existing Rust execution tester is Paper-only and defaults
to printing a plan without connecting. It uses `F.US`, one share, a $300 per-order cap and the shared
trade limiter. Verify the symbol contract before changing its constants. The tester checks for
conflicting symbol orders/inventory and uses the broker trading calendar to require the US regular
session. Execute the sell step only after verifying the buy's result:

```bash
CARGO_INCREMENTAL=0 cargo build -p nautilus-longbridge --features examples \
  --example longbridge-exec-tester -j 2
target/debug/examples/longbridge-exec-tester --check-paper
target/debug/examples/longbridge-exec-tester --paper-buy
# Inspect PAPER_ACCEPTANCE and the broker account before the separate sell step.
target/debug/examples/longbridge-exec-tester --paper-sell
```

Each execution run submits one marketable limit, stops after 60 seconds, individually cancels its
remaining orders and queries the broker again. It never auto-closes with a market order and never
retries a failed acceptance run. A successful result requires one native filled order with execution
IDs, no active symbol orders and the expected broker quantity (one after buy, zero after sell).
A timeout or incomplete result requires read-only reconciliation before another execution run.
This tests the shared adapter/native execution path, not a complete Dynamic Grid restart scenario.

Full strategy acceptance still needs an isolated account or an explicitly reconciled checkpoint.
Do not run the default portfolio merely to force a fill. Record the following evidence before
considering live deployment:

1. Submitted client ID, broker order ID and real execution IDs match across push and query.
2. Partial and terminal fills update position, cash and grid inventory once, including replay.
3. Cancellation is broker-confirmed before reservations disappear or replacement orders start.
4. After reconnect/restart, orders and positions match the broker; no new entry occurs while unknown.
5. Rate-limit/timeout incidents retain reservations and stop entries; no repeated submission occurs.
6. Shutdown is followed by a broker check for residual orders and inventory, not just a clean exit code.

Dynamic Grid keeps timeout risk latched; successful connectivity or a recovered fill does not
automatically authorize new entries. Resolve discrepancies before using the existing risk-reset
workflow. Do not delete checkpoints to bypass recovery. Broker fee totals still require statement
reconciliation, and the limiter coordinates only one process. Do not intentionally overload the
broker or disconnect a real-money account to induce these failure scenarios.

### Bounded Paper result (2026-09-23)

The user-authorized acceptance used `F.US`, one share, a $300 maximum buy notional, cash-mode risk
checks and US regular-session limits. No live-money orders or default grid portfolio were started.

| Check                                      | Result                 | Evidence                                                                                                                                                                                                      |
| ------------------------------------------ | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Paper buy                                  | PASS                   | 14:30:34 UTC; one share at $12.875; native Submitted, Accepted and Filled with a broker execution ID.                                                                                                         |
| Separate-process recovery                  | PASS                   | Seller startup restored the earlier buy execution and one-share position before submitting its sell.                                                                                                          |
| Paper sell                                 | PASS                   | 14:32:27 UTC; one share at $12.8839; native Filled with a different broker execution ID.                                                                                                                      |
| Final broker check                         | PASS                   | `F.US` quantity 0, active orders 0; account-wide today orders 2, executions 2, active orders 0.                                                                                                               |
| Limiter                                    | PARTIAL                | Encountered real `429003` before strategy startup; after serialization/cooldown, both execution runs completed without this error. Concurrent-process quotas remain untested.                                 |
| Ambiguous submission / partial-fill replay | PASS (offline)         | SDK HTTP fixture recovers by client remark, then native execution applies the real-shaped fill only once. No real broker timeout was induced.                                                                 |
| Acceptance reporter                        | PARTIAL                | Seller incorrectly counted its recovered buy as another new submission. Fixed by checking current-run Submitted events; three example tests pass. No extra trades were made to rerun this reporting-only fix. |
| Full Dynamic Grid checkpoint recovery      | NOT RUN on broker      | The round trip used the existing native execution tester, not the grid portfolio. Grid timeout/reservation regression passes offline.                                                                         |
| Statement fees / live money                | NOT VERIFIED / NOT RUN | Broker execution queries omit commission; zero reported commission is not proof of zero account fees.                                                                                                         |

Earlier attempts exposed quote-padding precision and server-arrival rate-limit issues; neither
attempt submitted a broker order. The final account still has its original unrelated position;
`fresh_account_candidate=false` is therefore expected, not permission to start a fresh grid over it.
Raw local logs can contain account balances and must not be committed or published.

### Startup gate result (2026-09-25)

The Paper SDK check still reports one nonzero configured-symbol holding without a grid checkpoint,
plus unattributed frozen cash. Startup is blocked by `BROKER_STATE_WITHOUT_CHECKPOINT` and
`UNATTRIBUTED_FROZEN_CASH`. No position was adopted or sold; no strategy node or live account was
started. The next broker acceptance requires an isolated Paper account, or an independently audited
ownership/recovery process. Do not solve this by changing symbols or ignoring the existing position.

For an already running, correctly reconciled node, use its existing SIGINT/SIGTERM shutdown path.
It stops strategies, requests cancellations, drains residual events and persists final state.
Stopping is not flattening, and a cancellation request is not a broker confirmation. After exit,
query the broker again, check retained inventory and every unresolved order against the checkpoint,
and do not restart while there is a discrepancy. Preserve the checkpoint, configuration and local
logs; do not delete them, send repeated mutations or automatically reset latched risk.

Full broker restart/timeout acceptance, statement fee reconciliation, alert delivery and a bounded
live rollout remain unaccepted. See the [Chinese implementation record](dynamic_grid_review_fixes.zh-CN.md#启动门禁与部署验收推进2026-09-25).

Validation: debug build passed; adapter library 42 tests passed, Dynamic Grid 87 passed with four
pre-existing performance tests ignored, execution tester three passed. Targeted adapter Clippy
with `--no-deps` and changed-file formatting passed. Dependency-inclusive Clippy remains blocked
by 13 pre-existing findings in `dynamic_grid/files.rs` and `momentum_pullback`; workspace formatting
also has pre-existing import differences outside this change. This is not full live readiness.

### Six-symbol 15-minute configuration (2026-09-25)

`dynamic_grid_paper_six.json` and `dynamic_grid_live_six.json` cover AAPL, MSFT, NVDA, TSLA, AMZN
and META. Both load `crates/backtest/examples/dynamic_grid_comparison.json` and its existing
per-symbol files. The resolved strategy uses 15-minute Regime bars, eight-bar confirmation and
no Grid-scale candidate. AAPL/MSFT use `instuments/comparison/`; the remaining symbols use
`instuments/`. Backtest dates and CSV paths do not restrict live trading or preload live indicators.
Changing those shared files also changes the deployment configuration; stop and review checkpoint
compatibility before editing them for another experiment.

These are the existing research-budget settings, not a small real-money canary: initial capital
is USD 100,000, maximum total exposure 70%, minimum cash reserve 20%, portfolio drawdown gate 25%
and daily-loss gate 7%. Allocation weights are 18% AAPL, 18% MSFT, 14% NVDA and 10% each for
TSLA/AMZN/META; shared risk gates can reduce actual allocations. Loss gates do not guarantee
maximum realized losses. No risk limits were relaxed for this deployment configuration.

The two new runners use distinct trader IDs, checkpoint paths and recovery contexts. The original
two-symbol Paper configuration and its checkpoint are unchanged. Do not rename, copy or delete an
old checkpoint to switch universes, and do not run both old and new nodes on the same account.
`account_id` is a Nautilus identity, not a selector for a different brokerage account; routing uses
OAuth and the explicit Paper/Live mode.

From the repository root, with `LONGBRIDGE_OAUTH_CLIENT_ID` exported:

```bash
# Offline validation only; prints effective strategy configuration.
target/debug/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_paper_six.json
target/debug/longbridge-dynamic-grid crates/adapters/longbridge/examples/dynamic_grid_live_six.json

# Read-only broker diagnostics; inspect startup_blockers, not just the exit code.
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_paper_six.json --check-paper
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_live_six.json --check-live
```

Both six-symbol native configuration checks passed, and resolved per-symbol parameters and
portfolio risk settings match the source and each other. Four wrong-mode/flag cases were rejected
before connecting. No Rust logic changed, no additional backtest or rebuild was run, and the
previously built debug runner was used. No grid process was running at the time of inspection.

Both read-only broker checks returned `BROKER_STATE_WITHOUT_CHECKPOINT` and
`UNATTRIBUTED_FROZEN_CASH`: one nonzero in-universe holding, zero active orders, and no six-symbol
checkpoint. Available USD covered configured capital, but that does not establish ownership of
the existing holding or frozen cash. Neither startup reconciliation nor broker execution was
performed. Do not bypass these blockers; use an isolated account or a separately audited ownership
and recovery procedure. Broker state can change, so repeat the check before any later launch.

The local filesystem also had only about 127 MiB free. Free operational space before running a
node that must persist orders and inventory. The current grid startup subscribes to new confirmed
bars without requesting historical warmup; a fresh 15-minute ADX/confirmation state may require
multiple sessions. Launching a process is not a promise of a trade on its first evening.

Only after resolving account ownership, frozen-cash attribution, disk capacity and the remaining
acceptance requirements, choose the appropriate command below. These commands were **not run**:

```bash
# Broker Paper account, not local simulation.
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_paper_six.json --run

# Real capital: requires Live configuration and both explicit flags.
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_live_six.json --run --live
```

Use Ctrl-C for the normal shutdown path and verify broker orders/inventory afterwards. Stopping
does not flatten positions. Preserve the matching checkpoint; never restart with a fresh state
merely to evade a recovery or risk failure.

### 外部持仓隔离（2026-09-25）

Runner 支持显式配置 `isolated_instruments`，例如：

```json
"isolated_instruments": ["AAPL.US.LONGBRIDGE"]
```

这是**整个标的只读隔离**，不是把 `enabled` 改为 `false`。隔离标的仍须在
`instruments` 中提供行情与行业配置，但不会构建网格引擎；不会买入、卖出、撤单或
被 kill switch / Flatten 接管。其原有配置额度不自动转给其他股票。
当前 `dynamic_grid_live_six.json` 已隔离 AAPL，因此实际可交易的是另外五只股票。
六标的 Paper 配置经独立 `--check-paper` 核对后也隔离 AAPL，不接管已有 20 股。

开启隔离后，runner 不再按标的认领未知外部订单。已有策略订单依赖原子检查点恢复
原生归属；外部持仓由 Nautilus 对账保持为 `EXTERNAL`。隔离只支持配置池内、同报价币种
的多头股票；池外持仓、空头、币种不匹配和未知未终结订单继续阻止启动。
仍有手工委托的隔离标的不会自动撤单或放行。

外部持仓市值进入总暴露、行业、相关性和同时持仓标的数限制，不增加可交易现金。
首次新鲜有效 bid 建立风险估值基线，后续外部浮盈亏纳入组合亏损风控，但不混入
策略收益报告或网格周期。风险预算仍以配置资本为基准，不因已有外部资产而自动放大。
缺少新鲜报价时禁止所有新增买入；有真实网格库存覆盖的卖出仍按原规则处理。

检查点保存隔离名单、数量和风险价格基线。重启清空报价新鲜度；券商数量与基线不符、
包括手工买卖或公司行动造成的数量变化时，停止新增交易并要求重新核对，不自动接管差额。
不得删除检查点或直接修改隔离名单来绕过这项保护。

只读检查现在合并今日与历史接口的订单，并输出 `holdings`、`isolated_positions`、
`cash_check.available_cash`、`frozen_cash`、`settling_cash`、`frozen_transaction_fees`
及待买限价单名义金额。输出包含账户敏感数据，不要提交或公开原始日志。
历史接口有返回窗口，未找到活动订单并不证明冻结额无效；手续费金额相同也不等于
逐笔预留已验证。`freeze_attribution=UNVERIFIED` 时保留 `UNATTRIBUTED_FROZEN_CASH`
新启动门禁（下文的 Paper 显式豁免除外）；冻结和待交收资金不会加回可用现金。

本次 SDK/OAuth 只读核对已排除“未隔离持仓无检查点”这一阻塞，但冻结资金来源仍未
得到订单证据支持，**未启动 Paper 或 Live 交易节点**。CLI 与 runner 的账户快照也存在
差异，不能拿不同授权上下文的余额互相佐证。部署前应由同一 OAuth 授权下的券商账户
和账单确认资金归属；本改动不宣称完成真实券商恢复或成交验收。

离线验证：Dynamic Grid 单元回归 193 项、原生集成 87 项、Adapter 库 42 项、runner
门禁 15 项通过；4 项原有性能测试未执行。debug 构建、修改文件格式检查和 Adapter
定向 Clippy 通过。核心 crate 的 Clippy 仍有 12 项未改动的 `momentum_pullback`
既有错误；未宣称全仓检查通过。没有运行季度回测或真实券商交易验收。

### 模拟账户冻结资金豁免

`paper_allow_unattributed_frozen_cash` 默认 `false`，仅 `mode=Paper` 可设为 `true`；
Live / Sandbox 配置启用它会在连接券商前报错。六标的 Paper 配置已开启该选项，
Live 配置保持严格门禁。

该选项只将 `UNATTRIBUTED_FROZEN_CASH` 降为
`startup_warnings=["PAPER_UNATTRIBUTED_FROZEN_CASH_WAIVED"]`，运行时也打印警告。
`freeze_attribution` 仍为 `UNVERIFIED`，不表示冻结款已核清或可以使用。
可用现金不足、未知订单、未隔离持仓、检查点校验和原生启动对账均不豁免。
要恢复严格检查，将此选项改回 `false`。

Paper 使用真正的 Longbridge Adapter 提交券商模拟委托，不是本地 Sandbox 撮合。
预检和执行均启用 SDK `papertrading=true`；SDK 使用 `x-papertrading: true`，
由服务端拒绝实盘账户令牌。未启用这个标记不代表账户一定是实盘，因此模拟账户应使用
Paper 配置和 `--run`，不要沿用 Live 配置及 `--live`。

```bash
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_paper_six.json --check-paper

# 会提交券商模拟订单；先检查上方输出，再由操作员启动。
target/debug/longbridge-dynamic-grid \
  crates/adapters/longbridge/examples/dynamic_grid_paper_six.json --run
```

## Examples and tests

The Rust examples construct complete `LiveNode` instances. The data tester registers a sample
`AAPL.US` equity; the bounded Paper execution tester uses `F.US`. The grid example registers the
three equities described below:

```bash
cargo run -p nautilus-longbridge --features examples --example longbridge-data-tester
cargo run -p nautilus-longbridge --features examples --example longbridge-exec-tester
cargo run -p nautilus-longbridge --features examples --example longbridge-grid-mm
```

Equivalent Python examples are available under `examples/live/longbridge/`:

```bash
python examples/live/longbridge/data_tester.py
python examples/live/longbridge/exec_tester.py
```

The data tester subscribes to quotes, 10-level depth, trades and one-minute external bars. The Rust
execution tester enables reconciliation and exercises one-share Paper limits and private order
push, as described above. It avoids unsupported post-only and reduce-only flags. The Python
execution example is separate; inspect its settings before running it.

The grid example runs an independent built-in Rust `GridMarketMaker` for each of `AAPL.US`,
`MSFT.US` and `NVDA.US`. Each symbol uses three levels per side, ten shares per order, a 60-share
maximum position, 25 bps grid spacing and a 10 bps requote threshold. The node queries Longbridge's
trading-day and trading-session APIs and runs until the current US regular session closes. It handles
US daylight-saving time and Longbridge half-trading days, and refuses to start on a non-trading day
or after the regular close.

The per-symbol strategy IDs, order tags, reconciliation claims and position limits are distinct.
The margin account is required because each symmetric grid can open short positions. The example
does not impose an aggregate account-level position or notional cap.

At startup, the three strategies can submit up to 18 resting orders in total. Longbridge does not
support post-only or reduce-only instructions, so the example disables both. Consequently, limit
orders can execute immediately and the asynchronous cancel-and-close sequence cannot guarantee a
flat account. Use an isolated paper account, confirm its available buying power, and inspect the
account after shutdown before adapting the example for live trading.

The embedded `AAPL.US`, `MSFT.US` and `NVDA.US` definitions are examples, not a security master.
Verify every raw symbol, currency, price increment, lot size and minimum quantity against current
venue rules. Replace them with a catalog or custom provider definition for production use.

Run its focused Rust and PyO3 tests with:

```bash
cargo test -p nautilus-longbridge
cargo test -p nautilus-longbridge --features python --test python
```
