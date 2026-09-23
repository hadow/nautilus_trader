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

Run the offline regressions separately from any account session:

```bash
CARGO_INCREMENTAL=0 cargo test -p nautilus-longbridge --features dynamic-grid \
  --lib --profile dev -j 2 -- --test-threads=1
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

Validation: debug build passed; adapter library 42 tests passed, Dynamic Grid 87 passed with four
pre-existing performance tests ignored, execution tester three passed. Targeted adapter Clippy
with `--no-deps` and changed-file formatting passed. Dependency-inclusive Clippy remains blocked
by 13 pre-existing findings in `dynamic_grid/files.rs` and `momentum_pullback`; workspace formatting
also has pre-existing import differences outside this change. This is not full live readiness.

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
