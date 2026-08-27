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
Stream Longbridge stock data with the built-in DataTester actor.

Set ``LONGBRIDGE_OAUTH_CLIENT_ID`` before running. On the first connection,
open the authorization URL from the logs and complete the OAuth callback.
No orders are placed.

Verify the sample instrument definition against current venue rules before use.

"""

from __future__ import annotations

from nautilus_trader.adapters.longbridge import LONGBRIDGE
from nautilus_trader.adapters.longbridge import LongbridgeDataClientConfig
from nautilus_trader.adapters.longbridge import LongbridgeDataClientFactory
from nautilus_trader.common import Environment
from nautilus_trader.live import LiveNode
from nautilus_trader.model import BarType
from nautilus_trader.model import ClientId
from nautilus_trader.model import Currency
from nautilus_trader.model import Equity
from nautilus_trader.model import InstrumentId
from nautilus_trader.model import Price
from nautilus_trader.model import Quantity
from nautilus_trader.model import Symbol
from nautilus_trader.model import TraderId
from nautilus_trader.testkit import DataTesterConfig


TRADER_ID = TraderId.from_str("TESTER-001")
INSTRUMENT_ID = InstrumentId.from_str(f"AAPL.US.{LONGBRIDGE}")
RAW_SYMBOL = Symbol.from_str("AAPL.US")
PRICE_INCREMENT = Price.from_str("0.01")
LOT_SIZE = Quantity.from_str("1")
BAR_TYPE = BarType.from_str(f"{INSTRUMENT_ID}-1-MINUTE-LAST-EXTERNAL")
ENABLE_OVERNIGHT = False


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
    node = (
        LiveNode.builder("LONGBRIDGE-DATA-TESTER-001", TRADER_ID, Environment.LIVE)
        .add_data_client(
            None,
            LongbridgeDataClientFactory(),
            LongbridgeDataClientConfig(enable_overnight=ENABLE_OVERNIGHT),
        )
        .build()
    )
    node.cache.add_instrument(sample_equity())
    node.add_builtin_actor(
        "DataTester",
        DataTesterConfig(
            client_id=ClientId.from_str(LONGBRIDGE),
            instrument_ids=[INSTRUMENT_ID],
            bar_types=[BAR_TYPE],
            subscribe_book_depth=True,
            book_depth=10,
            subscribe_quotes=True,
            subscribe_trades=True,
            subscribe_bars=True,
            manage_book=False,
            log_data=True,
        ),
    )

    node.run()


if __name__ == "__main__":
    main()
