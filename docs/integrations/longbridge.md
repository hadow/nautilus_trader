# Longbridge

Longbridge is an experimental Rust-native adapter backed by the official
[`longbridge`](https://crates.io/crates/longbridge) Rust SDK. The adapter keeps the SDK's transport
ownership intact: `QuoteContext` supplies public market data, while `TradeContext` supplies order,
execution, account, position and private push APIs.

## Capabilities

| Capability                | Status    | Notes                                                                                   |
| ------------------------- | --------- | --------------------------------------------------------------------------------------- |
| Top-of-book quotes        | Supported | Derived from the best levels of the depth stream.                                       |
| 10-level depth            | Supported | Published as `OrderBookDepth10` snapshots.                                              |
| Trades                    | Supported | The SDK does not expose aggressor side, so it is reported as unknown.                   |
| External bars             | Supported | Only Longbridge candlestick periods and `LAST` price bars.                              |
| Submit orders             | Supported | Market, limit, market-if-touched and limit-if-touched.                                  |
| Modify and cancel         | Supported | A venue order ID is required.                                                           |
| Account balances          | Supported | Currency cash records and account-level margin requirements are aggregated by currency. |
| Order/fill reconciliation | Supported | Today's and historical endpoints are merged and deduplicated.                           |
| Stock positions           | Supported | Positive, negative and flat quantities map to net positions.                            |
| Private order push        | Supported | Each notification refreshes authoritative order and execution records.                  |

The SDK execution record does not expose commission or liquidity side. Fill reports therefore use
zero commission in the order currency and `NO_LIQUIDITY_SIDE`; downstream accounting should replace
these values from a broker statement if exact fee reconciliation is required.

## Credentials

Set the official SDK environment variables:

```bash
export LONGBRIDGE_APP_KEY="..."
export LONGBRIDGE_APP_SECRET="..."
export LONGBRIDGE_ACCESS_TOKEN="..."
```

The legacy `LONGPORT_*` aliases are also accepted. Explicit constructor values take precedence over
environment values. Secrets are redacted from Rust and Python configuration representations.

Paper trading is selected independently in the execution configuration:

```python
from nautilus_trader.adapters.longbridge import LongbridgeDataClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeDataClientFactory
from nautilus_trader.adapters.longbridge import LongbridgeExecClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeExecutionClientFactory


data_config = LongbridgeDataClientConfig(enable_overnight=True)
exec_config = LongbridgeExecClientConfig(
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

## Rust examples and tests

The crate provides configuration smoke examples:

```bash
cargo run -p nautilus-longbridge --example longbridge-data-tester
cargo run -p nautilus-longbridge --example longbridge-exec-tester
```

Run its focused Rust and PyO3 tests with:

```bash
cargo test -p nautilus-longbridge
cargo test -p nautilus-longbridge --features python --test python
```
