#!/usr/bin/env python3
# -------------------------------------------------------------------------------------------------
#  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
#  https://nautechsystems.io
#
#  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
#  You may not use this file except in compliance with the License.
#  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
#
#  Unless required by applicable law or agreed to in writing, software
#  distributed under the License is distributed on an "AS IS" BASIS,
#  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#  See the License for the specific language governing permissions and
#  limitations under the License.
# -------------------------------------------------------------------------------------------------
"""
Test Longbridge stock execution with the built-in ExecTester strategy.

WARNING: ``DRY_RUN = False`` submits orders. The defaults route them to
Longbridge paper trading, but setting ``PAPERTRADING = False`` routes orders to
the configured live account. On start the tester buys one share, maintains one
passive buy order, then cancels orders and closes positions on stop. The strategy
has no alpha advantage and is not intended for production trading.

Set ``LONGBRIDGE_OAUTH_CLIENT_ID`` before running. On the first connection,
open the authorization URL from the logs and complete the OAuth callback.
Verify the sample instrument definition against current venue rules before use.

"""

from __future__ import annotations

from decimal import Decimal

from nautilus_trader.adapters.longbridge import LONGBRIDGE
from nautilus_trader.adapters.longbridge import LongbridgeDataClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeDataClientFactory
from nautilus_trader.adapters.longbridge import LongbridgeExecClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeExecutionClientFactory
from nautilus_trader.common import Environment
from nautilus_trader.config import LiveRiskEngineConfig
from nautilus_trader.live import LiveNode
from nautilus_trader.model import AccountId
from nautilus_trader.model import AccountType
from nautilus_trader.model import ClientId
from nautilus_trader.model import Currency
from nautilus_trader.model import Equity
from nautilus_trader.model import InstrumentId
from nautilus_trader.model import Price
from nautilus_trader.model import Quantity
from nautilus_trader.model import StrategyId
from nautilus_trader.model import Symbol
from nautilus_trader.model import TimeInForce
from nautilus_trader.model import TraderId
from nautilus_trader.testkit import ExecTesterConfig


TRADER_ID = TraderId.from_str("TESTER-001")
ACCOUNT_ID = AccountId.from_str("LONGBRIDGE-001")
STRATEGY_ID = StrategyId.from_str("EXEC_TESTER-001")
INSTRUMENT_ID = InstrumentId.from_str(f"AAPL.US.{LONGBRIDGE}")
RAW_SYMBOL = Symbol.from_str("AAPL.US")
PRICE_INCREMENT = Price.from_str("0.01")
LOT_SIZE = Quantity.from_str("1")
ORDER_QTY = Quantity.from_str("1")
TOB_OFFSET_TICKS = 50
PAPERTRADING = True
DRY_RUN = False


def sample_equity() -> Equity:
    return Equity(
        instrument_id=INSTRUMENT_ID,
        raw_symbol=RAW_SYMBOL,
        currency=Currency.from_str("USD"),
        price_precision=PRICE_INCREMENT.precision,
        price_increment=PRICE_INCREMENT,
        lot_size=LOT_SIZE,
        min_quantity=LOT_SIZE,
        ts_event=0,
        ts_init=0,
    )


def main() -> None:
    environment = Environment.SANDBOX if PAPERTRADING else Environment.LIVE
    node = (
        LiveNode.builder("LONGBRIDGE-EXEC-TESTER-001", TRADER_ID, environment)
        .with_reconciliation(True)
        .with_risk_engine_config(LiveRiskEngineConfig(bypass=True))
        .add_data_client(
            None,
            LongbridgeDataClientFactory(),
            LongbridgeDataClientConfig(),
        )
        .add_exec_client(
            None,
            LongbridgeExecutionClientFactory(TRADER_ID, ACCOUNT_ID),
            LongbridgeExecClientConfig(
                account_type=AccountType.MARGIN,
                papertrading=PAPERTRADING,
                outside_rth=False,
            ),
        )
        .build()
    )
    node.cache.add_instrument(sample_equity())
    node.add_builtin_strategy(
        "ExecTester",
        ExecTesterConfig(
            strategy_id=STRATEGY_ID,
            instrument_id=INSTRUMENT_ID,
            client_id=ClientId.from_str(LONGBRIDGE),
            external_order_claims=[INSTRUMENT_ID],
            order_qty=ORDER_QTY,
            subscribe_quotes=True,
            subscribe_trades=True,
            open_position_on_start_qty=Decimal(1),
            open_position_on_first_quote=True,
            open_position_time_in_force=TimeInForce.DAY,
            enable_limit_buys=True,
            enable_limit_sells=False,
            tob_offset_ticks=TOB_OFFSET_TICKS,
            limit_time_in_force=TimeInForce.DAY,
            use_post_only=False,
            cancel_orders_on_stop=True,
            close_positions_on_stop=True,
            close_positions_time_in_force=TimeInForce.DAY,
            reduce_only_on_stop=False,
            dry_run=DRY_RUN,
            log_data=False,
        ),
    )

    node.run()


if __name__ == "__main__":
    main()
