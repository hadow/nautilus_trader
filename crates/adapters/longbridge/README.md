# Longbridge adapter

Rust-native NautilusTrader adapter built on the official Longbridge OpenAPI Rust SDK.

The adapter exposes live quotes, depth, trades, candlesticks, order management, account balances,
positions, execution queries and private order push. Authentication uses the official SDK's OAuth
2.0 authorization-code flow with persisted, automatically refreshed tokens. See
[`docs/integrations/longbridge.md`](../../../docs/integrations/longbridge.md) for configuration,
supported semantics and current limitations.

## Examples

Run the complete Rust data and paper-execution tester nodes with:

```bash
cargo run -p nautilus-longbridge --features examples --example longbridge-data-tester
cargo run -p nautilus-longbridge --features examples --example longbridge-exec-tester
```

Python tester nodes are available in
[`examples/live/longbridge`](../../../examples/live/longbridge). The examples register an explicit
sample `AAPL.US` equity because the adapter does not infer tick size or lot-size rules. Verify the
instrument definition before connecting, and review the execution warning before changing the
paper-trading default.
