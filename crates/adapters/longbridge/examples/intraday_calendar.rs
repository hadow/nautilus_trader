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
//  See the License for the specific language governing permissions and limitations under the License.
// -------------------------------------------------------------------------------------------------

use jiff::Timestamp;
use longbridge::{Market, quote::QuoteContext};
use nautilus_core::datetime::get_timezone;
use nautilus_longbridge::common::rate_limit::quote_api_call_with_retry;
use nautilus_trading::examples::strategies::IntradayMomentumSession;
use rust_decimal::Decimal;
use time::{Date, Month};
const US_TIMEZONE: &str = "America/New_York";

fn sdk_date(timestamp: Timestamp) -> anyhow::Result<Date> {
    let date = timestamp.to_zoned(get_timezone(US_TIMEZONE)?).date();
    Ok(Date::from_calendar_date(
        i32::from(date.year()),
        Month::try_from(u8::try_from(date.month())?)?,
        u8::try_from(date.day())?,
    )?)
}

pub(crate) async fn calendar(
    context: &QuoteContext,
    now: Timestamp,
    history_days: i64,
) -> anyhow::Result<Vec<IntradayMomentumSession>> {
    let today = sdk_date(now)?;
    let start = today - time::Duration::days(history_days);
    let end = today + time::Duration::days(7);
    let timezone = get_timezone(US_TIMEZONE)?;
    let mut trading_days = std::collections::BTreeSet::new();
    let mut half_days = std::collections::BTreeSet::new();
    let mut cursor = start;
    while cursor <= end {
        let boundary = (cursor + time::Duration::days(27)).min(end);
        let response =
            quote_api_call_with_retry(|| context.trading_days(Market::US, cursor, boundary))
                .await?;
        trading_days.extend(response.trading_days);
        trading_days.extend(response.half_trading_days.iter().copied());
        half_days.extend(response.half_trading_days);
        cursor = boundary + time::Duration::days(1);
    }

    let mut sessions = Vec::with_capacity(trading_days.len());
    for day in trading_days {
        let date = jiff::civil::Date::new(
            i16::try_from(day.year())?,
            i8::try_from(u8::from(day.month()))?,
            i8::try_from(day.day())?,
        )?;
        let open = date
            .to_datetime(jiff::civil::Time::new(9, 30, 0, 0)?)
            .to_zoned(timezone.clone())?
            .timestamp();
        let close_hour = if half_days.contains(&day) { 13 } else { 16 };
        let close = date
            .to_datetime(jiff::civil::Time::new(close_hour, 0, 0, 0)?)
            .to_zoned(timezone.clone())?
            .timestamp();
        sessions.push(IntradayMomentumSession {
            open: open.into(),
            close: close.into(),
            dividend: Decimal::ZERO,
        });
    }
    sessions.sort_unstable_by_key(|session| session.open);
    Ok(sessions)
}
