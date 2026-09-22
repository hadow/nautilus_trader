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
    data::Bar,
    identifiers::InstrumentId,
    instruments::{InstrumentAny, stubs::equity_aapl},
    types::{Money, Price, Quantity},
};
use rstest::rstest;
use rust_decimal_macros::dec;

use super::{
    data::{MINUTE, MinuteBatch, SessionAggregator, SymbolFeatures},
    risk::{stop_price, target_price, trailing_stop},
    selection::{correlation, percentiles},
    signal::{KeyLevelDetector, LevelKind, market_regime},
    strategy::{excursion_state, profit_capture_ratio},
    structure::{DeliveryConfirmation, HTFStructureDetector},
    *,
};

fn session() -> Session {
    Session {
        open: UnixNanos::from(1_735_828_200_000_000_000),
        close: UnixNanos::from(1_735_851_600_000_000_000),
    }
}

fn bar(minute: u64, open: &str, high: &str, low: &str, close: &str) -> Bar {
    let ts = UnixNanos::from(session().open.as_u64() + minute * MINUTE);
    Bar::new(
        minute_bar_type("AAPL.XNAS".into()),
        Price::from(open),
        Price::from(high),
        Price::from(low),
        Price::from(close),
        Quantity::from(1000),
        ts,
        ts,
    )
}

pub(super) fn config() -> SlcMomentumConfig {
    SlcMomentumConfig {
        sessions: vec![session()],
        universe: vec![SymbolMetadata {
            instrument_id: "AAPL.XNAS".into(),
            sector: "technology".to_string(),
            sector_etf: "XLK.SIM".into(),
            market_cap: dec!(1000000000),
            effective_from: UnixNanos::default(),
            effective_until: u64::MAX.into(),
            known_at: UnixNanos::default(),
        }],
        ..SlcMomentumConfig::default()
    }
}

pub(super) fn signal() -> SlcSignal {
    SlcSignal {
        symbol: "AAPL.XNAS".into(),
        timestamp: session().open,
        available_at: session().open,
        side: TradeSide::Long,
        intraday_return: 0.02,
        sector: "technology".to_string(),
        momentum: MomentumRank {
            symbol: "AAPL.XNAS".into(),
            timestamp: session().open,
            daily_cutoff: 1.into(),
            rank: 1,
            percentile: 95.0,
            score: 90.0,
            returns: vec![0.1; 4],
            relative_strength: [0.1; 3],
            relative_volume: 2.0,
            average_dollar_volume: dec!(100000000),
            average_volume: dec!(1000000),
            atr_fraction: 0.02,
            gap: 0.01,
        },
        market_regime: MarketRegime::Bullish,
        structure: Structure::Bullish,
        structure_score: 2.0,
        structure_confirmed_at: None,
        cisd_confirmed_at: None,
        cisd_level: None,
        level_type: "DEMAND".to_string(),
        level_tests: 1,
        level_breaks: 0,
        level_reclaimed_at: None,
        level_low: Price::from("98.00"),
        level_high: Price::from("99.00"),
        level_created: 1.into(),
        level_score: 8.0,
        confirmation_score: 10.0,
        confirmation_flags: u8::MAX,
        confirmation_enabled: u8::MAX,
        stochastic_k: 25.0,
        stochastic_d: 18.0,
        vwap: 99.0,
        anchored_vwap: 99.0,
        relative_volume: 2.0,
        entry_price: Price::from("100.00"),
        atr: dec!(1),
        setup_type: "MOMENTUM_SLC_PULLBACK".to_string(),
    }
}

#[rstest]
#[case(dec!(0), dec!(-10), "NEVER_FAVORABLE")]
#[case(dec!(25), dec!(-10), "FAVORABLE_UNCAPTURED")]
#[case(dec!(25), dec!(10), "FAVORABLE_CAPTURED")]
fn test_excursion_state(
    #[case] mfe: rust_decimal::Decimal,
    #[case] pnl: rust_decimal::Decimal,
    #[case] expected: &str,
) {
    assert_eq!(excursion_state(mfe, pnl), expected);
}

#[rstest]
fn test_configuration_rejects_invalid_inputs() {
    let mut c = config();
    c.validate().unwrap();
    c.momentum.return_weights[0] = f64::NAN;
    assert!(c.validate().is_err());
    c = config();
    c.momentum.return_weights[0] = f64::MAX;
    assert!(c.validate().is_err());
    c = config();
    c.momentum.refresh_minutes = u64::MAX;
    assert!(c.validate().is_err());
    c = config();
    c.sessions[0].open = UnixNanos::from(c.sessions[0].open.as_u64() + 1);
    c.sessions[0].close = UnixNanos::from(c.sessions[0].close.as_u64() + 1);
    assert!(c.validate().is_err());
    c = config();
    c.slc.stochastic_smoothing = 0;
    assert!(c.validate().is_err());
    c = config();
    c.slc.max_level_breaks = 2;
    assert!(c.validate().is_err());
    c = config();
    c.slc.reclaim_impulse_atr = f64::NAN;
    assert!(c.validate().is_err());
    c = config();
    c.longbridge = true;
    c.entry_mode = EntryMode::Stop;
    assert!(c.validate().is_err());
}

#[rstest]
fn test_momentum_ranker_ties_are_order_independent() {
    let a = InstrumentId::from("A.SIM");
    let b = InstrumentId::from("B.SIM");
    let c = InstrumentId::from("C.SIM");
    let values = [(a, 1.0), (b, 1.0), (c, 2.0)];
    let ranked = percentiles(&values);
    assert_eq!(ranked[&a], 25.0);
    assert_eq!(ranked[&b], 25.0);
    assert_eq!(ranked[&c], 100.0);
    assert_eq!(
        ranked,
        percentiles(&values.into_iter().rev().collect::<Vec<_>>())
    );
}

#[rstest]
fn test_no_lookahead_batch_and_availability() {
    let first = bar(1, "100", "101", "99", "100");
    let mut batch = MinuteBatch::default();
    batch.push(first).unwrap();
    assert!(batch.flush(first.ts_event).is_empty());
    assert_eq!(
        batch.flush(bar(2, "100", "101", "99", "100").ts_event),
        vec![first]
    );
    assert!(batch.push(first).is_err());
    assert!(data::validate_bar(&first, UnixNanos::from(first.ts_init.as_u64() - 1)).is_err());
}

#[rstest]
fn test_session_aggregation_missing_minute_and_short_bucket() {
    let mut aggregator = SessionAggregator::default();
    for n in 1..5 {
        assert!(
            aggregator
                .update(bar(n, "100", "102", "99", "101"), session(), 5)
                .is_none()
        );
    }
    let complete = aggregator
        .update(bar(5, "101", "103", "100", "102"), session(), 5)
        .unwrap();
    assert_eq!(complete.volume, Quantity::from(5000));
    assert_eq!(complete.high, Price::from("103"));
    for n in [6, 7, 9, 10] {
        assert!(
            aggregator
                .update(bar(n, "100", "102", "99", "101"), session(), 5)
                .is_none()
        );
    }
    let half = Session {
        close: bar(210, "100", "101", "99", "100").ts_event,
        ..session()
    };
    let mut hour = SessionAggregator::default();
    let completed = (1..=210)
        .filter_map(|n| hour.update(bar(n, "100", "102", "99", "101"), half, 60))
        .count();
    assert_eq!(completed, 3);
}

#[rstest]
fn test_htf_structure_waits_for_confirmed_pivots() {
    let c = SlcConfig {
        require_htf_ema: false,
        ..SlcConfig::default()
    };
    let mut detector = HTFStructureDetector::new(&c);
    let sequence = [
        ("101", "99"),
        ("105", "101"),
        ("103", "100"),
        ("106", "102"),
        ("104", "101"),
        ("107", "103"),
    ];
    for (index, (high, low)) in sequence.into_iter().enumerate() {
        detector.update(bar(index as u64 + 1, low, high, low, high));
        if index < 5 {
            assert_eq!(detector.structure, Structure::Range);
        }
    }
    assert_eq!(detector.structure, Structure::Bullish);
    detector.update(bar(7, "101", "102", "99", "100"));
    assert_eq!(detector.structure, Structure::Range);
}

#[rstest]
#[case(LevelKind::Demand, "100", "104", "99", "104")]
#[case(LevelKind::Supply, "100", "101", "96", "96")]
fn test_demand_and_supply_level(
    #[case] kind: LevelKind,
    #[case] open: &str,
    #[case] high: &str,
    #[case] low: &str,
    #[case] close: &str,
) {
    let c = SlcConfig {
        impulse_atr: 1.0,
        impulse_volume: 1.0,
        ..SlcConfig::default()
    };
    let mut f = SymbolFeatures::new(&c);
    f.atr.value = 1.0;
    f.atr.initialized = true;
    f.ltf_volume.extend([1000.0; 5]);
    let mut levels = KeyLevelDetector::default();
    levels.update(bar(5, "100", "101", "99", "100"), &f, &c, Ablation::F);
    levels.update(bar(10, open, high, low, close), &f, &c, Ablation::F);
    assert_eq!(levels.levels.len(), 1);
    assert_eq!(levels.levels[0].kind, kind);
    assert_eq!(
        levels.levels[0].created,
        bar(10, open, high, low, close).ts_event
    );
}

fn demand() -> (KeyLevelDetector, SymbolFeatures, SlcConfig) {
    let c = SlcConfig {
        impulse_atr: 1.0,
        impulse_volume: 1.0,
        ..SlcConfig::default()
    };
    let mut f = SymbolFeatures::new(&c);
    f.atr.value = 1.0;
    f.atr.initialized = true;
    f.ltf_volume.extend([1000.0; 5]);
    let mut levels = KeyLevelDetector::default();
    levels.update(bar(5, "100", "101", "99", "100"), &f, &c, Ablation::F);
    levels.update(bar(10, "100", "104", "100", "104"), &f, &c, Ablation::F);
    (levels, f, c)
}

#[rstest]
fn test_level_freshness_and_distinct_tests() {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    level.observe(bar(15, "101", "102", "100", "101"), &c, Some(10.0), 1.0);
    level.observe(bar(20, "101", "102", "100", "101"), &c, Some(15.0), 1.0);
    assert_eq!(level.tests, 1);
    level.observe(bar(25, "103", "104", "102", "103"), &c, Some(25.0), 1.0);
    level.observe(bar(30, "101", "102", "100", "101"), &c, Some(10.0), 1.0);
    assert_eq!(level.tests, 2);
    assert!(!level.confirmed(
        bar(35, "101", "103", "100", "102"),
        25.0,
        Some(10.0),
        &c,
        Ablation::F
    ));
}

#[rstest]
fn test_stochastic_confirmation_needs_touch_extreme_and_reentry() {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    let rebound = bar(20, "100", "103", "100", "102");
    assert!(!level.confirmed(rebound, 25.0, Some(10.0), &c, Ablation::F));
    level.observe(bar(15, "101", "102", "100", "100"), &c, Some(10.0), 1.0);
    assert!(level.confirmed(rebound, 25.0, Some(10.0), &c, Ablation::F));
    assert!(!level.confirmed(rebound, 30.0, Some(25.0), &c, Ablation::F));
}

#[rstest]
#[case(LevelKind::Demand)]
#[case(LevelKind::Supply)]
fn test_broken_level_requires_strong_reclaim_then_later_retest(#[case] kind: LevelKind) {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    level.kind = kind;
    let (broken, continued, weak, reclaimed, retest, response, extreme, reentry) = match kind {
        LevelKind::Demand => (
            bar(15, "102", "103", "97", "98"),
            bar(20, "98", "99", "96", "97"),
            bar(25, "101.8", "102.1", "101.7", "102.0"),
            bar(30, "98", "103", "97", "102"),
            bar(35, "102.0", "102.0", "100.0", "100.5"),
            bar(40, "100.5", "103.0", "100.0", "102.0"),
            10.0,
            25.0,
        ),
        LevelKind::Supply => (
            bar(15, "98", "103", "97", "102"),
            bar(20, "102", "104", "101", "103"),
            bar(25, "98.2", "98.3", "97.9", "98.0"),
            bar(30, "102", "103", "97", "98"),
            bar(35, "98.0", "100.0", "98.0", "99.5"),
            bar(40, "99.5", "100.0", "97.0", "98.0"),
            90.0,
            75.0,
        ),
    };
    level.observe(broken, &c, Some(extreme), 1.0);
    level.observe(continued, &c, Some(extreme), 1.0);
    assert_eq!(level.breaks, 1);
    assert_eq!(level.tests, 0);
    assert!(!level.available());
    level.observe(weak, &c, Some(reentry), 1.0);
    assert!(!level.available());
    level.observe(reclaimed, &c, Some(reentry), 1.0);
    assert!(level.available());
    assert!(!level.confirmed(reclaimed, reentry, Some(extreme), &c, Ablation::F));
    assert!(!level.confirmed(response, reentry, Some(extreme), &c, Ablation::F));
    level.observe(retest, &c, Some(extreme), 1.0);
    assert_eq!(level.tests, 1);
    assert!(level.confirmed(response, reentry, Some(extreme), &c, Ablation::F));
    assert!(!level.confirmed(response, extreme, Some(extreme), &c, Ablation::F));
}

#[rstest]
fn test_second_adverse_excursion_invalidates_even_without_a_strong_reclaim() {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    level.observe(bar(15, "102", "103", "97", "98"), &c, Some(10.0), 1.0);
    level.observe(
        bar(20, "101.8", "102.1", "101.7", "102.0"),
        &c,
        Some(25.0),
        1.0,
    );
    level.observe(bar(25, "102", "103", "97", "98"), &c, Some(10.0), 1.0);
    level.observe(bar(30, "98", "103", "97", "102"), &c, Some(25.0), 1.0);
    assert_eq!(level.breaks, 2);
    assert!(!level.available());
    assert!(!level.confirmed(
        bar(35, "100", "103", "100", "102"),
        25.0,
        Some(10.0),
        &c,
        Ablation::F
    ));
}

#[rstest]
fn test_break_retest_can_be_disabled_and_broken_levels_still_expire() {
    let (levels, _, mut c) = demand();
    let mut disabled = levels.levels[0].clone();
    c.max_level_breaks = 0;
    disabled.observe(bar(15, "102", "103", "97", "98"), &c, None, 1.0);
    disabled.observe(bar(20, "98", "103", "97", "102"), &c, None, 1.0);
    assert!(!disabled.available());
    let mut expired = levels.levels[0].clone();
    c.max_level_breaks = 1;
    c.max_level_age_bars = 1;
    expired.observe(bar(15, "102", "103", "97", "98"), &c, None, 1.0);
    expired.observe(bar(20, "98", "103", "97", "102"), &c, None, 1.0);
    assert!(!expired.available());
}

#[rstest]
fn test_reclaim_does_not_reset_prior_touch_limits() {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    level.observe(bar(15, "101", "102", "100", "100"), &c, Some(10.0), 1.0);
    level.observe(bar(20, "102", "103", "97", "98"), &c, Some(10.0), 1.0);
    level.observe(bar(25, "98", "103", "97", "102"), &c, Some(25.0), 1.0);
    level.observe(
        bar(30, "102.0", "102.0", "100.0", "100.5"),
        &c,
        Some(10.0),
        1.0,
    );
    assert_eq!(level.tests, 2);
    assert_eq!(level.breaks, 1);
    assert!(!level.available());
}

#[rstest]
fn test_wick_outside_zone_is_a_touch_not_a_close_break() {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    level.observe(bar(15, "102", "103", "98", "100"), &c, Some(10.0), 1.0);
    assert_eq!(level.breaks, 0);
    assert_eq!(level.tests, 1);
    assert!(level.available());
}

#[rstest]
fn test_position_sizing_and_short_signal() {
    let c = config();
    let risk = PortfolioRiskManager::default();
    let instrument = InstrumentAny::Equity(equity_aapl());
    let mut s = signal();
    let allocation = risk
        .allocate(
            &s,
            &instrument,
            s.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None,
        )
        .unwrap();
    assert!(allocation.reserved_risk <= dec!(500));
    assert!(allocation.notional <= c.risk.max_position_value);
    assert!(allocation.quantity.as_decimal() <= dec!(1000));
    s.side = TradeSide::Short;
    assert_eq!(
        risk.allocate(
            &s,
            &instrument,
            s.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None
        )
        .unwrap_err(),
        NoTrade::ShortDisabled
    );
}

#[rstest]
fn test_stop_loss_take_profit_and_trailing_exit() {
    let instrument = InstrumentAny::Equity(equity_aapl());
    let c = config();
    let stop = stop_price(
        Price::from("98.00"),
        dec!(1.03),
        &instrument,
        &c.risk,
        TradeSide::Long,
    )
    .unwrap();
    assert_eq!(stop, Price::from("97.79"));
    let target = target_price(
        Price::from("100.00"),
        stop,
        dec!(1),
        &instrument,
        &c.exit,
        None,
        TradeSide::Long,
    )
    .unwrap();
    assert_eq!(target, Price::from("104.42"));
    let trailed = trailing_stop(
        stop,
        Price::from("103.00"),
        Price::from("105.00"),
        dec!(1),
        &instrument,
        &c.exit,
        TradeSide::Long,
    )
    .unwrap();
    assert_eq!(trailed, Price::from("102.00"));
    assert_eq!(
        trailing_stop(
            trailed,
            Price::from("101.00"),
            Price::from("102.00"),
            dec!(2),
            &instrument,
            &c.exit,
            TradeSide::Long
        )
        .unwrap(),
        trailed
    );
}

#[rstest]
fn test_daily_loss_limit_latches_until_next_session() {
    let c = RiskConfig::default();
    let mut risk = PortfolioRiskManager::default();
    risk.mark_equity(1.into(), dec!(100000), &c);
    risk.mark_equity(1.into(), dec!(98000), &c);
    assert!(risk.halted);
    risk.mark_equity(1.into(), dec!(100000), &c);
    assert!(risk.halted);
    risk.mark_equity(2.into(), dec!(100000), &c);
    assert!(!risk.halted);
}

#[rstest]
fn test_sector_exposure_and_duplicate_order() {
    let mut c = config();
    c.risk.max_sector_positions = 1;
    let mut risk = PortfolioRiskManager::default();
    let s = signal();
    let instrument = InstrumentAny::Equity(equity_aapl());
    let allocation = risk
        .allocate(
            &s,
            &instrument,
            s.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None,
        )
        .unwrap();
    risk.reserve(s.symbol, s.sector.clone(), &allocation)
        .unwrap();
    assert_eq!(
        risk.reserve(s.symbol, s.sector.clone(), &allocation),
        Err(NoTrade::DuplicateOrder)
    );
    let mut other = s;
    other.symbol = "AMD.XNAS".into();
    assert_eq!(
        risk.allocate(
            &other,
            &instrument,
            other.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None
        )
        .unwrap_err(),
        NoTrade::MaxExposure
    );
}

#[rstest]
fn test_market_regime_requires_all_benchmarks() {
    assert!(market_regime(&Default::default(), session().open, &config()).is_none());
}

#[rstest]
fn test_rank_scores_survive_json_round_trip() {
    let mut rank = signal().momentum;
    for score in [21.553884711779446, 21.55388471177945] {
        rank.score = score;
        let encoded = serde_json::to_vec(&rank).unwrap();
        let decoded: super::MomentumRank = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            decoded.score.to_bits(),
            rank.score.to_bits(),
            "score={score}"
        );
    }
}

#[rstest]
fn test_no_trade_regime_and_long_signal_gates() {
    let (mut levels, f, _) = demand();
    let c = config();
    let s = signal();
    assert_eq!(
        levels
            .evaluate(
                bar(20, "100", "103", "100", "102"),
                &f,
                &s.momentum,
                MarketRegime::Bearish,
                &c,
                session().close
            )
            .unwrap_err(),
        NoTrade::MarketBearish
    );
    assert_eq!(
        levels
            .evaluate(
                bar(20, "100", "103", "100", "102"),
                &f,
                &s.momentum,
                MarketRegime::Bullish,
                &c,
                session().close
            )
            .unwrap_err(),
        NoTrade::HtfRange
    );
}

#[rstest]
fn test_correlation_uses_returns_and_rejects_zero_variance() {
    assert_eq!(correlation(&[0.1, 0.2, -0.1], &[0.2, 0.4, -0.2]), Some(1.0));
    assert_eq!(correlation(&[0.1, 0.1], &[0.2, 0.4]), None);
}

#[rstest]
fn test_long_signal_requires_all_slc_gates() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.minimum_confirmation_volume = 1.0;
    f.structure.structure = Structure::Bullish;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 25.0;
    f.stochastic.value_d = 15.0;
    f.previous_k = Some(10.0);
    f.vwap.value = 101.0;
    let mut untouched = levels.levels[0].clone();
    untouched.volume = 2.0;
    untouched.created = bar(15, "100", "101", "99", "100").ts_event;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, Some(10.0), 1.0);
    // A higher-scored untouched zone must not hide a confirmed setup at another zone
    levels.levels.push_back(untouched);
    let signal = levels
        .evaluate(
            bar(20, "100", "103", "100", "102"),
            &f,
            &signal().momentum,
            MarketRegime::Bullish,
            &c,
            bar(21, "100", "101", "99", "100").ts_event,
        )
        .unwrap();
    assert_eq!(signal.side, TradeSide::Long);
    assert_eq!(
        signal.level_created,
        bar(10, "100", "101", "99", "100").ts_event
    );
    assert!(signal.available_at > signal.timestamp);
    assert!(signal.confirmation_score >= c.slc.confirmation_threshold);
    assert!(
        levels
            .evaluate(
                bar(25, "100", "103", "100", "102"),
                &f,
                &signal.momentum,
                MarketRegime::Bullish,
                &c,
                session().close
            )
            .is_err()
    );
}

#[rstest]
fn test_slc_only_skips_auxiliary_signal_gates() {
    let (_, mut f, _) = demand();
    let mut c = config();
    c.ablation = Ablation::SlcOnly;
    c.slc.impulse_volume = 2.0;
    c.slc.require_intraday_trend = true;
    let mut levels = KeyLevelDetector::default();
    levels.update(bar(5, "100", "101", "99", "100"), &f, &c.slc, c.ablation);
    levels.update(bar(10, "100", "104", "100", "104"), &f, &c.slc, c.ablation);
    assert_eq!(levels.levels.len(), 1);
    f.structure.structure = Structure::Bullish;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 25.0;
    f.previous_k = Some(10.0);
    f.vwap.value = 103.0;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, Some(10.0), 1.0);
    let mut rank = signal().momentum;
    rank.percentile = 1.0;

    let selected = levels
        .evaluate(
            bar(20, "100", "103", "100", "102"),
            &f,
            &rank,
            MarketRegime::Bearish,
            &c,
            session().close,
        )
        .unwrap();

    assert_eq!(selected.side, TradeSide::Long);
    assert_eq!(selected.confirmation_enabled & 0b0011_0010, 0);
}

#[rstest]
fn test_weighted_evidence_accepts_range_when_other_evidence_is_strong() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.confirmation_mode = ConfirmationMode::WeightedEvidence;
    c.slc.confirmation_threshold = 8.0;
    c.slc.neutral_extra_score = 0.0;
    c.slc.min_level_score = 5.0;
    f.structure.structure = Structure::Range;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 25.0;
    f.previous_k = Some(10.0);
    f.vwap.value = 101.0;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, Some(10.0), 1.0);
    let mut response = bar(20, "100", "103", "100", "102");
    response.volume = Quantity::from(2000);

    let selected = levels
        .evaluate(
            response,
            &f,
            &signal().momentum,
            MarketRegime::Neutral,
            &c,
            response.ts_event,
        )
        .unwrap();

    assert_eq!(selected.structure, Structure::Range);
    assert_eq!(selected.confirmation_score, 8.0);
    assert_eq!(selected.confirmation_flags & 1, 0);
    assert_eq!(selected.confirmation_enabled, u8::MAX);
}

#[rstest]
fn test_weighted_evidence_rejects_opposite_htf_structure() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.confirmation_mode = ConfirmationMode::WeightedEvidence;
    c.slc.confirmation_threshold = 8.0;
    c.slc.neutral_extra_score = 0.0;
    f.structure.structure = Structure::Bearish;

    assert_eq!(
        levels
            .evaluate(
                bar(20, "100", "103", "100", "102"),
                &f,
                &signal().momentum,
                MarketRegime::Neutral,
                &c,
                session().close,
            )
            .unwrap_err(),
        NoTrade::HtfRange,
    );
}

#[rstest]
fn test_weighted_evidence_can_offset_low_level_quality() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.confirmation_mode = ConfirmationMode::WeightedEvidence;
    c.slc.confirmation_threshold = 8.0;
    c.slc.neutral_extra_score = 0.0;
    f.structure.structure = Structure::Bullish;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 25.0;
    f.previous_k = Some(10.0);
    f.vwap.value = 101.0;
    levels.levels[0].impulse = 1.0;
    levels.levels[0].volume = 1.2;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, Some(10.0), 1.0);
    let mut response = bar(20, "100", "103", "100", "102");
    response.volume = Quantity::from(2000);

    let selected = levels
        .evaluate(
            response,
            &f,
            &signal().momentum,
            MarketRegime::Neutral,
            &c,
            response.ts_event,
        )
        .unwrap();

    assert!(selected.level_score < c.slc.min_level_score);
    assert_eq!(selected.confirmation_score, 8.0);
}

#[rstest]
fn test_weighted_evidence_can_confirm_without_stochastic() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.confirmation_mode = ConfirmationMode::WeightedEvidence;
    c.slc.confirmation_threshold = 8.0;
    c.slc.neutral_extra_score = 0.0;
    f.structure.structure = Structure::Bullish;
    f.vwap.value = 101.0;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, None, 1.0);
    let mut response = bar(20, "100", "103", "100", "102");
    response.volume = Quantity::from(2000);

    let selected = levels
        .evaluate(
            response,
            &f,
            &signal().momentum,
            MarketRegime::Neutral,
            &c,
            response.ts_event,
        )
        .unwrap();

    assert_eq!(selected.confirmation_score, 9.0);
}

#[rstest]
fn test_weighted_evidence_can_confirm_below_volume_threshold() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.confirmation_mode = ConfirmationMode::WeightedEvidence;
    c.slc.confirmation_threshold = 8.0;
    c.slc.neutral_extra_score = 0.0;
    f.structure.structure = Structure::Bullish;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 25.0;
    f.previous_k = Some(10.0);
    f.vwap.value = 101.0;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, Some(10.0), 1.0);
    let response = bar(20, "100", "103", "100", "102");

    let selected = levels
        .evaluate(
            response,
            &f,
            &signal().momentum,
            MarketRegime::Neutral,
            &c,
            response.ts_event,
        )
        .unwrap();

    assert!(selected.relative_volume < c.slc.minimum_confirmation_volume);
    assert_eq!(selected.confirmation_score, 9.0);
    assert_eq!(selected.confirmation_flags & (1 << 5), 0);
}

#[rstest]
#[case(TradeSide::Long)]
#[case(TradeSide::Short)]
fn test_weighted_evidence_rejects_stochastic_exhaustion(#[case] side: TradeSide) {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    c.slc.confirmation_mode = ConfirmationMode::WeightedEvidence;
    c.slc.confirmation_threshold = 8.0;
    c.slc.neutral_extra_score = 0.0;
    let mut rank = signal().momentum;
    let (k, touch, response, regime) = match side {
        TradeSide::Long => (
            95.0,
            bar(15, "101", "102", "100", "100"),
            bar(20, "100", "103", "100", "102"),
            MarketRegime::Bullish,
        ),
        TradeSide::Short => {
            levels.levels[0].kind = LevelKind::Supply;
            rank.percentile = 5.0;
            (
                5.0,
                bar(15, "99", "100", "98", "100"),
                bar(20, "100", "100", "97", "98"),
                MarketRegime::Bearish,
            )
        }
    };
    f.structure.structure = side.structure();
    f.stochastic.initialized = true;
    f.stochastic.value_k = k;
    f.vwap.value = 100.0;
    levels.levels[0].observe(touch, &c.slc, None, 1.0);
    let mut response = response;
    response.volume = Quantity::from(2000);

    assert_eq!(
        levels
            .evaluate(response, &f, &rank, regime, &c, response.ts_event)
            .unwrap_err(),
        NoTrade::ConfirmationMissing,
    );
}

#[rstest]
#[case(TradeSide::Long)]
#[case(TradeSide::Short)]
fn test_stochastic_reentry_can_wait_for_volume_within_touch_window(#[case] side: TradeSide) {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    let mut rank = signal().momentum;
    let (touch, reentry, expansion, extreme, crossed, later, regime) = match side {
        TradeSide::Long => (
            bar(15, "101", "102", "100", "100"),
            bar(20, "100", "103", "100", "102"),
            bar(25, "101.5", "103.0", "101.5", "102.0"),
            10.0,
            25.0,
            30.0,
            MarketRegime::Bullish,
        ),
        TradeSide::Short => {
            levels.levels[0].kind = LevelKind::Supply;
            rank.percentile = 5.0;
            (
                bar(15, "99", "100", "98", "100"),
                bar(20, "100", "100", "97", "98"),
                bar(25, "98.5", "98.5", "97.0", "98.0"),
                90.0,
                75.0,
                70.0,
                MarketRegime::Bearish,
            )
        }
    };
    f.structure.structure = side.structure();
    f.vwap.value = 100.0;
    f.stochastic.initialized = true;
    f.stochastic.value_k = extreme;
    levels.update(touch, &f, &c.slc, c.ablation);
    f.stochastic.value_k = crossed;
    f.previous_k = Some(extreme);
    levels.update(reentry, &f, &c.slc, c.ablation);
    assert_eq!(
        levels
            .evaluate(reentry, &f, &rank, regime, &c, reentry.ts_event)
            .unwrap_err(),
        NoTrade::RvolLow,
    );
    f.ltf_volume.clear();
    f.ltf_volume.extend([500.0; 5]);
    f.stochastic.value_k = later;
    f.previous_k = Some(crossed);
    levels.update(expansion, &f, &c.slc, c.ablation);
    let selected = levels
        .evaluate(expansion, &f, &rank, regime, &c, expansion.ts_event)
        .unwrap();
    assert_eq!(selected.side, side);
    assert_eq!(selected.timestamp, expansion.ts_event);
    assert!(selected.relative_volume >= c.slc.minimum_confirmation_volume);
    assert!(!levels.levels[0].available());
}

#[rstest]
fn test_retained_stochastic_confirmation_expires_and_resets() {
    let (mut levels, _, mut c) = demand();
    c.max_level_tests = 2;
    let level = &mut levels.levels[0];
    let touch = bar(15, "101", "102", "100", "100");
    let cross = bar(20, "100", "103", "100", "102");
    let response = bar(25, "102", "103", "102", "103");
    level.observe(touch, &c, Some(10.0), 1.0);
    level.observe(cross, &c, Some(25.0), 1.0);
    assert!(level.confirmed(response, 30.0, Some(25.0), &c, Ablation::F));
    assert!(!level.confirmed(touch, 30.0, Some(25.0), &c, Ablation::F));
    assert!(!level.confirmed(
        bar(40, "102", "103", "102", "103"),
        30.0,
        Some(25.0),
        &c,
        Ablation::F,
    ));

    let mut extreme = level.clone();
    extreme.observe(response, &c, Some(10.0), 1.0);
    assert!(!extreme.confirmed(response, 10.0, Some(25.0), &c, Ablation::F));
    assert!(!extreme.confirmed(response, 30.0, Some(25.0), &c, Ablation::F));

    let mut retested = level.clone();
    retested.observe(response, &c, Some(30.0), 1.0);
    retested.observe(bar(30, "101", "102", "100", "100"), &c, Some(30.0), 1.0);
    assert_eq!(retested.tests, 2);
    assert!(!retested.confirmed(
        bar(35, "102", "103", "102", "103"),
        40.0,
        Some(30.0),
        &c,
        Ablation::F,
    ));

    let mut broken = level.clone();
    broken.observe(bar(25, "100", "100", "97", "98"), &c, Some(30.0), 1.0);
    assert!(!broken.confirmed(response, 30.0, Some(25.0), &c, Ablation::F));
}

#[rstest]
fn test_reclaimed_demand_signal_records_its_distinct_setup_and_timestamps() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.minimum_confirmation_volume = 1.0;
    f.structure.structure = Structure::Bullish;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 25.0;
    f.previous_k = Some(10.0);
    f.vwap.value = 101.0;
    levels.levels[0].observe(bar(15, "102", "103", "97", "98"), &c.slc, Some(10.0), 1.0);
    levels.levels[0].observe(bar(20, "98", "103", "97", "102"), &c.slc, Some(25.0), 1.0);
    levels.levels[0].observe(
        bar(25, "102.0", "102.0", "100.0", "100.5"),
        &c.slc,
        Some(10.0),
        1.0,
    );
    let signal = levels
        .evaluate(
            bar(30, "100.5", "103.0", "100.0", "102.0"),
            &f,
            &signal().momentum,
            MarketRegime::Bullish,
            &c,
            bar(31, "100", "101", "99", "100").ts_event,
        )
        .unwrap();
    assert_eq!(signal.setup_type, "MOMENTUM_SLC_BREAK_RETEST");
    assert_eq!(signal.level_breaks, 1);
    assert_eq!(signal.level_tests, 1);
    assert_eq!(
        signal.level_reclaimed_at,
        Some(bar(20, "98", "103", "97", "102").ts_event)
    );
    assert!(signal.level_reclaimed_at.unwrap() < signal.timestamp);
}

#[rstest]
fn test_metadata_is_point_in_time() {
    let mut c = config();
    c.universe[0].known_at = bar(30, "100", "101", "99", "100").ts_event;
    assert!(
        c.metadata(
            "AAPL.XNAS".into(),
            bar(20, "100", "101", "99", "100").ts_event
        )
        .is_none()
    );
    assert!(
        c.metadata(
            "AAPL.XNAS".into(),
            bar(35, "100", "101", "99", "100").ts_event
        )
        .is_some()
    );
}

#[rstest]
fn test_calendar_handles_dst_without_utc_anchor() {
    let mut c = config();
    let ts = |s: &str| -> UnixNanos { s.parse::<jiff::Timestamp>().unwrap().into() };
    c.sessions = vec![
        Session {
            open: ts("2025-03-07T14:30:00Z"),
            close: ts("2025-03-07T21:00:00Z"),
        },
        Session {
            open: ts("2025-03-10T13:30:00Z"),
            close: ts("2025-03-10T20:00:00Z"),
        },
    ];
    c.validate().unwrap();
    c.sessions[1].open = ts("2025-03-10T14:30:00Z");
    assert!(c.validate().is_err());
}

#[rstest]
fn test_no_future_daily_ranking() {
    let mut c = config();
    c.momentum.minimum_universe_size = 2;
    c.momentum.minimum_atr_fraction = 0.001;
    c.momentum.minimum_average_dollar_volume = rust_decimal_macros::dec!(1000000);
    let second = SymbolMetadata {
        instrument_id: "MSFT.XNAS".into(),
        ..c.universe[0].clone()
    };
    c.universe.push(second);
    let base = session().open.as_u64();
    c.sessions = (0..25)
        .map(|n| Session {
            open: (base + n * 24 * 60 * MINUTE).into(),
            close: (base + n * 24 * 60 * MINUTE + 390 * MINUTE).into(),
        })
        .collect();
    let active = c.sessions[24];
    let cutoff = UnixNanos::from(active.open.as_u64() + 5 * MINUTE);
    let mut ranker = CrossSectionalMomentumRanker::default();
    let mut comparison = CrossSectionalMomentumRanker::default();
    let mut features = std::collections::BTreeMap::new();
    for id in c.instrument_ids() {
        for (n, session) in c.sessions.iter().enumerate().take(24) {
            let price = Price::from(
                format!(
                    "{}.00",
                    100 + n * if id == c.universe[0].instrument_id {
                        2
                    } else {
                        1
                    }
                )
                .as_str(),
            );
            let daily_type = data::bar_type(
                id,
                1,
                nautilus_model::enums::BarAggregation::Day,
                nautilus_model::enums::AggregationSource::External,
            );
            let b = Bar::new(
                daily_type,
                price,
                Price::new(price.as_f64() + 2.0, 2),
                Price::new(price.as_f64() - 2.0, 2),
                price,
                Quantity::from(1_000_000),
                session.close,
                session.close,
            );
            ranker.update_daily(b, cutoff).unwrap();
            comparison.update_daily(b, cutoff).unwrap();
        }
        let mut f = SymbolFeatures::new(&c.slc);
        let previous = ranker.daily[&id].back().unwrap().close;
        for minute in 1..=5 {
            let timestamp = UnixNanos::from(active.open.as_u64() + minute * MINUTE);
            let b = Bar::new(
                minute_bar_type(id),
                previous,
                previous,
                previous,
                previous,
                Quantity::from(10_000),
                timestamp,
                timestamp,
            );
            f.update(b, active, &c.slc);
        }
        f.volume_profiles.extend(std::iter::repeat_n(
            vec![rust_decimal_macros::dec!(50000); 390],
            5,
        ));
        features.insert(id, f);
        let future = Bar::new(
            data::bar_type(
                id,
                1,
                nautilus_model::enums::BarAggregation::Day,
                nautilus_model::enums::AggregationSource::External,
            ),
            Price::from("500"),
            Price::from("501"),
            Price::from("499"),
            Price::from("500"),
            Quantity::from(1_000_000),
            active.close,
            active.close,
        );
        comparison.update_daily(future, active.close).unwrap();
    }
    ranker.refresh(cutoff, &c, &features);
    comparison.refresh(cutoff, &c, &features);
    assert_eq!(ranker.snapshot.ranks.len(), 2);
    assert_eq!(
        serde_json::to_value(ranker.snapshot()).unwrap(),
        serde_json::to_value(comparison.snapshot()).unwrap()
    );
}

#[rstest]
#[case(1.0, MarketRegime::Bullish)]
#[case(-1.0, MarketRegime::Bearish)]
#[case(0.0, MarketRegime::Neutral)]
fn test_market_regime_votes(#[case] slope: f64, #[case] expected: MarketRegime) {
    let mut c = config();
    c.slc.ema_fast = 3;
    c.slc.ema_slow = 5;
    let mut features = std::collections::BTreeMap::new();
    for id in c.benchmarks {
        let mut f = SymbolFeatures::new(&c.slc);
        for minute in 1..=60 {
            let close = Price::new(100.0 + slope * minute as f64 / 10.0, 2);
            let ts = UnixNanos::from(session().open.as_u64() + minute * MINUTE);
            f.update(
                Bar::new(
                    minute_bar_type(id),
                    close,
                    close,
                    close,
                    close,
                    Quantity::from(1000),
                    ts,
                    ts,
                ),
                session(),
                &c.slc,
            );
        }
        features.insert(id, f);
    }
    let ts = UnixNanos::from(session().open.as_u64() + 60 * MINUTE);
    assert_eq!(market_regime(&features, ts, &c), Some(expected));

    features.remove(&c.benchmarks[1]);
    assert_eq!(market_regime(&features, ts, &c), None);
    c.regime_benchmarks = Some(vec![c.benchmarks[0], c.benchmarks[2]]);
    assert!(c.validate().is_ok());
    assert_eq!(market_regime(&features, ts, &c), Some(expected));
    features.remove(&c.benchmarks[2]);
    assert_eq!(market_regime(&features, ts, &c), None);

    c.regime_benchmarks = Some(vec![c.benchmarks[0], c.benchmarks[0]]);
    assert!(c.validate().is_err());
    c.regime_benchmarks = Some(vec![]);
    assert!(c.validate().is_err());
    c.regime_benchmarks = Some(vec!["UNKNOWN.SIM".into(), c.benchmarks[0]]);
    assert!(c.validate().is_err());
    c.regime_benchmarks = Some(vec![c.benchmarks[0]]);
    assert!(c.validate().is_err());
}

#[rstest]
fn test_volume_comparison_excludes_the_current_bar() {
    let mut f = SymbolFeatures::new(&SlcConfig::default());
    for minute in [5, 10, 15, 20, 25] {
        f.record_ltf_volume(bar(minute, "100", "101", "99", "100"));
    }
    let mut current = bar(30, "100", "101", "99", "100");
    current.volume = Quantity::from(2000);
    assert_eq!(f.relative_ltf_volume(current), Some(2.0));
    f.record_ltf_volume(current);
    assert_eq!(f.relative_ltf_volume(current), Some(2.0));
}

#[rstest]
fn test_without_stochastic_does_not_add_a_price_filter() {
    let (mut levels, _, c) = demand();
    let level = &mut levels.levels[0];
    level.observe(bar(15, "101", "104", "100", "101"), &c, None, 1.0);
    assert!(level.confirmed(
        bar(20, "101", "103", "100", "102"),
        50.0,
        Some(40.0),
        &c,
        Ablation::WithoutStochastic
    ));
    assert!(!level.confirmed(
        bar(20, "101", "103", "100", "102"),
        50.0,
        Some(40.0),
        &c,
        Ablation::F
    ));
}

#[rstest]
fn test_atr_target_keeps_configured_minimum_r() {
    let c = ExitConfig {
        target_mode: TargetMode::Atr,
        ..ExitConfig::default()
    };
    let instrument = InstrumentAny::Equity(equity_aapl());
    let target = target_price(
        Price::from("100.00"),
        Price::from("98.00"),
        dec!(0.1),
        &instrument,
        &c,
        None,
        TradeSide::Long,
    )
    .unwrap();
    assert_eq!(target, Price::from("104.00"));
}

#[rstest]
fn test_momentum_only_does_not_require_stochastic_readiness() {
    let (mut levels, mut f, _) = demand();
    f.stochastic.initialized = false;
    let c = SlcMomentumConfig {
        ablation: Ablation::A,
        ..config()
    };
    let result = levels
        .evaluate(
            bar(20, "100", "103", "100", "102"),
            &f,
            &signal().momentum,
            MarketRegime::Bullish,
            &c,
            session().close,
        )
        .unwrap();
    assert_eq!(result.setup_type, "MOMENTUM_ONLY");
}

fn full_market_fixture(
    size: usize,
) -> (
    SlcMomentumConfig,
    CrossSectionalMomentumRanker,
    Vec<MarketObservation>,
) {
    let mut c = config();
    c.market = Some(MarketSelectionConfig {
        universe_size: size,
        max_candidates: size.div_ceil(10),
        ..Default::default()
    });
    c.momentum.min_percentile = 90.0;
    c.momentum.minimum_universe_size = 2;
    c.momentum.minimum_atr_fraction = 0.001;
    c.momentum.minimum_average_dollar_volume = dec!(1000000);
    c.universe = (0..size)
        .map(|i| SymbolMetadata {
            instrument_id: format!("STOCK{i:04}.SIM").parse().unwrap(),
            sector: "technology".into(),
            sector_etf: c.benchmarks[0],
            ..c.universe[0].clone()
        })
        .collect();
    let base = session().open.as_u64();
    c.sessions = (0..25)
        .map(|n| Session {
            open: (base + n * 1440 * MINUTE).into(),
            close: (base + n * 1440 * MINUTE + 390 * MINUTE).into(),
        })
        .collect();
    let current = c.sessions[24];
    c.trading_start = current.open;
    c.trading_end = current.close;
    let timestamp = UnixNanos::from(current.open.as_u64() + 5 * MINUTE);
    let mut ranker = CrossSectionalMomentumRanker::default();
    for (i, id) in c.instrument_ids().iter().enumerate() {
        for (n, s) in c.sessions.iter().take(24).enumerate() {
            let close = dec!(100)
                + rust_decimal::Decimal::new((i + 1) as i64, 4) * rust_decimal::Decimal::from(n);
            let price = |v: rust_decimal::Decimal| Price::from(v.to_string().as_str());
            let bar = Bar::new(
                data::bar_type(
                    *id,
                    1,
                    nautilus_model::enums::BarAggregation::Day,
                    nautilus_model::enums::AggregationSource::External,
                ),
                price(close),
                price(close + dec!(2)),
                price(close - dec!(2)),
                price(close),
                Quantity::from(1_000_000),
                s.close,
                s.close,
            );
            ranker.update_daily(bar, timestamp).unwrap();
        }
    }
    let quotes = c
        .universe
        .iter()
        .map(|m| {
            let last = ranker.daily[&m.instrument_id].back().unwrap().close;
            MarketObservation {
                symbol: m.instrument_id,
                timestamp,
                available_at: timestamp,
                open: last,
                last,
                relative_volume: 1.0,
            }
        })
        .collect();
    (c, ranker, quotes)
}

#[test]
fn test_full_market_3000_selects_global_top_ten_percent_without_minute_features() {
    let (c, mut ranker, quotes) = full_market_fixture(3000);
    c.validate().unwrap();
    let timestamp = quotes[0].timestamp;
    let selected = ranker
        .rank_market(timestamp, c.sessions[24].open, &c, &quotes)
        .unwrap();
    assert_eq!(selected.universe_size, 3000);
    assert_eq!(selected.observed_size, 3000);
    assert_eq!(selected.ranking.ranks.len(), 3000);
    assert_eq!(selected.candidates.len(), 300);
    assert!(
        selected
            .candidates
            .iter()
            .all(|id| selected.ranking.ranks[id].rank <= 300)
    );
    let mut reversed = quotes.clone();
    reversed.reverse();
    assert_eq!(
        selected,
        ranker
            .rank_market(timestamp, c.sessions[24].open, &c, &reversed)
            .unwrap()
    );
    let mut partial = selected.clone();
    partial
        .ranking
        .ranks
        .retain(|id, _| selected.candidates.contains(id));
    assert!(partial.validate(&c, timestamp).is_err());
    let mut changed = c.clone();
    changed.market.as_mut().unwrap().top_fraction = 0.05;
    changed.momentum.min_percentile = 95.0;
    assert_eq!(
        ranker
            .rank_market(timestamp, c.sessions[24].open, &changed, &quotes)
            .unwrap()
            .candidates
            .len(),
        150
    );
}

#[test]
fn test_global_selection_missing_stale_future_quotes_and_expiry_fail_closed() {
    let (c, mut ranker, quotes) = full_market_fixture(100);
    let at = quotes[0].timestamp;
    let open = c.sessions[24].open;
    let selected = ranker.rank_market(at, open, &c, &quotes).unwrap();
    let id = *selected.candidates.first().unwrap();
    assert!(selected.permits(id, at));
    assert!(!selected.permits(id, selected.valid_until));
    assert!(!selected.permits(id, UnixNanos::from(at.as_u64() - 1)));
    let missing = ranker.rank_market(at, open, &c, &quotes[..90]).unwrap();
    assert!(missing.candidates.is_empty());
    assert_eq!(missing.ranking.excluded.len(), 100);
    let mut future = quotes.clone();
    for q in &mut future {
        q.available_at = UnixNanos::from(at.as_u64() + 1);
    }
    assert!(
        ranker
            .rank_market(at, open, &c, &future)
            .unwrap()
            .candidates
            .is_empty()
    );
    let stale = UnixNanos::from(at.as_u64() + 2 * MINUTE);
    assert!(
        ranker
            .rank_market(stale, open, &c, &quotes)
            .unwrap()
            .candidates
            .is_empty()
    );
    let mut duplicate = quotes.clone();
    duplicate.push(quotes[0].clone());
    assert!(ranker.rank_market(at, open, &c, &duplicate).is_err());
    let mut tampered = selected.clone();
    tampered.ranking.ranks.get_mut(&id).unwrap().percentile = 1.0;
    assert!(tampered.validate(&c, at).is_err());
    for id in quotes.iter().take(20).map(|q| q.symbol) {
        ranker.daily.remove(&id);
    }
    let missing_history = ranker.rank_market(at, open, &c, &quotes).unwrap();
    assert_eq!(missing_history.observed_size, 80);
    assert!(missing_history.candidates.is_empty());
}

#[test]
fn test_global_ranking_ignores_current_day_close_and_custom_event_roundtrip() {
    let (c, mut ranker, quotes) = full_market_fixture(100);
    let at = quotes[0].timestamp;
    let session = c.sessions[24];
    let selected = ranker.rank_market(at, session.open, &c, &quotes).unwrap();
    for id in c.instrument_ids() {
        let mut future = *ranker.daily[&id].back().unwrap();
        future.ts_event = session.close;
        future.ts_init = session.close;
        future.close = future.high;
        ranker.update_daily(future, session.close).unwrap();
    }
    assert_eq!(
        selected,
        ranker.rank_market(at, session.open, &c, &quotes).unwrap()
    );
    let event = MarketUpdate {
        timestamp: at,
        selection: Some(selected),
        warmup_symbols: Default::default(),
        bars: vec![],
    };
    let json = serde_json::to_string(&event).unwrap();
    let decoded: MarketUpdate = serde_json::from_str(&json).unwrap();
    let roundtrip = decoded.selection.as_ref().unwrap();
    roundtrip.validate(&c, at).unwrap();
    assert_eq!(event.timestamp, decoded.timestamp);
    assert_eq!(
        event.selection.as_ref().unwrap().candidates,
        roundtrip.candidates
    );
    for (id, rank) in &roundtrip.ranking.ranks {
        let original = &event.selection.as_ref().unwrap().ranking.ranks[id];
        assert_eq!(rank.rank, original.rank);
        assert_eq!(rank.average_dollar_volume, original.average_dollar_volume);
        assert!((rank.score - original.score).abs() < 1e-10);
    }
    assert!(matches!(
        event.into_data(),
        nautilus_model::data::Data::Custom(_)
    ));
}

#[test]
fn test_full_market_uses_effective_membership_not_future_replacements() {
    let (mut c, mut ranker, quotes) = full_market_fixture(100);
    let at = quotes[0].timestamp;
    let replacement_time = UnixNanos::from(at.as_u64() + 30 * MINUTE);
    let old = c.universe[0].instrument_id;
    let new = InstrumentId::from("NEW.SIM");
    c.universe[0].effective_until = replacement_time;
    let mut replacement = c.universe[0].clone();
    replacement.instrument_id = new;
    replacement.effective_from = replacement_time;
    replacement.effective_until = u64::MAX.into();
    c.universe.push(replacement);
    c.validate().unwrap();
    let first = ranker
        .rank_market(at, c.sessions[24].open, &c, &quotes)
        .unwrap();
    assert!(first.ranking.ranks.contains_key(&old));
    assert!(!first.ranking.ranks.contains_key(&new));
    let mut next = quotes.clone();
    for q in &mut next {
        q.timestamp = replacement_time;
        q.available_at = replacement_time;
        if q.symbol == old {
            q.symbol = new;
        }
    }
    let second = ranker
        .rank_market(replacement_time, c.sessions[24].open, &c, &next)
        .unwrap();
    assert!(!second.ranking.ranks.contains_key(&old));
    assert_eq!(second.ranking.excluded.get(&new), Some(&NoTrade::Warmup));
    assert_eq!(
        second.ranking.ranks.len() + second.ranking.excluded.len(),
        100
    );
}

#[test]
fn test_global_mode_rejects_a_missing_intraday_minute_before_regime_or_slc() {
    let mut c = config();
    c.market = Some(MarketSelectionConfig::default());
    c.slc.ema_slow = 5;
    c.slc.ema_fast = 3;
    let mut features = std::collections::BTreeMap::new();
    for id in c.benchmarks {
        let mut f = SymbolFeatures::new(&c.slc);
        for minute in 1..=30 {
            let mut b = bar(minute, "100", "101", "99", "100");
            b.bar_type = minute_bar_type(id);
            f.update(b, session(), &c.slc);
        }
        features.insert(id, f);
    }
    let at = bar(30, "100", "101", "99", "100").ts_event;
    assert_eq!(
        market_regime(&features, at, &c),
        Some(MarketRegime::Neutral)
    );
    let mut incomplete = SymbolFeatures::new(&c.slc);
    for minute in 1..=30 {
        if minute != 17 {
            incomplete.update(bar(minute, "100", "101", "99", "100"), session(), &c.slc);
        }
    }
    assert!(!incomplete.complete_at(at));
    let mut levels = KeyLevelDetector::default();
    assert_eq!(
        levels
            .evaluate(
                bar(30, "100", "101", "99", "100"),
                &incomplete,
                &signal().momentum,
                MarketRegime::Bullish,
                &c,
                at
            )
            .unwrap_err(),
        NoTrade::DataMissing
    );
    features.insert(c.benchmarks[0], incomplete);
    assert_eq!(market_regime(&features, at, &c), None);
}

#[rstest]
fn test_short_signal_requires_supply_bearish_structure_and_intraday_downtrend() {
    use nautilus_indicators::indicator::MovingAverage;
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    c.slc.require_intraday_trend = true;
    c.slc.minimum_confirmation_volume = 1.0;
    let zone = &mut levels.levels[0];
    zone.kind = LevelKind::Supply;
    zone.observe(bar(15, "99", "100", "98", "100"), &c.slc, Some(90.0), 1.0);
    f.structure.structure = Structure::Bearish;
    f.stochastic.initialized = true;
    f.stochastic.value_k = 75.0;
    f.stochastic.value_d = 85.0;
    f.previous_k = Some(90.0);
    f.vwap.value = 99.0;
    f.open = Some(Price::from("105"));
    for n in 0..60 {
        f.ema_fast.update_raw(110.0 - f64::from(n) * 0.15);
        f.ema_slow.update_raw(110.0 - f64::from(n) * 0.15);
    }
    let mut rank = signal().momentum;
    rank.percentile = 4.0;
    let candle = bar(20, "100", "100", "97", "98");
    assert_eq!(
        levels
            .evaluate(
                candle,
                &f,
                &rank,
                MarketRegime::Bullish,
                &c,
                session().close
            )
            .unwrap_err(),
        NoTrade::MarketBullish
    );
    f.open = Some(Price::from("95"));
    assert_eq!(
        levels
            .evaluate(
                candle,
                &f,
                &rank,
                MarketRegime::Bearish,
                &c,
                session().close
            )
            .unwrap_err(),
        NoTrade::IntradayTrendMismatch
    );
    f.open = Some(Price::from("105"));
    let signal = levels
        .evaluate(
            candle,
            &f,
            &rank,
            MarketRegime::Bearish,
            &c,
            session().close,
        )
        .unwrap();
    assert_eq!(signal.side, TradeSide::Short);
    assert_eq!(signal.level_type, "SUPPLY");
    assert!(signal.intraday_return < 0.0);
    assert!(!levels.levels[0].available());
}

#[rstest]
fn test_short_risk_targets_trailing_and_shared_gross_limits() {
    let mut c = config();
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    let instrument = InstrumentAny::Equity(equity_aapl());
    let mut s = signal();
    s.side = TradeSide::Short;
    s.entry_price = Price::from("100.00");
    s.level_high = Price::from("102.00");
    s.atr = dec!(1.03);
    let mut risk = PortfolioRiskManager::default();
    let allocation = risk
        .allocate(
            &s,
            &instrument,
            s.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None,
        )
        .unwrap();
    assert_eq!(allocation.stop, Price::from("102.21"));
    assert_eq!(allocation.target, Price::from("95.58"));
    assert!(allocation.reserved_risk <= dec!(500));
    assert!(allocation.notional * c.risk.short_margin_ratio <= c.risk.max_position_value);
    let lowered = trailing_stop(
        allocation.stop,
        Price::from("97.00"),
        Price::from("94.00"),
        dec!(1),
        &instrument,
        &c.exit,
        TradeSide::Short,
    )
    .unwrap();
    assert_eq!(lowered, Price::from("97.00"));
    assert_eq!(
        trailing_stop(
            lowered,
            Price::from("101.00"),
            Price::from("100.00"),
            dec!(1),
            &instrument,
            &c.exit,
            TradeSide::Short
        )
        .unwrap(),
        lowered
    );
    risk.reserve(s.symbol, s.sector.clone(), &allocation)
        .unwrap();
    // A different side cannot bypass the same-symbol reservation or gross sector limits.
    s.side = TradeSide::Long;
    assert_eq!(
        risk.allocate(
            &s,
            &instrument,
            s.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None
        )
        .unwrap_err(),
        NoTrade::DuplicateOrder
    );
    c.risk.max_sector_positions = 1;
    s.symbol = "OTHER.XNAS".into();
    assert_eq!(
        risk.allocate(
            &s,
            &instrument,
            s.entry_price,
            Money::from("100000 USD"),
            dec!(100000),
            0,
            &c,
            dec!(100000),
            None
        )
        .unwrap_err(),
        NoTrade::MaxExposure
    );
}

#[rstest]
fn test_global_long_short_tails_and_side_specific_rotation() {
    let (mut c, mut ranker, quotes) = full_market_fixture(3000);
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    c.market.as_mut().unwrap().max_candidates = 600;
    c.validate().unwrap();
    let at = quotes[0].timestamp;
    let selected = ranker
        .rank_market(at, c.sessions[24].open, &c, &quotes)
        .unwrap();
    assert_eq!(selected.candidates.len(), 600);
    let long = selected
        .ranking
        .ranks
        .iter()
        .find(|(_, r)| r.rank == 1)
        .unwrap()
        .0;
    let short = selected
        .ranking
        .ranks
        .iter()
        .find(|(_, r)| r.rank == 3000)
        .unwrap()
        .0;
    assert!(selected.permits_side(*long, at, TradeSide::Long, &c));
    assert!(!selected.permits_side(*long, at, TradeSide::Short, &c));
    assert!(selected.permits_side(*short, at, TradeSide::Short, &c));
    assert!(!selected.permits_side(*short, at, TradeSide::Long, &c));
    c.directions = vec![TradeSide::Long];
    assert!(!selected.permits_side(*short, at, TradeSide::Short, &c));
}

#[rstest]
fn test_weighted_percentile_mean_stays_within_global_contract() {
    let (mut c, mut ranker, mut quotes) = full_market_fixture(100);
    c.momentum.return_weights = vec![0.0, 1.0, 1.0, 1.0];
    c.momentum.spy_weight = 1.0;
    c.momentum.qqq_weight = 1.0;
    c.momentum.sector_weight = 1.0;
    c.momentum.relative_volume_weight = 1.0;
    c.momentum.intraday_weight = 0.0;
    let at = quotes[0].timestamp;
    let initial = ranker
        .rank_market(at, c.sessions[24].open, &c, &quotes)
        .unwrap();
    let best = *initial
        .ranking
        .ranks
        .iter()
        .find(|(_, r)| r.rank == 1)
        .unwrap()
        .0;
    quotes
        .iter_mut()
        .find(|q| q.symbol == best)
        .unwrap()
        .relative_volume = 2.0;
    let selection = ranker
        .rank_market(at, c.sessions[24].open, &c, &quotes)
        .unwrap();
    assert_eq!(selection.ranking.ranks[&best].score, 100.0);
    selection.validate(&c, at).unwrap();
}

#[rstest]
fn test_direction_and_margin_configuration_rejects_unsafe_values() {
    let mut c = config();
    c.directions = vec![TradeSide::Long, TradeSide::Long];
    assert!(c.validate().is_err());
    c.directions = vec![TradeSide::Long, TradeSide::Short];
    c.risk.short_margin_ratio = dec!(0.5);
    assert!(c.validate().is_err());
    c.risk.short_margin_ratio = dec!(1000000000);
    assert!(c.validate().is_err());
    c.risk.short_margin_ratio = dec!(1.5);
    c.slc.minimum_intraday_return = f64::NAN;
    assert!(c.validate().is_err());
}

#[rstest]
#[case(false)]
#[case(true)]
fn test_sweep_reclaim_is_symmetric_causal_and_expires(#[case] short: bool) {
    let c = SlcConfig {
        structure_mode: StructureMode::SweepReclaim,
        require_htf_ema: false,
        ..Default::default()
    };
    let candle = |minute, o, h, l, cl| {
        let b = bar(minute, o, h, l, cl);
        if !short {
            return b;
        }
        let mirror = |p: Price| Price::from_decimal(dec!(200) - p.as_decimal()).unwrap();
        Bar::new(
            b.bar_type,
            mirror(b.open),
            mirror(b.low),
            mirror(b.high),
            mirror(b.close),
            b.volume,
            b.ts_event,
            b.ts_init,
        )
    };
    let mut detector = HTFStructureDetector::new(&c);
    detector.update(candle(30, "100", "104", "98", "101"));
    assert_eq!(detector.structure, Structure::Range);
    let c2 = candle(60, "99", "103", "97", "100");
    detector.update(c2);
    let expected = if short {
        Structure::Bearish
    } else {
        Structure::Bullish
    };
    assert_eq!(detector.structure, expected);
    assert_eq!(detector.confirmed_at, Some(c2.ts_event));
    detector.update(c2);
    assert_eq!(detector.structure, expected);
    detector.update(candle(90, "100", "105", "99", "105"));
    assert_eq!(detector.structure, expected);
    detector.update(candle(120, "105", "106", "100", "106"));
    assert_eq!(detector.structure, Structure::Range);
}

#[rstest]
fn test_double_sweep_and_observed_failure_do_not_rewrite_past_confirmation() {
    let mut c = SlcConfig {
        structure_mode: StructureMode::SweepReclaim,
        require_htf_ema: false,
        ..Default::default()
    };
    let mut d = HTFStructureDetector::new(&c);
    d.update(bar(30, "100", "104", "98", "101"));
    d.update(bar(60, "99", "105", "97", "100"));
    assert_eq!(d.structure, Structure::Range);
    d.reset_session();
    d.update(bar(90, "100", "104", "98", "101"));
    d.update(bar(120, "99", "103", "97", "100"));
    let known_at = d.confirmed_at;
    d.invalidate_sweep(bar(121, "100", "101", "96", "100"));
    assert_eq!(d.structure, Structure::Range);
    assert_eq!(d.confirmed_at, known_at);
    d.reset_session();
    assert_eq!(d.confirmed_at, None);
    c.require_htf_ema = true;
    let mut d = HTFStructureDetector::new(&c);
    d.update(bar(30, "100", "104", "98", "101"));
    d.update(bar(60, "99", "103", "97", "100"));
    assert_eq!(d.structure, Structure::Range);
}

#[rstest]
fn test_cisd_uses_completed_crossing_and_run_open_in_both_directions() {
    let mut d = DeliveryConfirmation::default();
    for b in [
        bar(5, "104", "105", "101", "102"),
        bar(10, "102", "103", "99", "100"),
        bar(15, "100", "105", "99", "104"),
    ] {
        d.update(b);
    }
    assert_eq!(d.event, None);
    let crossed = bar(20, "104", "106", "103", "105");
    d.update(crossed);
    assert_eq!(
        d.event,
        Some((crossed.ts_event, Structure::Bullish, Price::from("104")))
    );
    d.update(crossed);
    let down = bar(25, "105", "106", "98", "99");
    d.update(down);
    assert_eq!(
        d.event,
        Some((down.ts_event, Structure::Bearish, Price::from("100")))
    );
}

#[rstest]
fn test_cisd_before_touch_cannot_confirm_a_later_pullback() {
    let (mut levels, mut f, _) = demand();
    let mut c = config();
    c.slc.confirmation_mode = ConfirmationMode::Cisd;
    c.slc.minimum_confirmation_volume = 1.0;
    f.structure.structure = Structure::Bullish;
    f.vwap.value = 101.0;
    levels.levels[0].observe(bar(15, "101", "102", "100", "100"), &c.slc, None, 1.0);
    let response = bar(20, "100", "103", "100", "102");
    f.delivery.event = Some((
        bar(10, "101", "102", "100", "101").ts_event,
        Structure::Bullish,
        Price::from("101"),
    ));
    assert_eq!(
        levels
            .evaluate(
                response,
                &f,
                &signal().momentum,
                MarketRegime::Bullish,
                &c,
                response.ts_event
            )
            .unwrap_err(),
        NoTrade::ConfirmationMissing
    );
    f.delivery.event = Some((response.ts_event, Structure::Bullish, Price::from("101")));
    let s = levels
        .evaluate(
            response,
            &f,
            &signal().momentum,
            MarketRegime::Bullish,
            &c,
            response.ts_event,
        )
        .unwrap();
    assert_eq!(s.cisd_confirmed_at, Some(response.ts_event));
    assert_eq!(s.cisd_level, Some(Price::from("101")));
    assert_eq!(s.confirmation_enabled & 8, 0);
}

#[rstest]
fn test_zero_mfe_has_no_profit_capture_ratio() {
    assert_eq!(profit_capture_ratio(dec!(0), dec!(-10)), None);
    assert_eq!(profit_capture_ratio(dec!(20), dec!(5)), Some(dec!(0.25)));
}

#[rstest]
fn test_htf_sweep_waits_for_full_bucket_and_resets_after_missing_bucket() {
    let c = SlcConfig {
        htf_minutes: 30,
        structure_mode: StructureMode::SweepReclaim,
        require_htf_ema: false,
        ..Default::default()
    };
    let mut f = SymbolFeatures::new(&c);
    for minute in 1..=30 {
        f.update(bar(minute, "100", "104", "98", "101"), session(), &c);
    }
    for minute in 31..60 {
        f.update(bar(minute, "99", "103", "97", "100"), session(), &c);
    }
    assert_eq!(f.structure.structure, Structure::Range);
    let last = bar(60, "99", "103", "97", "100");
    f.update(last, session(), &c);
    assert_eq!(f.structure.structure, Structure::Bullish);
    assert_eq!(f.structure.confirmed_at, Some(last.ts_event));
    // Missing minute 61 makes the next HTF bucket unusable
    for minute in 62..=120 {
        f.update(bar(minute, "100", "105", "99", "105"), session(), &c);
    }
    assert_eq!(f.structure.structure, Structure::Range);
    assert_eq!(f.structure.confirmed_at, None);
}
