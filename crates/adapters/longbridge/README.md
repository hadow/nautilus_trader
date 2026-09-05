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
cargo run -p nautilus-longbridge --features examples --example longbridge-slc-backtest
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

The SLC trader is a parameterized, bar-signal live strategy for US equities. Configure its symbols
and exact price increments in [`examples/slc_symbols.toml`](examples/slc_symbols.toml), or point
`LONGBRIDGE_SLC_CONFIG_PATH` at another TOML file. It defaults to Longbridge paper trading and one
strategy per symbol. Fresh and once-broken/reclaimed levels are retained in bounded per-side
collections. Stochastic confirmation defaults to three bars and must close within 0.35 ATR of the
level. The re-entry candle must also close at least 55% toward the trade-facing end of its range,
which filters indicator crosses without matching price rejection. Configure these gates with
`LONGBRIDGE_SLC_CONFIRMATION_WINDOW_BARS`,
`LONGBRIDGE_SLC_CONFIRMATION_MAX_DISTANCE_ATR`, and
`LONGBRIDGE_SLC_CONFIRMATION_MIN_CLOSE_LOCATION`. Entry orders are one-bar marketable limits sized
from their worst allowed price. Every fill receives a
market-if-touched protective stop, and its 2R target is recalculated from average fill price.
Executable top-of-book quotes trigger the cancel-then-close target exit immediately; completed
5-minute bars remain a conservative fallback. New entries stop at least 60 minutes before the
pre-close flatten time, and exposure which has not reached 0.5R MFE after nine completed bars is
closed; these thresholds are configurable. It requires realtime candlestick pushes;
confirmed-only pushes would add a bar of signal latency.

The SLC backtest uses the same strategy, symbol TOML and `LONGBRIDGE_SLC_*` parameters as the live
example. Set `LONGBRIDGE_SLC_BACKTEST_START` and `LONGBRIDGE_SLC_BACKTEST_END` to UTC timestamps;
the defaults replay August 2026. `LONGBRIDGE_SLC_BACKTEST_STARTING_BALANCE` defaults to
`100_000 USD`, `LONGBRIDGE_SLC_BACKTEST_TIMEOUT_SECS` defaults to 300, and
`LONGBRIDGE_SLC_BACKTEST_LOG_BARS=true` enables per-bar diagnostics. Conservative diagnostics
subtract `LONGBRIDGE_SLC_BACKTEST_ROUND_TRIP_COST_PER_SHARE`, which defaults to USD 0.01 per share,
and assume every entry fills at its configured worst permitted limit instead of the ideal signal
close; Nautilus engine statistics remain raw. They also report daily peak-to-trough maximum
drawdown, annualized return, Calmar, and positive, negative, and flat trading-day counts. It warms up
each symbol before the requested window, skips US half trading days, sends only completed 5-minute
bars to the matching engine, and advances 4-hour bars inside the strategy only when the next 4-hour
period begins. Orders submitted from a completed signal bar can fill against its last close in the
bar matcher, so the conservative entry-limit stress is the primary guard against this optimistic
execution. Resting 2R targets and stops use adaptive OHLC high/low ordering, and diagnostics flag
bars which touched both prices, so intrabar ordering, spreads, quote-trigger latency and broker
commissions are not reconstructed; treat results as a bar-level estimate rather than evidence of
stable profitability.

Set `LONGBRIDGE_SLC_BACKTEST_RISK_REWARDS=1.5,1.75,2` to compare fixed targets after one historical
download. Adding `LONGBRIDGE_SLC_BACKTEST_WALK_FORWARD_SPLIT=<UTC timestamp>` uses bars before the
split for parameter selection and runs only the winning target after the split. Selection uses
daily cost- and entry-slippage-stressed Sharpe and reprices a target win as a full-risk loss when
its OHLC bar also touched the stop; the final output reports OOS Sharpe degradation, maximum
drawdown, and Calmar, and rejects non-positive or undefined Sharpe.

All per-symbol strategies share a persisted SLC-owned risk ledger. Defaults cap open risk at USD 50,
account notional at USD 5,000, simultaneous entries or positions at two, and realized daily loss at
USD 50. Each order receives at most an equal share of account notional across usable position slots,
so the first expensive symbol cannot reserve the entire account limit. Manual trades and positions
owned by other strategies are outside this ledger, so use an isolated account. Orders using less
than 10% of the configured risk budget are rejected by default instead of adding economically tiny,
non-comparable trades; adjust this floor with `LONGBRIDGE_SLC_MIN_RISK_UTILIZATION`. Override the
account limits with
`LONGBRIDGE_SLC_MAX_OPEN_RISK`,
`LONGBRIDGE_SLC_MAX_ACCOUNT_NOTIONAL`, `LONGBRIDGE_SLC_MAX_OPEN_POSITIONS`, and
`LONGBRIDGE_SLC_DAILY_LOSS_LIMIT`. `LONGBRIDGE_SLC_RISK_STATE_PATH` selects the state file; paper and
live routing use separate files under `target/` by default. Corrupt state prevents startup, and a
restart retains daily trade counts, realized P&L, and conservative open-risk reservations.

At INFO level it reports per-symbol warmup counts, finalized 5-minute OHLCV and indicators, 4-hour
structure trends, active zone counts, data readiness, account risk, orders, exits and realized P&L.
Live routing requires both
`LONGBRIDGE_SLC_PAPERTRADING=false` and
`LONGBRIDGE_SLC_LIVE_ACK=I_UNDERSTAND_LIVE_ORDERS`. Reconciled strategy exposure is flattened before
new entries are accepted, and managed stop keeps reconciling orders and positions during shutdown.
Paper-test the strategy and inspect the broker account after every shutdown; profitability and a
flat shutdown state cannot be guaranteed.

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
