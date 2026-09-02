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
cargo run -p nautilus-longbridge --features examples --example longbridge-range-fakeout-backtest
cargo run -p nautilus-longbridge --features examples --example longbridge-slc-trader
```

The range-fakeout backtest downloads up to 1,000 historical Longbridge 5-minute bars, loads the
instrument definition from Longbridge, and replays only those bars through `BacktestEngine`. Its
defaults implement a New York 00:00-04:00 range, close-confirmed breakout and reentry, one-tick stop
buffer, signal-close-based 2R target, and one trade per day. Override its
`LONGBRIDGE_BACKTEST_*` environment variables for the instrument, exact price increment, date range,
session times, size and risk parameters. The signal is confirmed at bar close and the market bracket
fills no earlier than the next bar, so gaps and intrabar ordering remain conservative OHLC
assumptions rather than tick-level simulation.

The SLC trader is a parameterized, bar-only live strategy for US equities. It defaults to `QQQ.US`
in Longbridge paper trading, risks at most USD 25 and five shares per entry, allows one trade per
day, submits a market-if-touched protective stop for every entry fill, and starts a cancel-then-close
exit when a completed 5-minute bar closes beyond the signal-close-based 2R target or reaches the
pre-close cutoff. It requires realtime candlestick pushes; confirmed-only pushes would add a bar of
latency. Live routing requires both
`LONGBRIDGE_SLC_PAPERTRADING=false` and
`LONGBRIDGE_SLC_LIVE_ACK=I_UNDERSTAND_LIVE_ORDERS`. A restart resets the in-memory daily counters and
flattens reconciled strategy exposure before accepting new entries. Paper-test the strategy and
inspect the broker account after every shutdown; profitability and a flat shutdown state cannot be
guaranteed.

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
