// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Example demonstrating live stock data testing with the Longbridge adapter.
//!
//! Edit the instrument constants below and verify them against the venue before running.
//!
//! Run with:
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-data-tester`
//!
//! Required environment variable:
//! - `LONGBRIDGE_OAUTH_CLIENT_ID`: OAuth 2.0 public client ID.

use nautilus_common::enums::Environment;
use nautilus_core::UnixNanos;
use nautilus_live::node::LiveNode;
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, common::consts::LONGBRIDGE_CLIENT_ID,
};
use nautilus_model::{
    data::bar::BarType,
    identifiers::{InstrumentId, Symbol, TraderId},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Price, Quantity},
};
use nautilus_testkit::testers::{DataTester, DataTesterConfig};

const TRADER_ID: &str = "TESTER-001";
const NODE_NAME: &str = "LONGBRIDGE-DATA-TESTER-001";
const INSTRUMENT_ID: &str = "AAPL.US.LONGBRIDGE";
const RAW_SYMBOL: &str = "AAPL.US";
const CURRENCY: &str = "USD";
const PRICE_INCREMENT: &str = "0.01";
const LOT_SIZE: &str = "1";
const BAR_SPEC: &str = "1-MINUTE-LAST-EXTERNAL";
const ENABLE_OVERNIGHT: bool = false;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let trader_id = TraderId::from(TRADER_ID);
    let instrument = sample_equity()?;
    let instrument_id = instrument.id;
    let bar_type = BarType::from(format!("{instrument_id}-{BAR_SPEC}").as_str());

    let data_config = LongbridgeDataClientConfig {
        enable_overnight: ENABLE_OVERNIGHT,
        ..Default::default()
    };

    let mut node = LiveNode::builder(trader_id, Environment::Live)?
        .with_name(NODE_NAME.to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_delay_post_stop_secs(2)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(data_config),
        )?
        .build()?;

    let cache = node.kernel().cache();
    cache
        .borrow_mut()
        .add_instrument(InstrumentAny::Equity(instrument))?;

    let tester_config = DataTesterConfig::builder()
        .client_id(*LONGBRIDGE_CLIENT_ID)
        .instrument_ids(vec![instrument_id])
        .bar_types(vec![bar_type])
        .subscribe_book_depth(true)
        .book_depth(10)
        .subscribe_quotes(true)
        .subscribe_trades(true)
        .subscribe_bars(true)
        .manage_book(false)
        .build()?;

    node.add_actor(DataTester::new(tester_config))?;
    node.run().await?;

    Ok(())
}

fn sample_equity() -> anyhow::Result<Equity> {
    let price_increment = Price::from(PRICE_INCREMENT);

    Ok(Equity::new_checked(
        InstrumentId::from(INSTRUMENT_ID),
        Symbol::from(RAW_SYMBOL),
        None,
        Currency::from(CURRENCY),
        price_increment.precision,
        price_increment,
        Some(Quantity::from(LOT_SIZE)),
        None,
        Some(Quantity::from(LOT_SIZE)),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        UnixNanos::default(),
        UnixNanos::default(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    fn test_sample_equity_matches_longbridge_symbol_contract() {
        let instrument = sample_equity().unwrap();

        assert_eq!(instrument.id, InstrumentId::from(INSTRUMENT_ID));
        assert_eq!(instrument.raw_symbol, Symbol::from(RAW_SYMBOL));
        assert_eq!(instrument.price_increment, Price::from(PRICE_INCREMENT));
        assert_eq!(instrument.lot_size, Some(Quantity::from(LOT_SIZE)));
    }
}
