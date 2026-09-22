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

use nautilus_model::{
    identifiers::{InstrumentId, Symbol},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use rstest::rstest;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::{
    EntryConfirmationMode, MarketRegime, MomentumPullbackConfig, PullbackType, SetupState,
    TrailingStopMode,
    model::{
        EntryFactors, ExitDecision, ExitFactors, ExitReason, MomentumFactors, PullbackFactors,
        ScoreWeights, atr_stop_price, available_position_slots, build_risk_snapshot,
        classify_market_regime, classify_pullback, entry_gate_status, evaluate_exit,
        is_overextended, momentum_score, percent_return, pullback_quality_score, relative_strength,
        trailing_stop_price, trend_aligned,
    },
};

fn equity() -> InstrumentAny {
    InstrumentAny::Equity(Equity::new(
        InstrumentId::from("AAPL.US.SIM"),
        Symbol::from("AAPL.US"),
        None,
        Currency::USD(),
        2,
        Price::from("0.01"),
        Some(Quantity::from("1")),
        None,
        Some(Quantity::from("1")),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0.into(),
        0.into(),
    ))
}

#[rstest]
#[case(105.0, 100.0, 0.05)]
#[case(90.0, 100.0, -0.10)]
fn test_percent_return(#[case] current: f64, #[case] previous: f64, #[case] expected: f64) {
    assert!((percent_return(current, previous).unwrap() - expected).abs() < f64::EPSILON);
}

#[rstest]
fn test_relative_strength_subtracts_market_return() {
    assert!((relative_strength(0.12, 0.04) - 0.08).abs() < f64::EPSILON);
}

#[rstest]
fn test_trend_alignment_requires_close_above_ordered_averages() {
    assert!(trend_aligned(120.0, 115.0, 110.0, 100.0));
    assert!(!trend_aligned(120.0, 105.0, 110.0, 100.0));
}

#[rstest]
fn test_momentum_score_is_bounded_and_weighted() {
    let score = momentum_score(
        MomentumFactors {
            relative_strength: 1.0,
            momentum_short: 1.0,
            momentum_medium: 1.0,
            trend: 1.0,
            volume: 1.0,
            volatility: 1.0,
        },
        ScoreWeights::default(),
    );

    assert_eq!(score, 100.0);
    assert_eq!(
        momentum_score(
            MomentumFactors {
                relative_strength: 2.0,
                momentum_short: -1.0,
                momentum_medium: 0.0,
                trend: 0.0,
                volume: 0.0,
                volatility: 0.0,
            },
            ScoreWeights::default(),
        ),
        30.0,
    );
}

#[rstest]
#[case(0.03, true, PullbackType::Shallow)]
#[case(0.05, true, PullbackType::Normal)]
#[case(0.10, true, PullbackType::Deep)]
#[case(0.101, true, PullbackType::Breakdown)]
#[case(0.05, false, PullbackType::Breakdown)]
fn test_pullback_classification(
    #[case] pullback_pct: f64,
    #[case] structure_intact: bool,
    #[case] expected: PullbackType,
) {
    let config = MomentumPullbackConfig::default();
    assert_eq!(
        classify_pullback(pullback_pct, structure_intact, &config),
        expected,
    );
}

#[rstest]
fn test_pullback_volume_contraction_scores_above_expansion() {
    let config = MomentumPullbackConfig::default();
    let contracted = pullback_quality_score(
        PullbackFactors {
            pullback_pct: 0.05,
            volume_ratio: 0.70,
            above_sma_fast: true,
            above_sma_medium: true,
            volatility_ratio: 0.80,
            structure_intact: true,
        },
        &config,
    );
    let expanded = pullback_quality_score(
        PullbackFactors {
            volume_ratio: 1.30,
            ..PullbackFactors {
                pullback_pct: 0.05,
                volume_ratio: 0.70,
                above_sma_fast: true,
                above_sma_medium: true,
                volatility_ratio: 0.80,
                structure_intact: true,
            }
        },
        &config,
    );

    assert!(contracted > expanded);
}

#[rstest]
#[case(EntryConfirmationMode::Standard, true)]
#[case(EntryConfirmationMode::Strict, false)]
#[case(EntryConfirmationMode::Relaxed, true)]
fn test_entry_confirmation_modes(#[case] mode: EntryConfirmationMode, #[case] expected: bool) {
    let factors = EntryFactors {
        close: 106.0,
        previous_close: 103.0,
        previous_high: 105.0,
        pullback_swing_high: 107.0,
        sma_fast: 102.0,
        atr: 3.0,
        volume_ratio: 1.30,
        minimum_volume_ratio: 1.20,
        maximum_extension_atr: 2.0,
    };

    assert_eq!(entry_gate_status(mode, factors).confirmed(), expected);
}

#[rstest]
fn test_entry_rejects_volume_failure_and_overextension() {
    let low_volume = EntryFactors {
        close: 106.0,
        previous_close: 103.0,
        previous_high: 105.0,
        pullback_swing_high: 105.0,
        sma_fast: 102.0,
        atr: 3.0,
        volume_ratio: 1.10,
        minimum_volume_ratio: 1.20,
        maximum_extension_atr: 2.0,
    };

    assert!(!entry_gate_status(EntryConfirmationMode::Standard, low_volume).confirmed());
    assert!(is_overextended(109.0, 102.0, 3.0, 2.0));
}

#[rstest]
#[case(1.10, 106.0, 105.0, false, true, true)]
#[case(1.30, 104.0, 105.0, true, false, true)]
#[case(1.30, 109.0, 108.0, true, true, false)]
fn test_entry_gate_status_identifies_each_rejection_condition(
    #[case] volume_ratio: f64,
    #[case] close: f64,
    #[case] previous_high: f64,
    #[case] volume_confirmed: bool,
    #[case] breakout_confirmed: bool,
    #[case] extension_acceptable: bool,
) {
    let status = entry_gate_status(
        EntryConfirmationMode::Standard,
        EntryFactors {
            close,
            previous_close: 103.0,
            previous_high,
            pullback_swing_high: 110.0,
            sma_fast: 102.0,
            atr: 3.0,
            volume_ratio,
            minimum_volume_ratio: 1.20,
            maximum_extension_atr: 2.0,
        },
    );

    assert!(status.trend_supported);
    assert_eq!(status.volume_confirmed, volume_confirmed);
    assert_eq!(status.breakout_confirmed, breakout_confirmed);
    assert_eq!(status.extension_acceptable, extension_acceptable);
    assert_eq!(status.confirmed(), false);
    assert_eq!(status.failed_count(), 1);
}

#[rstest]
fn test_atr_stop_uses_farther_structure_stop_and_caps_distance() {
    assert_eq!(
        atr_stop_price(dec!(100), dec!(4), dec!(1.5), dec!(96), dec!(1), dec!(0.08)),
        Some(dec!(94.0)),
    );
    assert_eq!(
        atr_stop_price(dec!(100), dec!(6), dec!(1.5), dec!(96), dec!(1), dec!(0.08)),
        None,
    );
}

#[rstest]
fn test_position_sizing_uses_exact_fixed_risk_and_lot_size() {
    let snapshot = build_risk_snapshot(
        &equity(),
        Price::from("100.00"),
        Price::from("95.00"),
        Money::from("100000 USD"),
        dec!(0.005),
        Decimal::ONE,
        Decimal::ZERO,
        Decimal::ONE,
        None,
        Quantity::from(1),
    )
    .unwrap();

    assert_eq!(snapshot.risk_amount, dec!(500));
    assert_eq!(snapshot.risk_per_share, dec!(5));
    assert_eq!(snapshot.position_size, Quantity::from("100"));
}

#[rstest]
fn test_maximum_positions_counts_open_and_inflight_entries() {
    assert_eq!(available_position_slots(10, 6, 2), 2);
    assert_eq!(available_position_slots(10, 9, 2), 0);
}

#[rstest]
#[case(210.0, 205.0, MarketRegime::Bull)]
#[case(195.0, 190.0, MarketRegime::Bear)]
#[case(195.0, 210.0, MarketRegime::Neutral)]
fn test_market_regimes(
    #[case] close: f64,
    #[case] sma_medium: f64,
    #[case] expected: MarketRegime,
) {
    assert_eq!(classify_market_regime(close, sma_medium, 200.0), expected);
}

#[rstest]
fn test_chandelier_trailing_stop() {
    assert_eq!(
        trailing_stop_price(
            TrailingStopMode::AtrChandelier,
            120.0,
            2.0,
            3.0,
            110.0,
            108.0
        ),
        114.0,
    );
}

#[rstest]
#[case(
    ExitFactors { current_price: 110.0, initial_risk_per_share: 5.0, entry_price: 100.0,
        highest_high: 110.0, atr: 2.0, sma_fast: 104.0, swing_low: 103.0,
        bars_held: 2, partial_taken: false, partial_exit_r: 2.0, time_stop_days: 5,
        partial_exit_fraction: 0.5, time_stop_min_r: 0.5, maximum_holding_days: 30,
        trailing_mode: TrailingStopMode::AtrChandelier,
        chandelier_multiple: 3.0 },
    Some(ExitDecision::Partial { fraction: 0.5 })
)]
#[case(
    ExitFactors { current_price: 103.0, initial_risk_per_share: 5.0, entry_price: 100.0,
        highest_high: 112.0, atr: 2.0, sma_fast: 104.0, swing_low: 102.0,
        bars_held: 3, partial_taken: true, partial_exit_r: 2.0, time_stop_days: 5,
        partial_exit_fraction: 0.5, time_stop_min_r: 0.5, maximum_holding_days: 30,
        trailing_mode: TrailingStopMode::AtrChandelier,
        chandelier_multiple: 3.0 },
    Some(ExitDecision::Full { reason: ExitReason::TrailingStop })
)]
#[case(
    ExitFactors { current_price: 99.0, initial_risk_per_share: 5.0, entry_price: 100.0,
        highest_high: 104.0, atr: 2.0, sma_fast: 98.0, swing_low: 97.0,
        bars_held: 5, partial_taken: true, partial_exit_r: 2.0, time_stop_days: 5,
        partial_exit_fraction: 0.5, time_stop_min_r: 0.5, maximum_holding_days: 30,
        trailing_mode: TrailingStopMode::AtrChandelier,
        chandelier_multiple: 3.0 },
    Some(ExitDecision::Full { reason: ExitReason::TimeStop })
)]
fn test_exit_decisions(#[case] factors: ExitFactors, #[case] expected: Option<ExitDecision>) {
    assert_eq!(evaluate_exit(factors), expected);
}

#[rstest]
fn test_ma20_exit() {
    let decision = evaluate_exit(ExitFactors {
        current_price: 99.0,
        initial_risk_per_share: 5.0,
        entry_price: 100.0,
        highest_high: 110.0,
        atr: 2.0,
        sma_fast: 100.0,
        swing_low: 98.0,
        bars_held: 3,
        partial_taken: true,
        partial_exit_r: 2.0,
        partial_exit_fraction: 0.5,
        time_stop_days: 5,
        time_stop_min_r: 0.5,
        maximum_holding_days: 30,
        trailing_mode: TrailingStopMode::MaFast,
        chandelier_multiple: 3.0,
    });

    assert_eq!(
        decision,
        Some(ExitDecision::Full {
            reason: ExitReason::TrailingStop,
        }),
    );
}

#[rstest]
fn test_maximum_holding_period_forces_exit() {
    let decision = evaluate_exit(ExitFactors {
        current_price: 110.0,
        initial_risk_per_share: 5.0,
        entry_price: 100.0,
        highest_high: 115.0,
        atr: 2.0,
        sma_fast: 105.0,
        swing_low: 104.0,
        bars_held: 30,
        partial_taken: true,
        partial_exit_r: 2.0,
        partial_exit_fraction: 0.5,
        time_stop_days: 5,
        time_stop_min_r: 0.5,
        maximum_holding_days: 30,
        trailing_mode: TrailingStopMode::AtrChandelier,
        chandelier_multiple: 3.0,
    });

    assert_eq!(
        decision,
        Some(ExitDecision::Full {
            reason: ExitReason::MaximumHoldingPeriod,
        }),
    );
}

#[rstest]
fn test_setup_state_machine_rejects_invalid_jump() {
    assert!(SetupState::Watchlist.can_transition_to(SetupState::Qualified));
    assert!(SetupState::Pullback.can_transition_to(SetupState::Invalidated));
    assert!(!SetupState::Watchlist.can_transition_to(SetupState::Long));
}
