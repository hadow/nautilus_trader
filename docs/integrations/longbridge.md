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
| Instrument definitions    | No        | Register exact definitions from a catalog or custom provider before use.                |
| Historical market data    | No        | The current adapter slice exposes live subscriptions only.                              |
| Submit orders             | Supported | Market, limit, market‑if‑touched and limit‑if‑touched.                                  |
| Modify and cancel         | Supported | A venue order ID is required.                                                           |
| Account balances          | Supported | Currency cash records and account‑level margin requirements are aggregated by currency. |
| Order/fill reconciliation | Supported | Today's and historical endpoints are merged and deduplicated.                           |
| Stock positions           | Supported | Positive, negative and flat quantities map to net positions.                            |
| Private order push        | Supported | Each notification refreshes authoritative order and execution records.                  |

The SDK execution record does not expose commission or liquidity side. Fill reports therefore use
zero commission in the order currency and `NO_LIQUIDITY_SIDE`; downstream accounting should replace
these values from a broker statement if exact fee reconciliation is required.

## Equity adapter comparison

The in-tree adapters expose three materially different forms of equity access. Only Interactive
Brokers and Longbridge route orders for cash equities. Databento supplies equity data but does not
execute orders. Architect AX and Hyperliquid expose equity-linked perpetual derivatives, not shares
in the underlying companies.

| Adapter             | Exposure                        | Market data                          | Execution            | Instrument definitions                         |
| ------------------- | ------------------------------- | ------------------------------------ | -------------------- | ---------------------------------------------- |
| Interactive Brokers | Cash equities and other assets  | Live and historical                  | Broker‑routed orders | Loaded from TWS or IB Gateway contract details |
| Databento           | US cash‑equity datasets         | Rich live and historical schemas     | Not available        | Decoded from definition records                |
| Longbridge          | Cash equities available to user | Live quotes, depth, trades, and bars | Broker‑routed orders | Must be supplied by the application            |
| Architect AX        | Equity‑linked perpetuals        | Derivative order book and trades     | Derivative orders    | Loaded from AX                                 |
| Hyperliquid         | HIP‑3 equity‑linked perpetuals  | Derivative order book and trades     | Derivative orders    | Loaded from Hyperliquid                        |

### Example coverage comparison

| Adapter             | Rust examples                       | Python examples                            | Equity‑relevant gap                         |
| ------------------- | ----------------------------------- | ------------------------------------------ | ------------------------------------------- |
| Interactive Brokers | Data and execution testers          | Testers, contract and historical downloads | Requires a running TWS or IB Gateway        |
| Databento           | Data tester                         | Data tester and historical workflows       | No execution client                         |
| Longbridge          | Data and paper‑execution testers    | Data and paper‑execution testers           | No instrument‑provider or history example   |
| Architect AX        | Data and execution testers          | Testers and strategy examples              | Trades equity‑linked derivatives, not stock |
| Hyperliquid         | Data, execution and outcome testers | Data, execution and outcome testers        | Trades equity‑linked derivatives, not stock |

Interactive Brokers is the closest in-tree comparison because both adapters combine stock market
data, account state, positions, reconciliation, and execution. Its instrument provider is broader
and can resolve contract metadata dynamically. Longbridge has a simpler direct OAuth connection
and official SDK contexts, but the current adapter requires the application to provide exact stock
metadata before subscribing or trading.

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

The Longbridge static-security response includes currency and board lot size, but not a reliable
minimum price increment across all supported security classes and markets. The adapter therefore
does not manufacture instrument definitions. Register instruments from a catalog or a custom
instrument provider before subscribing or submitting orders. This preserves exact validation of
price and quantity increments instead of inferring a tick size from a recent quote.

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

## Examples and tests

The Rust examples construct a complete `LiveNode`, register a sample `AAPL.US` equity, and run the
built-in data or execution tester:

```bash
cargo run -p nautilus-longbridge --features examples --example longbridge-data-tester
cargo run -p nautilus-longbridge --features examples --example longbridge-exec-tester
```

Equivalent Python examples are available under `examples/live/longbridge/`:

```bash
python examples/live/longbridge/data_tester.py
python examples/live/longbridge/exec_tester.py
```

The data tester subscribes to quotes, 10-level depth, trades and one-minute external bars. The
execution tester enables reconciliation and exercises market submission, a passive limit order,
cancellation, position close and private order push. Its defaults use Longbridge paper trading,
submit one-share orders, and avoid unsupported post-only and reduce-only flags.

The embedded `AAPL.US` definition is an example, not a security master. Verify the raw symbol,
currency, price increment, lot size and minimum quantity against current venue rules. Replace it
with a catalog or custom provider definition for production use.

Run its focused Rust and PyO3 tests with:

```bash
cargo test -p nautilus-longbridge
cargo test -p nautilus-longbridge --features python --test python
```
