# Longbridge adapter

Rust-native NautilusTrader adapter built on the official Longbridge OpenAPI Rust SDK.

The adapter exposes instrument definitions, historical and live candlesticks, live quotes, depth,
trades, order management, account balances, positions, execution queries and private order push.
Authentication uses the official SDK's OAuth 2.0 authorization-code flow with persisted,
automatically refreshed tokens. See
[`docs/integrations/longbridge.md`](../../../docs/integrations/longbridge.md) for configuration,
supported semantics and current limitations.

## Examples

Run the complete Rust data and paper-execution tester nodes with:

```bash
cargo run -p nautilus-longbridge --features examples --example longbridge-data-tester
cargo run -p nautilus-longbridge --features examples --example longbridge-exec-tester
cargo run -p nautilus-longbridge --features examples --example longbridge-grid-mm
```

Python tester nodes are available in
[`examples/live/longbridge`](../../../examples/live/longbridge). The tester examples register an
explicit sample `AAPL.US` equity. Production data clients can instead configure exact price
increments and load the remaining static metadata from Longbridge. Verify every definition before
connecting, and review the execution warning before changing the paper-trading default.

The Rust grid example runs one built-in `GridMarketMaker` for each of `AAPL.US`, `MSFT.US` and
`NVDA.US`. Each symbol uses three levels per side, ten shares per order, a 60-share position limit
and 25 bps spacing. Strategy IDs, order tags, reconciliation claims and position limits are isolated
per symbol. The example explicitly disables post-only grid orders and reduce-only exit orders
because Longbridge does not support those instructions. This weakens execution guarantees: up to 18
orders may rest across the three symbols, an order can execute immediately, and shutdown cannot
guarantee a flat account. The example has no aggregate account-level position or notional cap. Use
an isolated paper account and inspect it after the node stops.

The grid node uses Longbridge's trading-day and trading-session APIs to run until the current US
regular session closes. It handles US daylight-saving time and Longbridge half-trading days, and
refuses to start on a non-trading day or after the regular close.
