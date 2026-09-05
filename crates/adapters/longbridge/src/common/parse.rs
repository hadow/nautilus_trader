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

//! Exact fixed-point conversions between Longbridge SDK and Nautilus model types.

use std::{collections::hash_map::DefaultHasher, hash::Hasher, str::FromStr};

use ahash::AHashMap;
use anyhow::Context;
use longbridge::{
    quote::{Candlestick, Depth, Period, SecurityBoard, SecurityStaticInfo, Trade},
    trade::{
        AccountBalance as LongbridgeAccountBalance, Execution, Order,
        OrderSide as LongbridgeOrderSide, OrderStatus as LongbridgeOrderStatus,
        OrderType as LongbridgeOrderType, StockPosition, TimeInForceType as LongbridgeTimeInForce,
    },
};
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{
        Bar, BarType, DEPTH10_LEN, OrderBookDepth10, QuoteTick, TradeTick,
        order::{BookOrder, NULL_ORDER},
    },
    enums::{
        AggressorSide, BarAggregation, LiquiditySide, OrderSide, OrderStatus, OrderType,
        PositionSideSpecified, PriceType, RecordFlag, TimeInForce,
    },
    identifiers::{AccountId, ClientOrderId, InstrumentId, Symbol, TradeId, VenueOrderId},
    instruments::{Equity, InstrumentAny},
    reports::{FillReport, OrderStatusReport, PositionStatusReport},
    types::{AccountBalance, Currency, MarginBalance, Money, Price, Quantity},
};
use rust_decimal::Decimal;
use time::OffsetDateTime;

use crate::common::consts::LONGBRIDGE_VENUE;

/// Converts a Longbridge symbol such as `AAPL.US` to the adapter instrument ID.
#[must_use]
pub fn instrument_id(symbol: &str) -> InstrumentId {
    InstrumentId::new(Symbol::from(symbol), *LONGBRIDGE_VENUE)
}

/// Converts an SDK timestamp to UNIX nanoseconds.
///
/// # Errors
///
/// Returns an error for pre-epoch or out-of-range timestamps.
pub fn unix_nanos(timestamp: OffsetDateTime) -> anyhow::Result<UnixNanos> {
    let nanos = timestamp.unix_timestamp_nanos();
    let nanos = u64::try_from(nanos).context("Longbridge timestamp is outside UNIX u64 range")?;
    Ok(UnixNanos::from(nanos))
}

/// Maps a Nautilus bar type to a Longbridge candlestick period.
///
/// # Errors
///
/// Returns an error when Longbridge does not publish the requested external-LAST interval.
pub fn period_from_bar_type(bar_type: BarType) -> anyhow::Result<Period> {
    let spec = bar_type.spec();
    if spec.price_type != PriceType::Last {
        anyhow::bail!("Longbridge candlesticks only support LAST price bars");
    }

    let step = spec.step.get();
    let period = match (step, spec.aggregation) {
        (1, BarAggregation::Minute) => Period::OneMinute,
        (2, BarAggregation::Minute) => Period::TwoMinute,
        (3, BarAggregation::Minute) => Period::ThreeMinute,
        (5, BarAggregation::Minute) => Period::FiveMinute,
        (10, BarAggregation::Minute) => Period::TenMinute,
        (15, BarAggregation::Minute) => Period::FifteenMinute,
        (20, BarAggregation::Minute) => Period::TwentyMinute,
        (30, BarAggregation::Minute) => Period::ThirtyMinute,
        (45, BarAggregation::Minute) => Period::FortyFiveMinute,
        (1, BarAggregation::Hour) => Period::SixtyMinute,
        (2, BarAggregation::Hour) => Period::TwoHour,
        (3, BarAggregation::Hour) => Period::ThreeHour,
        (4, BarAggregation::Hour) => Period::FourHour,
        (1, BarAggregation::Day) => Period::Day,
        (1, BarAggregation::Week) => Period::Week,
        (1, BarAggregation::Month) => Period::Month,
        (3, BarAggregation::Month) => Period::Quarter,
        (1, BarAggregation::Year) => Period::Year,
        _ => anyhow::bail!("Unsupported Longbridge bar interval: {bar_type}"),
    };
    Ok(period)
}

/// Parses Longbridge static security metadata into a cash equity definition.
///
/// # Errors
///
/// Returns an error for a non-equity board or invalid currency, lot size, or price increment.
pub fn parse_instrument(
    info: &SecurityStaticInfo,
    price_increment: Price,
    ts_init: UnixNanos,
) -> anyhow::Result<InstrumentAny> {
    anyhow::ensure!(
        matches!(
            info.board,
            SecurityBoard::USMain
                | SecurityBoard::USPink
                | SecurityBoard::HKEquity
                | SecurityBoard::HKPreIPO
                | SecurityBoard::SHMainConnect
                | SecurityBoard::SHMainNonConnect
                | SecurityBoard::SHSTAR
                | SecurityBoard::SZMainConnect
                | SecurityBoard::SZMainNonConnect
                | SecurityBoard::SZGEMConnect
                | SecurityBoard::SZGEMNonConnect
                | SecurityBoard::SGMain
        ),
        "Longbridge security {} has unsupported board {}",
        info.symbol,
        info.board,
    );
    anyhow::ensure!(
        info.lot_size > 0,
        "Longbridge security {} has invalid lot size {}",
        info.symbol,
        info.lot_size,
    );

    let instrument_id = instrument_id(&info.symbol);
    let raw_symbol = Symbol::from(info.symbol.as_str());
    let currency = Currency::from_str(&info.currency)
        .with_context(|| format!("invalid currency for Longbridge security {instrument_id}"))?;
    let lot_size = Quantity::from_decimal(Decimal::from(info.lot_size))?;
    let equity = Equity::new_checked(
        instrument_id,
        raw_symbol,
        None,
        currency,
        price_increment.precision,
        price_increment,
        Some(lot_size),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        ts_init,
        ts_init,
    )?;
    Ok(InstrumentAny::from(equity))
}

/// Parses a Longbridge depth snapshot and its top-of-book quote.
///
/// # Errors
///
/// Returns an error if a price or quantity is not representable exactly.
pub fn parse_depth(
    symbol: &str,
    bids: &[Depth],
    asks: &[Depth],
    ts_event: UnixNanos,
    ts_init: UnixNanos,
) -> anyhow::Result<(OrderBookDepth10, Option<QuoteTick>)> {
    let instrument_id = instrument_id(symbol);
    let mut parsed_bids = [NULL_ORDER; DEPTH10_LEN];
    let mut parsed_asks = [NULL_ORDER; DEPTH10_LEN];
    let mut bid_counts = [0_u32; DEPTH10_LEN];
    let mut ask_counts = [0_u32; DEPTH10_LEN];

    for (index, (level, raw_price)) in bids
        .iter()
        .filter_map(|level| level.price.map(|price| (level, price)))
        .take(DEPTH10_LEN)
        .enumerate()
    {
        let price = Price::from_decimal(raw_price)?;
        let size = Quantity::from_decimal(Decimal::from(level.volume))?;
        parsed_bids[index] = BookOrder::new(OrderSide::Buy, price, size, 0);
        bid_counts[index] = u32::try_from(level.order_num.max(0)).unwrap_or(u32::MAX);
    }

    for (index, (level, raw_price)) in asks
        .iter()
        .filter_map(|level| level.price.map(|price| (level, price)))
        .take(DEPTH10_LEN)
        .enumerate()
    {
        let price = Price::from_decimal(raw_price)?;
        let size = Quantity::from_decimal(Decimal::from(level.volume))?;
        parsed_asks[index] = BookOrder::new(OrderSide::Sell, price, size, 0);
        ask_counts[index] = u32::try_from(level.order_num.max(0)).unwrap_or(u32::MAX);
    }

    let quote = match (parsed_bids.first(), parsed_asks.first()) {
        (Some(bid), Some(ask))
            if bid.side == OrderSide::Buy
                && ask.side == OrderSide::Sell
                && !bid.size.is_zero()
                && !ask.size.is_zero() =>
        {
            Some(QuoteTick::new_checked(
                instrument_id,
                bid.price,
                ask.price,
                bid.size,
                ask.size,
                ts_event,
                ts_init,
            )?)
        }
        _ => None,
    };

    let depth = OrderBookDepth10::new(
        instrument_id,
        parsed_bids,
        parsed_asks,
        bid_counts,
        ask_counts,
        RecordFlag::F_SNAPSHOT as u8,
        0,
        ts_event,
        ts_init,
    );
    Ok((depth, quote))
}

/// Parses a batch of Longbridge trades.
///
/// # Errors
///
/// Returns an error if a timestamp, price or quantity cannot be represented.
pub fn parse_trades(
    symbol: &str,
    trades: &[Trade],
    ts_init: UnixNanos,
) -> anyhow::Result<Vec<TradeTick>> {
    trades
        .iter()
        .enumerate()
        .map(|(index, trade)| {
            let ts_event = unix_nanos(trade.timestamp)?;
            let price = Price::from_decimal(trade.price)?;
            let quantity = Quantity::from_decimal(Decimal::from(trade.volume))?;
            // Longbridge reports the trade's price direction relative to the preceding trade,
            // not the aggressing order side. Do not infer buyer/seller initiation from it.
            let aggressor_side = AggressorSide::NoAggressor;
            let mut hasher = DefaultHasher::new();
            hasher.write(symbol.as_bytes());
            hasher.write(&trade.timestamp.unix_timestamp_nanos().to_le_bytes());
            hasher.write(trade.price.to_string().as_bytes());
            hasher.write_i64(trade.volume);
            hasher.write_u64(
                u64::try_from(index).context("Longbridge trade batch index exceeds u64")?,
            );
            hasher.write(trade.trade_type.as_bytes());
            hasher.write(format!("{:?}", trade.direction).as_bytes());
            hasher.write(format!("{:?}", trade.trade_session).as_bytes());
            let trade_id = TradeId::new(format!("LB-{:016X}", hasher.finish()));
            Ok(TradeTick::new(
                instrument_id(symbol),
                price,
                quantity,
                aggressor_side,
                trade_id,
                ts_event,
                ts_init,
            ))
        })
        .collect()
}

/// Parses a Longbridge candlestick using its source price precision.
///
/// # Errors
///
/// Returns an error with symbol, bar type, timestamp, and OHLC context if a value cannot be
/// represented or the candlestick violates Nautilus OHLC invariants.
pub fn parse_bar(
    bar_type: BarType,
    candlestick: Candlestick,
    ts_init: UnixNanos,
) -> anyhow::Result<Bar> {
    parse_bar_inner(bar_type, candlestick, ts_init, None)
}

/// Parses a Longbridge candlestick at the configured instrument price precision.
///
/// # Errors
///
/// Returns an error with symbol, bar type, timestamp, and OHLC context if a value cannot be
/// represented or the candlestick violates Nautilus OHLC invariants.
pub fn parse_bar_with_price_precision(
    bar_type: BarType,
    candlestick: Candlestick,
    ts_init: UnixNanos,
    price_precision: u8,
) -> anyhow::Result<Bar> {
    parse_bar_inner(bar_type, candlestick, ts_init, Some(price_precision))
}

fn parse_bar_inner(
    bar_type: BarType,
    candlestick: Candlestick,
    ts_init: UnixNanos,
    price_precision: Option<u8>,
) -> anyhow::Result<Bar> {
    let (timestamp, open, high, low, close) = (
        candlestick.timestamp,
        candlestick.open,
        candlestick.high,
        candlestick.low,
        candlestick.close,
    );
    let price = |value| match price_precision {
        Some(precision) => Price::from_decimal_dp(value, precision),
        None => Price::from_decimal(value),
    };
    (|| {
        Bar::new_checked(
            bar_type,
            price(open)?,
            price(high)?,
            price(low)?,
            price(close)?,
            Quantity::from_decimal(Decimal::from(candlestick.volume))?,
            unix_nanos(timestamp)?,
            ts_init,
        )
    })()
    .with_context(|| {
        format!(
            "invalid Longbridge bar: symbol={}, bar_type={bar_type}, timestamp={timestamp}, open={open}, high={high}, low={low}, close={close}",
            bar_type.instrument_id().symbol,
        )
    })
}

/// Maps a Nautilus order side to the SDK.
///
/// # Errors
///
/// Returns an error for an unspecified side.
pub fn to_longbridge_order_side(side: OrderSide) -> anyhow::Result<LongbridgeOrderSide> {
    match side {
        OrderSide::Buy => Ok(LongbridgeOrderSide::Buy),
        OrderSide::Sell => Ok(LongbridgeOrderSide::Sell),
        OrderSide::NoOrderSide => anyhow::bail!("Longbridge orders require BUY or SELL side"),
    }
}

/// Maps a Nautilus order type to the SDK's supported subset.
///
/// # Errors
///
/// Returns an error for order types not represented by the official SDK API.
pub fn to_longbridge_order_type(order_type: OrderType) -> anyhow::Result<LongbridgeOrderType> {
    match order_type {
        OrderType::Market => Ok(LongbridgeOrderType::MO),
        OrderType::Limit => Ok(LongbridgeOrderType::LO),
        OrderType::MarketIfTouched => Ok(LongbridgeOrderType::MIT),
        OrderType::LimitIfTouched => Ok(LongbridgeOrderType::LIT),
        _ => anyhow::bail!("Unsupported Longbridge order type: {order_type}"),
    }
}

/// Maps a Nautilus time-in-force to the SDK.
///
/// # Errors
///
/// Returns an error for immediate or auction instructions unsupported by this adapter slice.
pub fn to_longbridge_time_in_force(
    time_in_force: TimeInForce,
) -> anyhow::Result<LongbridgeTimeInForce> {
    match time_in_force {
        TimeInForce::Day => Ok(LongbridgeTimeInForce::Day),
        TimeInForce::Gtc => Ok(LongbridgeTimeInForce::GoodTilCanceled),
        TimeInForce::Gtd => Ok(LongbridgeTimeInForce::GoodTilDate),
        _ => anyhow::bail!("Unsupported Longbridge time in force: {time_in_force}"),
    }
}

fn parse_order_side(side: LongbridgeOrderSide) -> anyhow::Result<OrderSide> {
    match side {
        LongbridgeOrderSide::Buy => Ok(OrderSide::Buy),
        LongbridgeOrderSide::Sell => Ok(OrderSide::Sell),
        LongbridgeOrderSide::Unknown => anyhow::bail!("Longbridge returned unknown order side"),
    }
}

fn parse_order_type(order_type: LongbridgeOrderType) -> anyhow::Result<OrderType> {
    match order_type {
        LongbridgeOrderType::MO | LongbridgeOrderType::AO => Ok(OrderType::Market),
        LongbridgeOrderType::LO
        | LongbridgeOrderType::ELO
        | LongbridgeOrderType::ALO
        | LongbridgeOrderType::ODD
        | LongbridgeOrderType::SLO => Ok(OrderType::Limit),
        LongbridgeOrderType::MIT => Ok(OrderType::MarketIfTouched),
        LongbridgeOrderType::LIT => Ok(OrderType::LimitIfTouched),
        LongbridgeOrderType::TSMAMT | LongbridgeOrderType::TSMPCT => {
            Ok(OrderType::TrailingStopMarket)
        }
        LongbridgeOrderType::TSLPAMT | LongbridgeOrderType::TSLPPCT => {
            Ok(OrderType::TrailingStopLimit)
        }
        LongbridgeOrderType::Unknown => anyhow::bail!("Longbridge returned unknown order type"),
    }
}

fn parse_time_in_force(time_in_force: LongbridgeTimeInForce) -> anyhow::Result<TimeInForce> {
    match time_in_force {
        LongbridgeTimeInForce::Day => Ok(TimeInForce::Day),
        LongbridgeTimeInForce::GoodTilCanceled => Ok(TimeInForce::Gtc),
        LongbridgeTimeInForce::GoodTilDate => Ok(TimeInForce::Gtd),
        LongbridgeTimeInForce::Unknown => {
            anyhow::bail!("Longbridge returned unknown time in force")
        }
    }
}

fn parse_order_status(status: LongbridgeOrderStatus) -> anyhow::Result<OrderStatus> {
    match status {
        LongbridgeOrderStatus::NotReported
        | LongbridgeOrderStatus::ReplacedNotReported
        | LongbridgeOrderStatus::ProtectedNotReported
        | LongbridgeOrderStatus::VarietiesNotReported
        | LongbridgeOrderStatus::WaitToNew => Ok(OrderStatus::Submitted),
        LongbridgeOrderStatus::New | LongbridgeOrderStatus::Replaced => Ok(OrderStatus::Accepted),
        LongbridgeOrderStatus::WaitToReplace | LongbridgeOrderStatus::PendingReplace => {
            Ok(OrderStatus::PendingUpdate)
        }
        LongbridgeOrderStatus::PartialFilled => Ok(OrderStatus::PartiallyFilled),
        LongbridgeOrderStatus::WaitToCancel | LongbridgeOrderStatus::PendingCancel => {
            Ok(OrderStatus::PendingCancel)
        }
        LongbridgeOrderStatus::Rejected => Ok(OrderStatus::Rejected),
        LongbridgeOrderStatus::Canceled | LongbridgeOrderStatus::PartialWithdrawal => {
            Ok(OrderStatus::Canceled)
        }
        LongbridgeOrderStatus::Expired => Ok(OrderStatus::Expired),
        LongbridgeOrderStatus::Filled => Ok(OrderStatus::Filled),
        LongbridgeOrderStatus::Unknown => anyhow::bail!("Longbridge returned unknown order status"),
    }
}

/// Converts an SDK order to a Nautilus reconciliation report.
///
/// # Errors
///
/// Returns an error if required fields are unknown or not exactly representable.
pub fn parse_order_status_report(
    order: &Order,
    account_id: AccountId,
    client_order_id: Option<ClientOrderId>,
    ts_init: UnixNanos,
) -> anyhow::Result<OrderStatusReport> {
    let ts_accepted = unix_nanos(order.submitted_at)?;
    let ts_last = order
        .updated_at
        .map(unix_nanos)
        .transpose()?
        .unwrap_or(ts_accepted);
    let mut report = OrderStatusReport::new(
        account_id,
        instrument_id(&order.symbol),
        client_order_id,
        VenueOrderId::from(order.order_id.as_str()),
        parse_order_side(order.side)?,
        parse_order_type(order.order_type)?,
        parse_time_in_force(order.time_in_force)?,
        parse_order_status(order.status)?,
        Quantity::from_decimal(order.quantity)?,
        Quantity::from_decimal(order.executed_quantity)?,
        ts_accepted,
        ts_last,
        ts_init,
        None,
    );

    if let Some(price) = order.price {
        report = report.with_price(Price::from_decimal(price)?);
    }

    if let Some(avg_px) = order.executed_price {
        report = report.with_avg_px(avg_px);
    }

    if let Some(trigger_price) = order.trigger_price {
        report = report.with_trigger_price(Price::from_decimal(trigger_price)?);
    }

    if !order.msg.is_empty() && report.order_status == OrderStatus::Canceled {
        report.cancel_reason = Some(order.msg.clone());
    }
    Ok(report)
}

/// Converts an SDK execution to a Nautilus fill report.
///
/// Longbridge's execution query does not include commission or liquidity classification; these
/// are reported explicitly as zero and `NO_LIQUIDITY_SIDE` instead of being inferred.
///
/// # Errors
///
/// Returns an error if a currency or fixed-point value cannot be represented.
pub fn parse_fill_report(
    execution: &Execution,
    account_id: AccountId,
    order_side: OrderSide,
    currency_code: &str,
    client_order_id: Option<ClientOrderId>,
    ts_init: UnixNanos,
) -> anyhow::Result<FillReport> {
    let currency = Currency::from_str(currency_code)
        .with_context(|| format!("Invalid Longbridge currency: {currency_code}"))?;
    Ok(FillReport::new(
        account_id,
        instrument_id(&execution.symbol),
        VenueOrderId::from(execution.order_id.as_str()),
        TradeId::from(execution.trade_id.as_str()),
        order_side,
        Quantity::from_decimal(execution.quantity)?,
        Price::from_decimal(execution.price)?,
        Money::from_decimal(Decimal::ZERO, currency)?,
        LiquiditySide::NoLiquiditySide,
        client_order_id,
        None,
        unix_nanos(execution.trade_done_at)?,
        ts_init,
        None,
    ))
}

/// Converts an SDK stock position to a Nautilus position report.
///
/// # Errors
///
/// Returns an error if the quantity cannot be represented.
pub fn parse_position_status_report(
    position: &StockPosition,
    account_id: AccountId,
    ts_init: UnixNanos,
) -> anyhow::Result<PositionStatusReport> {
    let (side, quantity) = if position.quantity.is_sign_negative() {
        (PositionSideSpecified::Short, position.quantity.abs())
    } else if position.quantity.is_zero() {
        (PositionSideSpecified::Flat, Decimal::ZERO)
    } else {
        (PositionSideSpecified::Long, position.quantity)
    };
    Ok(PositionStatusReport::new(
        account_id,
        instrument_id(&position.symbol),
        side,
        Quantity::from_decimal(quantity)?,
        ts_init,
        ts_init,
        None,
        None,
        Some(position.cost_price),
    ))
}

/// Converts Longbridge account records to Nautilus cash and margin balances.
///
/// # Errors
///
/// Returns an error if a currency or amount cannot be represented.
pub fn parse_account_state(
    accounts: &[LongbridgeAccountBalance],
) -> anyhow::Result<(Vec<AccountBalance>, Vec<MarginBalance>)> {
    let mut cash_by_currency: AHashMap<Currency, (Decimal, Decimal)> = AHashMap::new();
    let mut margin_by_currency: AHashMap<Currency, (Decimal, Decimal)> = AHashMap::new();

    for account in accounts {
        for cash in &account.cash_infos {
            let currency = Currency::from_str(&cash.currency)
                .with_context(|| format!("Invalid Longbridge currency: {}", cash.currency))?;
            let total = cash.available_cash + cash.frozen_cash + cash.settling_cash;
            let aggregate = cash_by_currency
                .entry(currency)
                .or_insert((Decimal::ZERO, Decimal::ZERO));
            aggregate.0 += total;
            aggregate.1 += cash.available_cash;
        }

        if !account.init_margin.is_zero() || !account.maintenance_margin.is_zero() {
            let currency = Currency::from_str(&account.currency)
                .with_context(|| format!("Invalid Longbridge currency: {}", account.currency))?;
            let aggregate = margin_by_currency
                .entry(currency)
                .or_insert((Decimal::ZERO, Decimal::ZERO));
            aggregate.0 += account.init_margin;
            aggregate.1 += account.maintenance_margin;
        }
    }

    let mut balances = cash_by_currency
        .into_iter()
        .map(|(currency, (total, free))| AccountBalance::from_total_and_free(total, free, currency))
        .collect::<Result<Vec<_>, _>>()?;
    balances.sort_by_key(|balance| balance.currency.code);

    let mut margins = margin_by_currency
        .into_iter()
        .map(|(currency, (initial, maintenance))| {
            Ok(MarginBalance::new(
                Money::from_decimal(initial, currency)?,
                Money::from_decimal(maintenance, currency)?,
                None,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    margins.sort_by_key(|margin| margin.currency.code);

    Ok((balances, margins))
}

#[cfg(test)]
mod tests {
    use longbridge::quote::{DerivativeType, SecurityBoard, SecurityStaticInfo, TradeDirection};
    use nautilus_model::{
        data::{BarSpecification, BarType},
        enums::{AggregationSource, BarAggregation, PriceType},
    };
    use rstest::rstest;
    use time::macros::datetime;

    use super::*;

    #[rstest]
    fn test_instrument_id_uses_adapter_venue() {
        assert_eq!(instrument_id("AAPL.US").to_string(), "AAPL.US.LONGBRIDGE");
    }

    #[rstest]
    fn test_period_from_bar_type() {
        let bar_type = BarType::new(
            instrument_id("700.HK"),
            BarSpecification::new(5, BarAggregation::Minute, PriceType::Last),
            AggregationSource::External,
        );
        assert_eq!(period_from_bar_type(bar_type).unwrap(), Period::FiveMinute);
    }

    #[rstest]
    fn test_parse_trade_preserves_decimal_values() {
        let trade = Trade {
            price: Decimal::from_str("123.456").unwrap(),
            volume: 200,
            timestamp: datetime!(2026-08-26 01:02:03 UTC),
            trade_type: String::new(),
            direction: TradeDirection::Up,
            trade_session: Default::default(),
        };
        let ticks = parse_trades("AAPL.US", &[trade], UnixNanos::from(2)).unwrap();
        assert_eq!(
            ticks[0].price.as_decimal(),
            Decimal::from_str("123.456").unwrap()
        );
        assert_eq!(ticks[0].size.as_decimal(), Decimal::from(200));
        assert_eq!(ticks[0].aggressor_side, AggressorSide::NoAggressor);
        assert!(ticks[0].trade_id.as_str().len() <= 36);

        let repeated = parse_trades(
            "AAPL.US",
            &[Trade {
                price: Decimal::from_str("123.456").unwrap(),
                volume: 200,
                timestamp: datetime!(2026-08-26 01:02:03 UTC),
                trade_type: String::new(),
                direction: TradeDirection::Up,
                trade_session: Default::default(),
            }],
            UnixNanos::from(3),
        )
        .unwrap();
        assert_eq!(ticks[0].trade_id, repeated[0].trade_id);
    }

    #[rstest]
    fn test_parse_instrument_uses_static_info_and_configured_tick() {
        let instrument = parse_instrument(
            &SecurityStaticInfo {
                symbol: "700.HK".to_string(),
                name_cn: "腾讯控股".to_string(),
                name_en: "Tencent".to_string(),
                name_hk: "騰訊控股".to_string(),
                exchange: "SEHK".to_string(),
                currency: "HKD".to_string(),
                lot_size: 100,
                total_shares: 1,
                circulating_shares: 1,
                hk_shares: 1,
                eps: Decimal::ZERO,
                eps_ttm: Decimal::ZERO,
                bps: Decimal::ZERO,
                dividend_yield: Decimal::ZERO,
                stock_derivatives: DerivativeType::empty(),
                board: SecurityBoard::HKEquity,
            },
            Price::from("0.001"),
            UnixNanos::from(42),
        )
        .unwrap();

        let InstrumentAny::Equity(equity) = instrument else {
            panic!("expected equity");
        };
        assert_eq!(equity.id.to_string(), "700.HK.LONGBRIDGE");
        assert_eq!(equity.currency, Currency::HKD());
        assert_eq!(equity.price_increment, Price::from("0.001"));
        assert_eq!(equity.lot_size, Some(Quantity::from(100)));
        assert_eq!(equity.min_quantity, None);
        assert_eq!(equity.ts_event, UnixNanos::from(42));
    }
}
