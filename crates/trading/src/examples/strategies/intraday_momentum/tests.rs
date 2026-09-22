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

use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarType, bar::BAR_SPEC_1_MINUTE_LAST},
    enums::AggregationSource,
    types::{Price, Quantity},
};
use rstest::rstest;
use rust_decimal_macros::dec;

use super::{
    IntradayMomentumConfig, IntradayMomentumModel, IntradayMomentumSession, PositionTarget,
    model::sized_quantity,
};

const MINUTE: u64 = 60_000_000_000;
const SESSION_GAP: u64 = 120 * MINUTE;

#[rstest]
fn calendar_lookup_preserves_open_exclusion_close_inclusion_and_gaps() {
    let calendar = sessions(2);
    let value = config(calendar.clone());
    for (time, expected) in [
        (0, None),
        (1, Some(0)),
        (90, Some(0)),
        (91, None),
        (120, None),
        (121, Some(1)),
        (210, Some(1)),
        (211, None),
        (10000, None),
    ] {
        assert_eq!(
            value.session((time * MINUTE).into()),
            expected.map(|i| calendar[i])
        );
    }
}

#[rstest]
#[case(dec!(0))]
#[case(dec!(-0.01))]
#[case(dec!(0.02))]
fn invalid_trade_risk_is_rejected(#[case] risk: rust_decimal::Decimal) {
    let mut value = config(sessions(3));
    value.max_daily_loss = dec!(0.01);
    value.risk_per_trade = Some(risk);
    assert!(value.validate().is_err());
}

fn sessions(count: usize) -> Vec<IntradayMomentumSession> {
    (0..count)
        .map(|index| {
            let open = UnixNanos::from(index as u64 * SESSION_GAP);
            IntradayMomentumSession {
                open,
                close: UnixNanos::from(open.as_u64() + 90 * MINUTE),
                dividend: dec!(0),
            }
        })
        .collect()
}

fn config(sessions: Vec<IntradayMomentumSession>) -> IntradayMomentumConfig {
    IntradayMomentumConfig {
        instrument_id: "SPY.SIM".into(),
        sessions,
        allow_ohlc_vwap_approximation: true,
        ..IntradayMomentumConfig::default()
    }
}

fn bar(session: IntradayMomentumSession, minute: u64, open: &str, close: &str) -> Bar {
    bar_with_volume(session, minute, open, close, "1000")
}

fn bar_with_volume(
    session: IntradayMomentumSession,
    minute: u64,
    open: &str,
    close: &str,
    volume: &str,
) -> Bar {
    let open = Price::from(open);
    let close = Price::from(close);
    let ts = UnixNanos::from(session.open.as_u64() + minute * MINUTE);
    Bar::new(
        BarType::new(
            "SPY.SIM".into(),
            BAR_SPEC_1_MINUTE_LAST,
            AggregationSource::External,
        ),
        open,
        open.max(close),
        open.min(close),
        close,
        Quantity::from(volume),
        ts,
        ts,
    )
}

fn history(sessions: &[IntradayMomentumSession]) -> Vec<Bar> {
    let mut result = Vec::new();
    for (index, session) in sessions.iter().copied().enumerate() {
        let session_close = match index % 3 {
            0 => "99.00",
            1 => "101.00",
            _ => "100.00",
        };
        result.extend((1..=90).map(|minute| {
            let close = if minute == 90 {
                session_close
            } else if minute % 30 == 0 {
                "101.00"
            } else {
                "100.00"
            };
            bar(session, minute, "100.00", close)
        }));
    }
    result
}

#[rstest]
fn model_emits_long_then_current_band_vwap_exit() {
    let calendar = sessions(16);
    let mut model = IntradayMomentumModel::new(config(calendar.clone())).unwrap();
    let current = calendar[15];
    let mut warmup = history(&calendar[..15]);
    warmup.push(bar(current, 1, "100.00", "100.00"));
    model.warmup(warmup).unwrap();

    let mut entry = None;
    let mut exit = None;
    for minute in 2..=60 {
        let close = match minute {
            30 => "103.00",
            60 => "100.50",
            _ => "100.00",
        };
        let decision = model.on_bar(bar(current, minute, "100.00", close)).unwrap();
        if minute == 30 {
            entry = decision;
        } else if minute == 60 {
            exit = decision;
        }
    }
    let entry = entry.unwrap();
    let exit = exit.unwrap();

    assert_eq!(entry.target, PositionTarget::Long);
    assert_eq!(entry.sigma_open, dec!(0.01));
    assert_eq!(entry.upper_bound, dec!(101.0000));
    assert_eq!(exit.target, PositionTarget::Flat);
    assert!(entry.daily_volatility > 0.0);
    assert!(entry.leverage <= dec!(4));
}

#[rstest]
fn model_filters_breakout_without_relative_volume_confirmation() {
    let calendar = sessions(16);
    let mut model_config = config(calendar.clone());
    model_config.relative_volume_threshold = Some(dec!(1));
    let mut model = IntradayMomentumModel::new(model_config).unwrap();
    let current = calendar[15];
    let mut warmup = history(&calendar[..15]);
    warmup.push(bar(current, 1, "100.00", "100.00"));
    model.warmup(warmup).unwrap();

    let mut decision = None;
    for minute in 2..=30 {
        let close = if minute == 30 { "103.00" } else { "100.00" };
        let volume = if minute == 30 { "500" } else { "1000" };
        decision = model
            .on_bar(bar_with_volume(current, minute, "100.00", close, volume))
            .unwrap()
            .or(decision);
    }
    let decision = decision.unwrap();

    assert_eq!(decision.target, PositionTarget::Flat);
    assert_eq!(decision.relative_volume, Some(dec!(0.5)));
}

#[rstest]
fn model_accepts_breakout_with_relative_volume_confirmation() {
    let calendar = sessions(16);
    let mut model_config = config(calendar.clone());
    model_config.relative_volume_threshold = Some(dec!(1));
    let mut model = IntradayMomentumModel::new(model_config).unwrap();
    let current = calendar[15];
    let mut warmup = history(&calendar[..15]);
    warmup.push(bar(current, 1, "100.00", "100.00"));
    model.warmup(warmup).unwrap();

    let mut decision = None;
    for minute in 2..=30 {
        let close = if minute == 30 { "103.00" } else { "100.00" };
        let volume = if minute == 30 { "1500" } else { "1000" };
        decision = model
            .on_bar(bar_with_volume(current, minute, "100.00", close, volume))
            .unwrap()
            .or(decision);
    }
    let decision = decision.unwrap();

    assert_eq!(decision.target, PositionTarget::Long);
    assert_eq!(decision.relative_volume, Some(dec!(1.5)));
}

#[rstest]
fn sizing_uses_equity_open_price_and_whole_lots() {
    let quantity = sized_quantity(
        dec!(100000),
        dec!(0.5),
        dec!(2),
        Price::from("501.25"),
        dec!(1),
    )
    .unwrap();

    assert_eq!(quantity, dec!(199));
}

#[rstest]
fn configuration_rejects_leverage_above_paper_cap() {
    let mut config = config(sessions(1));
    config.max_leverage = dec!(4.01);

    assert!(config.validate().is_err());
}

#[rstest]
fn configuration_rejects_capital_fraction_above_one() {
    let mut config = config(sessions(1));
    config.capital_fraction = dec!(1.01);

    assert!(config.validate().is_err());
}

#[rstest]
fn configuration_rejects_non_positive_relative_volume_threshold() {
    let mut config = config(sessions(1));
    config.relative_volume_threshold = Some(dec!(0));

    assert!(config.validate().is_err());
}

#[rstest]
fn warmup_rejects_missing_opening_minute() {
    let calendar = sessions(1);
    let mut model = IntradayMomentumModel::new(config(calendar.clone())).unwrap();

    assert!(
        model
            .warmup(vec![bar(calendar[0], 30, "100.00", "101.00")])
            .is_err()
    );
}
