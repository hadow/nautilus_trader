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

The SLC trader is a parameterized, bar-signal live strategy for US equities. Its Longbridge OAuth
and routing settings, strategy, risk, session, warmup, backtest and symbols are documented in
[`examples/slc_symbols.toml`](examples/slc_symbols.toml). The default command reads that file; pass a
different TOML as the sole program argument when needed:

```bash
cargo run -p nautilus-longbridge --features examples --example longbridge-slc-trader -- /path/to/slc.toml
cargo run -p nautilus-longbridge --features examples --example longbridge-slc-backtest -- /path/to/slc.toml
```

The OAuth public client ID is read from the TOML, while OAuth tokens remain in the official SDK's
local secure storage. The example defaults to Longbridge paper trading and creates one strategy per
symbol. Fresh and once-broken/reclaimed levels are retained in bounded per-side collections.
Stochastic confirmation defaults to three bars, must remain close to the level, and requires a
directional candle close. Entry orders are one-bar marketable limits sized from their worst allowed
price. Every fill receives a market-if-touched protective stop, and its 2R target is recalculated
from average fill price. Executable top-of-book quotes trigger the cancel-then-close target exit
immediately; completed 5-minute bars remain a conservative fallback.

The SLC backtest uses the same TOML and strategy implementation as the live example. Its `[backtest]`
table controls the UTC interval, initial balance, download timeout, per-bar logging, transaction-cost
stress, target candidates and optional walk-forward split. Conservative diagnostics assume every
entry fills at its worst permitted limit, subtract configured round-trip costs and reprice ambiguous
same-bar target/stop outcomes as losses; Nautilus engine statistics remain raw. The report includes
daily Sharpe, maximum drawdown, annualized return, Calmar, and positive, negative, and flat day counts.

Set `backtest.risk_rewards = ["1.5", "1.75", "2"]` to compare fixed targets after one historical
download. Set `backtest.walk_forward_split` to use bars before the split for parameter selection and
run only the winning target after the split. Selection uses daily cost- and entry-slippage-stressed
Sharpe and reports OOS degradation, maximum drawdown and Calmar.

All per-symbol strategies share the persisted risk ledger configured under `[longbridge]`. Account
limits live under `[risk]`; they cap daily loss, open risk, account notional, position count, order
size and minimum usable risk. Paper and live routing use separate state paths. Manual trades and
positions owned by other strategies remain outside this ledger, so use an isolated account.

At INFO level it reports per-symbol warmup counts, finalized 5-minute OHLCV and indicators, 4-hour
structure trends, active zone counts, data readiness, account risk, orders, exits and realized P&L.
Live routing requires both `longbridge.papertrading = false` and
`longbridge.live_order_ack = "I_UNDERSTAND_LIVE_ORDERS"` in the TOML. Reconciled strategy exposure
is flattened before new entries are accepted, and managed stop keeps reconciling orders and
positions during shutdown. Paper-test the strategy and inspect the broker account after every
shutdown; profitability and a flat shutdown state cannot be guaranteed.

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
