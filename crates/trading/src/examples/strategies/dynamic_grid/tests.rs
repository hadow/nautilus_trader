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

use nautilus_common::actor::DataActor;
use rstest::rstest;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::{
    config::{GridConfig, PositionSizing},
    engine::{GridEngine, spacing},
    orders::{OrderManager, OrderPhase, PositionComponent},
    regime::{MarketRegime, Observation, RegimeDetector},
    risk::{RiskManager, RiskSnapshot},
};

#[rstest]
fn default_regime_waits_for_all_twenty_seven_completed_bars() {
    let config = GridConfig::default();
    let mut detector = RegimeDetector::default();
    for count in 1..=27 {
        detector
            .update(
                &config,
                Observation {
                    ts_ns: count * 60_000_000_000,
                    high: 100.1,
                    low: 99.9,
                    close: 100.0,
                },
            )
            .unwrap();
        assert_eq!(detector.snapshot.initialized, count == 27);
        assert_eq!(detector.permits_order(&config, true, -1), count == 27);
    }
    assert_eq!(detector.snapshot.regime, MarketRegime::Range);
}

#[rstest]
#[case::current(1, 1, 180, true)]
#[case::exact_limit(1, 180_000_000_001, 180, true)]
#[case::one_nanosecond_expired(1, 180_000_000_002, 180, false)]
#[case::future_timestamp(2, 1, 180, false)]
#[case::large_duration(0, u64::MAX, u64::MAX, true)]
fn market_freshness_preserves_nanosecond_precision(
    #[case] timestamp: u64,
    #[case] now: u64,
    #[case] max_age: u64,
    #[case] expected: bool,
) {
    assert_eq!(super::regime::is_fresh(timestamp, now, max_age), expected);
}

#[rstest]
fn portfolio_trend_metric_uses_only_trending_sleeves() {
    use super::analytics::{EquityPoint, PerformanceTracker};
    let point: EquityPoint = serde_json::from_value(serde_json::json!({
        "ts_ns": 0, "price": "1", "equity": "1000", "exposure": "600",
        "position": "0", "utilization": 0.6, "regime": "Disabled",
        "trend_inventory": "200"
    }))
    .unwrap();
    let mut end = point.clone();
    end.ts_ns = 60_000_000_000;
    let mut report = PerformanceTracker::default();
    report.equity.extend([point, end]);
    report.finish(
        dec!(1000),
        &OrderManager::new(dec!(1000)),
        &RiskManager::new(dec!(1000)),
    );
    assert!((report.metrics.trend_exposure - 0.2).abs() < 1e-12);
    assert!((report.metrics.inventory_exposure - 0.6).abs() < 1e-12);
}

#[rstest]
fn daily_reset_budget_survives_restart_and_unknown_legacy_counts_fail_closed() {
    let mut raw = serde_json::to_value(GridConfig::default()).unwrap();
    raw["maximum_resets_per_day"] = 2.into();
    let c: GridConfig = serde_json::from_value(raw).unwrap();
    let mut risk = RiskManager::new(c.capital);
    risk.observe(&c, &capacity(), 1);
    risk.record_reset();
    risk.record_reset();
    assert_eq!(risk.reset_limit(&c), Some("Maximum resets per UTC day"));
    let mut recovered: RiskManager =
        serde_json::from_slice(&serde_json::to_vec(&risk).unwrap()).unwrap();
    assert_eq!(recovered.reset_limit(&c), risk.reset_limit(&c));
    recovered.observe(&c, &capacity(), 86_400_000_000_001);
    assert_eq!(recovered.reset_limit(&c), None);
    assert_eq!(recovered.total_resets, 2);
    let mut legacy = serde_json::to_value(&risk).unwrap();
    legacy.as_object_mut().unwrap().remove("daily_resets");
    let mut legacy: RiskManager = serde_json::from_value(legacy).unwrap();
    legacy.observe(&c, &capacity(), 2);
    assert_eq!(legacy.reset_limit(&c), Some("Maximum resets per UTC day"));
}

#[rstest]
fn reset_requires_atr_overshoot_on_both_boundaries() {
    let mut raw = serde_json::to_value(GridConfig::default()).unwrap();
    raw["grid_levels"] = 1.into();
    raw["minimum_reset_atr_multiple"] = "0.5".into();
    raw["minimum_reset_interval_secs"] = 0.into();
    let c: GridConfig = serde_json::from_value(raw).unwrap();
    let g = grid(&c);
    assert!(!g.can_reset(&c, dec!(102.01), dec!(1), 1));
    assert!(!g.can_reset(&c, dec!(97.99), dec!(1), 1));
    assert!(g.can_reset(&c, dec!(102.5), dec!(1), 1));
    assert!(g.can_reset(&c, dec!(97.5), dec!(1), 1));
}

#[rstest]
fn ema_regime_requires_configured_price_confirmation_and_recovers_causally() {
    let mut raw = serde_json::to_value(GridConfig::default()).unwrap();
    raw["regime_average"] = "Exponential".into();
    raw["require_price_ma_confirmation"] = true.into();
    raw["price_ma_confirmation_pct"] = 0.0001.into();
    for key in [
        "atr_period",
        "adx_period",
        "ma_period",
        "slope_period",
        "volatility_period",
    ] {
        raw[key] = 3.into();
    }
    let config: GridConfig = serde_json::from_value(raw.clone()).unwrap();
    let mut detector = RegimeDetector::default();
    for i in 0..12 {
        let price = 100.0 + f64::from(i);
        detector
            .update(
                &config,
                Observation {
                    ts_ns: i as u64 + 1,
                    high: price + 0.1,
                    low: price - 0.1,
                    close: price,
                },
            )
            .unwrap();
        if i < 3 {
            assert!(!detector.snapshot.initialized);
        }
    }
    assert_eq!(detector.snapshot.regime, MarketRegime::TrendUp);
    assert!((detector.snapshot.moving_average - 110.000_488_281_25).abs() < 1e-10);
    let mut restored: RegimeDetector =
        serde_json::from_slice(&serde_json::to_vec(&detector).unwrap()).unwrap();
    let next = Observation {
        ts_ns: 13,
        high: 112.1,
        low: 111.9,
        close: 112.0,
    };
    detector.update(&config, next).unwrap();
    restored.update(&config, next).unwrap();
    assert_eq!(
        serde_json::to_value(&detector).unwrap(),
        serde_json::to_value(&restored).unwrap()
    );

    raw["price_ma_confirmation_pct"] = 0.5.into();
    let stricter: GridConfig = serde_json::from_value(raw).unwrap();
    detector
        .update(
            &stricter,
            Observation {
                ts_ns: 14,
                high: 113.1,
                low: 112.9,
                close: 113.0,
            },
        )
        .unwrap();
    assert!(detector.snapshot.adx >= stricter.adx_trend_min);
    assert_eq!(detector.snapshot.regime, MarketRegime::Disabled);
}

#[rstest]
fn low_volatility_is_distinct_from_warmup_and_blocks_entries() {
    let c = GridConfig {
        atr_pct_min: 0.001,
        ..Default::default()
    };
    let mut detector = RegimeDetector::default();
    for i in 1..=60 {
        detector
            .update(
                &c,
                Observation {
                    ts_ns: i,
                    high: 100.001,
                    low: 99.999,
                    close: 100.0,
                },
            )
            .unwrap();
    }
    assert!(detector.snapshot.initialized);
    assert_eq!(
        serde_json::to_value(detector.snapshot.regime).unwrap(),
        "LowVolatility"
    );
    assert!(!detector.permits_order(&c, true, -1));
    assert!(detector.permits_order(&c, false, -1));
}

#[rstest]
fn geometric_level_limit_is_enforced() {
    let c = GridConfig {
        grid_levels: 2,
        max_grid_levels: 2,
        ..Default::default()
    };
    c.validate().unwrap();
    assert_eq!(grid(&c).levels.len(), 4);
}

#[rstest]
fn overnight_gap_is_included_in_daily_loss() {
    let c = GridConfig::default();
    let mut r = RiskManager::new(c.capital);
    r.observe(&c, &capacity(), 1);
    let mut s = capacity();
    s.equity = dec!(96000);
    r.observe(&c, &s, 86_400_000_000_001);
    assert_eq!(r.risk_off_reason.as_deref(), Some("Maximum daily loss"));
}

#[rstest]
#[case("0.00", "100.00")]
#[case("101.00", "100.00")]
#[case("0.00", "0.00")]
fn invalid_equity_quotes_are_ignored_before_trading(#[case] bid: &str, #[case] ask: &str) {
    use nautilus_model::{
        data::QuoteTick,
        identifiers::InstrumentId,
        types::{Price, Quantity},
    };
    let id = InstrumentId::from("AAPL.SIM");
    let mut config =
        super::DynamicGridConfig::new(id, "AAPL.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap());
    config.tick_execution = true;
    let mut strategy = super::DynamicGridStrategy::new(config).unwrap();
    let quote = QuoteTick::new(
        id,
        Price::from(bid),
        Price::from(ask),
        Quantity::from(1),
        Quantity::from(1),
        0.into(),
        0.into(),
    );
    strategy.on_quote(&quote).unwrap();
    assert_eq!(strategy.state(), super::StrategyState::Initializing);
}

#[rstest]
fn resting_limit_reservation_does_not_inflate_with_unreachable_higher_marks() {
    let c = GridConfig::default();
    let g = grid(&c);
    let mut orders = OrderManager::new(c.capital);
    let order = orders
        .entry("901", 1, &g.levels[0], dec!(10), None, 0)
        .unwrap();
    orders.transition(&order.id, OrderPhase::Accepted, 1);
    assert_eq!(
        orders.buy_reservations(&c, dec!(100)),
        orders.buy_reservations(&c, dec!(150))
    );
    orders.transition(&order.id, OrderPhase::CancelPending, 2);
    assert!(orders.buy_reservations(&c, dec!(150)).1 >= order.quantity * order.limit.unwrap());
}

#[rstest]
fn trend_policy_is_identical_for_resting_orders_and_new_intents() {
    use super::config::TrendPolicy;
    let mut c = GridConfig::default();
    let mut detector = RegimeDetector::default();
    detector.snapshot.regime = MarketRegime::TrendUp;
    assert!(detector.permits_order(&c, true, -10));
    assert!(!detector.permits_order(&c, false, -10));
    assert!(detector.permits_order(&c, false, -1));
    c.trend_up_policy = TrendPolicy::LongOnly;
    assert!(!detector.permits_order(&c, false, -1));
    detector.snapshot.regime = MarketRegime::TrendDown;
    assert!(!detector.permits_order(&c, true, -1));
    assert!(detector.permits_order(&c, false, -10));
    c.trend_down_policy = TrendPolicy::ReduceGrid;
    assert!(detector.permits_order(&c, true, -1));
    assert!(!detector.permits_order(&c, true, -10));
}

#[rstest]
fn intrabar_drawdown_is_retained_without_storing_every_tick() {
    let mut report = super::analytics::PerformanceTracker::default();
    report.observe_mark(dec!(100), dec!(100), dec!(20), dec!(2), 1_000_000_000);
    report.observe_mark(dec!(100), dec!(90), dec!(10), dec!(2), 2_000_000_000);
    report.observe_mark(dec!(100), dec!(100), dec!(20), dec!(2), 3_000_000_000);
    assert!((report.metrics.max_drawdown - 0.1).abs() < f64::EPSILON);
    assert_eq!(report.metrics.drawdown_duration_secs, 1.0);
    assert!(report.equity.is_empty());
    let restored: super::analytics::PerformanceTracker =
        serde_json::from_slice(&serde_json::to_vec(&report).unwrap()).unwrap();
    assert_eq!(restored.metrics.maximum_position, dec!(2));
}

#[rstest]
fn upper_inventory_seed_and_sell_buy_cycle() {
    let c = GridConfig {
        initial_inventory_fraction: dec!(0.5),
        ..Default::default()
    };
    let g = grid(&c);
    let mut m = OrderManager::new(c.capital);
    let buy = m
        .entry("902", 1, &g.levels[1], dec!(5), Some(dec!(100)), 0)
        .unwrap();
    m.fill(&buy.id, "seed", dec!(5), dec!(100.05), dec!(0.5), false, 1)
        .unwrap();
    let sell = m.exit("902", &buy.id, None, 2).unwrap();
    m.fill(&sell.id, "sell", dec!(5), dec!(102), dec!(0.5), false, 3)
        .unwrap();
    assert_eq!(m.cycles[0].gross_pnl, dec!(10));
    assert_eq!(m.cycles[0].slippage, dec!(0.25));
    assert_eq!(m.cycles[0].net_pnl, dec!(8.75));
    let next = m.entry("902", 1, &g.levels[1], dec!(5), None, 4).unwrap();
    assert_eq!(next.limit, Some(dec!(100)));
}

#[rstest]
fn partial_fills_covered_exits_cycle_and_reentry() {
    let c = GridConfig::default();
    let g = grid(&c);
    let mut m = OrderManager::new(c.capital);
    let entry = m.entry("001", 1, &g.levels[0], dec!(10), None, 0).unwrap();
    assert!(m.entry("001", 1, &g.levels[0], dec!(10), None, 0).is_err());
    m.fill(&entry.id, "f1", dec!(4), dec!(98), dec!(0.4), false, 1)
        .unwrap();
    assert!(
        !m.fill(&entry.id, "f1", dec!(4), dec!(98), dec!(0.4), false, 1)
            .unwrap()
    );
    let sell = m.exit("001", &entry.id, None, 2).unwrap();
    assert_eq!(sell.quantity, dec!(4));
    assert!(m.exit("001", &entry.id, None, 2).is_err());
    m.fill(&sell.id, "s1", dec!(4), dec!(100), dec!(0.4), false, 3)
        .unwrap();
    assert!(m.cycles.is_empty());
    m.fill(&entry.id, "f2", dec!(6), dec!(98), dec!(0.6), false, 4)
        .unwrap();
    let sell = m.exit("001", &entry.id, None, 5).unwrap();
    m.fill(&sell.id, "s2", dec!(6), dec!(100), dec!(0.6), false, 6)
        .unwrap();
    assert_eq!(m.inventory(), Decimal::ZERO);
    assert_eq!(m.cycles.len(), 1);
    assert_eq!(m.cycles[0].net_pnl, dec!(18));
    assert_eq!(m.cash, c.capital + dec!(18));
    assert_eq!(
        m.cycles[0].gross_pnl - m.cycles[0].slippage - m.cycles[0].fees,
        dec!(18)
    );
    let next = m.entry("001", 1, &g.levels[0], dec!(10), None, 7).unwrap();
    assert_ne!(entry.id, next.id);
}

#[rstest]
fn cancel_failure_retains_reservations_and_late_fill() {
    let c = GridConfig::default();
    let g = grid(&c);
    let mut m = OrderManager::new(c.capital);
    let entry = m.entry("001", 1, &g.levels[0], dec!(10), None, 0).unwrap();
    m.transition(&entry.id, OrderPhase::CancelPending, 1);
    m.transition(&entry.id, OrderPhase::Accepted, 2);
    assert_eq!(m.orders()[&entry.id].phase, OrderPhase::CancelPending);
    m.transition(&entry.id, OrderPhase::Unknown, 3);
    assert_eq!(m.buy_reservations(&c, dec!(100)).0, dec!(10));
    assert!(m.timed_out(c.order_timeout_secs, 40_000_000_000));
    m.fill(&entry.id, "late", dec!(2), dec!(98), dec!(0.2), true, 4)
        .unwrap();
    m.transition(&entry.id, OrderPhase::Cancelled, 5);
    assert_eq!(m.inventory(), dec!(2));
    assert_eq!(m.buy_reservations(&c, dec!(100)).0, dec!(0));
    assert_eq!(m.estimated_fee_fills, 1);
}

#[rstest]
fn uncertain_sell_reserves_inventory_until_terminal_cancellation() {
    let config = GridConfig::default();
    let grid = grid(&config);
    let mut ledger = OrderManager::new(config.capital);
    let entry = ledger
        .entry("sell-reservation", 1, &grid.levels[0], dec!(10), None, 0)
        .unwrap();
    ledger
        .fill(&entry.id, "buy", dec!(10), dec!(98), dec!(1), false, 1)
        .unwrap();
    let exit = ledger.exit("sell-reservation", &entry.id, None, 2).unwrap();
    ledger
        .fill(
            &exit.id,
            "partial-sell",
            dec!(4),
            dec!(100),
            dec!(0.4),
            false,
            3,
        )
        .unwrap();

    for (phase, now) in [(OrderPhase::CancelPending, 4), (OrderPhase::Unknown, 5)] {
        ledger.transition(&exit.id, phase, now);
        assert!(ledger.exit_candidates().is_empty());
        assert!(
            ledger
                .exit("sell-reservation", &entry.id, None, now)
                .is_err()
        );
    }

    ledger.transition(&exit.id, OrderPhase::Cancelled, 6);
    assert_eq!(ledger.exit_candidates(), vec![entry.id.clone()]);
    assert_eq!(
        ledger
            .exit("sell-reservation", &entry.id, None, 7)
            .unwrap()
            .quantity,
        dec!(6)
    );
}

#[rstest]
fn rejection_recovery_and_fill_validation() {
    let c = GridConfig::default();
    let g = grid(&c);
    let mut m = OrderManager::new(c.capital);
    let entry = m.entry("001", 1, &g.levels[0], dec!(10), None, 0).unwrap();
    m.transition(&entry.id, OrderPhase::Rejected, 1);
    assert!(!m.slot_busy(1, -1));
    let next = m.entry("001", 1, &g.levels[0], dec!(10), None, 2).unwrap();
    m.fill(&next.id, "f1", dec!(3), dec!(98), dec!(0.3), false, 3)
        .unwrap();
    let mut recovered: OrderManager =
        serde_json::from_slice(&serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(
        recovered
            .entry("001", 1, &g.levels[0], dec!(10), None, 4)
            .is_err()
    );
    assert!(
        !recovered
            .fill(&next.id, "f1", dec!(3), dec!(98), dec!(0.3), false, 3)
            .unwrap()
    );
    assert!(
        recovered
            .fill(&next.id, "bad", dec!(8), dec!(98), dec!(0), false, 5)
            .is_err()
    );
    assert_eq!(recovered.inventory(), dec!(3));
    assert_eq!(recovered.cash, m.cash);
    recovered.validate().unwrap();
    let mut broken = serde_json::to_value(&recovered).unwrap();
    broken["lots"].as_object_mut().unwrap().remove(&next.id);
    let recovered: OrderManager = serde_json::from_value(broken).unwrap();
    assert!(
        recovered.validate().is_err(),
        "valid JSON with a broken ledger must fail closed"
    );
}

#[rstest]
fn live_ledger_reads_match_audit_history_after_every_mutation() {
    let config = GridConfig::default();
    let grid = grid(&config);
    let mut ledger = OrderManager::new(config.capital);
    let check = |ledger: &OrderManager| {
        let active: Vec<_> = ledger
            .orders()
            .values()
            .filter(|o| !o.phase.terminal())
            .collect();
        assert_eq!(
            ledger.active_ids(),
            active.iter().map(|o| o.id.clone()).collect::<Vec<_>>()
        );
        let inventory: Decimal = ledger.lots().values().map(|l| l.bought - l.sold).sum();
        let cost: Decimal = ledger.lots().values().map(|l| l.remaining_cost).sum();
        assert_eq!(ledger.inventory(), inventory);
        assert_eq!(ledger.inventory_cost(), cost);
        let quantity: Decimal = active
            .iter()
            .filter(|o| o.buy)
            .map(|o| o.quantity - o.filled)
            .sum();
        assert_eq!(ledger.buy_reservations(&config, dec!(100)).0, quantity);
        for level in &grid.levels {
            let busy = active
                .iter()
                .any(|o| o.grid_id == 1 && o.level == level.level_index)
                || ledger
                    .lots()
                    .values()
                    .any(|l| l.grid_id == 1 && l.level == level.level_index && l.bought > l.sold);
            assert_eq!(ledger.slot_busy(1, level.level_index), busy);
        }
        ledger.validate().unwrap();
    };
    check(&ledger);
    let entry = ledger
        .entry("cache", 1, &grid.levels[0], dec!(10), None, 0)
        .unwrap();
    check(&ledger);
    ledger.resize_intent(&entry.id, dec!(8)).unwrap();
    check(&ledger);
    ledger.transition(&entry.id, OrderPhase::Submitted, 1);
    assert!(ledger.resize_intent(&entry.id, dec!(7)).is_err());
    assert!(ledger.timed_out(1, 2_000_000_000));
    check(&ledger);
    ledger
        .fill(&entry.id, "partial", dec!(2), dec!(98), dec!(0.2), false, 2)
        .unwrap();
    check(&ledger);
    ledger.transition(&entry.id, OrderPhase::CancelPending, 3);
    check(&ledger);
    ledger.transition(&entry.id, OrderPhase::Unknown, 4);
    check(&ledger);
    ledger.transition(&entry.id, OrderPhase::Cancelled, 5);
    assert!(!ledger.timed_out(1, 2_000_000_000));
    check(&ledger);
    ledger
        .fill(&entry.id, "late", dec!(1), dec!(98), dec!(0.1), false, 6)
        .unwrap();
    check(&ledger);
    assert!(
        !ledger
            .fill(&entry.id, "late", dec!(1), dec!(98), dec!(0.1), false, 6)
            .unwrap()
    );
    check(&ledger);
    let encoded = serde_json::to_value(&ledger).unwrap();
    assert!(encoded.get("live").is_none());
    ledger = serde_json::from_value(encoded).unwrap();
    check(&ledger);
    let exit = ledger.exit("cache", &entry.id, None, 7).unwrap();
    check(&ledger);
    ledger
        .fill(&exit.id, "exit", dec!(3), dec!(100), dec!(0.3), false, 8)
        .unwrap();
    check(&ledger);
    let deferred = ledger
        .entry("cache", 1, &grid.levels[0], dec!(10), None, 9)
        .unwrap();
    check(&ledger);
    ledger.resize_intent(&deferred.id, Decimal::ZERO).unwrap();
    check(&ledger);
    assert!(!ledger.orders().contains_key(&deferred.id));
    assert!(!ledger.lots().contains_key(&deferred.id));
}

#[rstest]
fn core_rebalances_are_accounted_separately_from_grid_cycles() {
    let config = GridConfig::default();
    let mut ledger = OrderManager::new(config.capital);
    let level = super::engine::GridLevel {
        level_index: 0,
        price: dec!(100),
        side: nautilus_model::enums::OrderSide::Buy,
        exit_price: dec!(101),
        quantity: dec!(2),
        status: super::engine::LevelStatus::Pending,
        entry_order_id: None,
        exit_order_id: None,
    };
    let entry = ledger
        .entry_component(
            "core",
            1,
            &level,
            dec!(2),
            Some(dec!(100)),
            PositionComponent::Core,
            0,
        )
        .unwrap();
    ledger
        .fill(
            &entry.id,
            "core-buy",
            dec!(2),
            dec!(100),
            Decimal::ZERO,
            false,
            1,
        )
        .unwrap();
    let exit = ledger
        .exit_quantity("core", &entry.id, Some(dec!(110)), dec!(2), 2)
        .unwrap();
    ledger
        .fill(
            &exit.id,
            "core-sell",
            dec!(2),
            dec!(110),
            Decimal::ZERO,
            false,
            3,
        )
        .unwrap();

    assert_eq!(ledger.realized_pnl, dec!(20));
    assert_eq!(
        ledger.component_realized_pnl(PositionComponent::Core),
        dec!(20)
    );
    assert_eq!(
        ledger.component_realized_pnl(PositionComponent::Grid),
        Decimal::ZERO
    );
    assert_eq!(
        ledger.component_turnover(PositionComponent::Core),
        dec!(420)
    );
    assert_eq!(ledger.cycles[0].component, PositionComponent::Core);
    ledger.validate().unwrap();
}

#[rstest]
fn hardening_delayed_acknowledgements_never_regress_a_partial_fill() {
    let config = GridConfig::default();
    let mut ledger = OrderManager::new(config.capital);
    let entry = ledger
        .entry("ack", 1, &grid(&config).levels[0], dec!(10), None, 0)
        .unwrap();
    ledger.transition(&entry.id, OrderPhase::Accepted, 1);
    ledger.transition(&entry.id, OrderPhase::Submitted, 2);
    assert_eq!(ledger.orders()[&entry.id].phase, OrderPhase::Accepted);
    ledger
        .fill(&entry.id, "partial", dec!(2), dec!(98), dec!(0.2), false, 3)
        .unwrap();
    for phase in [
        OrderPhase::Submitted,
        OrderPhase::Accepted,
        OrderPhase::Intent,
    ] {
        ledger.transition(&entry.id, phase, 4);
        assert_eq!(
            ledger.orders()[&entry.id].phase,
            OrderPhase::PartiallyFilled
        );
        assert_eq!(ledger.orders()[&entry.id].updated_ns, 1);
    }
    assert_eq!(ledger.buy_reservations(&config, dec!(100)).0, dec!(8));
}

fn capacity() -> RiskSnapshot {
    RiskSnapshot {
        equity: dec!(100000),
        cash: dec!(100000),
        account_equity: dec!(100000),
        account_free: dec!(100000),
        ..Default::default()
    }
}

#[rstest]
fn reservations_bound_position_and_capital() {
    let c = GridConfig {
        max_position: dec!(100),
        ..Default::default()
    };
    let r = RiskManager::new(c.capital);
    let mut s = capacity();
    s.position = dec!(80);
    s.pending_buy_quantity = dec!(15);
    s.exposure = dec!(8000);
    s.pending_buy_notional = dec!(1500);
    assert_eq!(
        r.buy_quantity(&c, &s, dec!(100), dec!(100), dec!(1)),
        dec!(5)
    );
    s.account_free = dec!(1600);
    assert_eq!(
        r.buy_quantity(&c, &s, dec!(100), dec!(100), dec!(1)),
        dec!(0)
    );
}

#[rstest]
fn volatility_sizing_scales_only_inside_existing_risk_capacity() {
    let c = GridConfig {
        position_sizing: PositionSizing::VolatilityAdjusted,
        position_volatility_target: dec!(0.01),
        ..Default::default()
    };
    let r = RiskManager::new(c.capital);
    let mut s = capacity();
    for (atr, expected) in [
        (dec!(0.02), dec!(50)),
        (dec!(0.005), dec!(100)),
        (dec!(0), dec!(0)),
    ] {
        s.atr_pct = atr;
        assert_eq!(
            r.buy_quantity(&c, &s, dec!(100), dec!(100), dec!(1)),
            expected
        );
    }
    s.atr_pct = dec!(0.005);
    s.position = dec!(995);
    assert_eq!(
        r.buy_quantity(&c, &s, dec!(100), dec!(100), dec!(1)),
        dec!(5)
    );
}

#[rstest]
#[case(dec!(89000), dec!(0), "Maximum drawdown")]
#[case(dec!(96000), dec!(0), "Maximum daily loss")]
#[case(dec!(100000), dec!(-9000), "Maximum unrealized loss")]
fn losses_latch_across_restart_and_new_day(
    #[case] equity: Decimal,
    #[case] unrealized: Decimal,
    #[case] reason: &str,
) {
    let c = GridConfig::default();
    let mut r = RiskManager::new(c.capital);
    let mut s = capacity();
    r.observe(&c, &s, 1);
    s.equity = equity;
    s.unrealized_pnl = unrealized;
    r.observe(&c, &s, 2);
    let mut restored: RiskManager =
        serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    restored.observe(&c, &capacity(), 86_400_000_000_001);
    assert_eq!(restored.risk_off_reason.as_deref(), Some(reason));
    assert_eq!(
        restored.buy_quantity(&c, &capacity(), dec!(1), dec!(100), dec!(1)),
        Decimal::ZERO
    );
}

#[rstest]
fn reset_and_order_limits() {
    let c = GridConfig::default();
    let mut r = RiskManager::new(c.capital);
    for _ in 0..=c.max_consecutive_resets {
        r.record_reset();
    }
    r.observe(&c, &capacity(), 1);
    assert!(r.risk_off_reason.is_some());
    let r = RiskManager::new(c.capital);
    let mut s = capacity();
    s.active_orders = c.max_orders;
    assert_eq!(
        r.buy_quantity(&c, &s, dec!(1), dec!(100), dec!(1)),
        Decimal::ZERO
    );
}

#[rstest]
#[case(0.0, MarketRegime::Range)]
#[case(1.0, MarketRegime::TrendUp)]
#[case(-0.4, MarketRegime::TrendDown)]
fn regime_warmup_and_trends(#[case] slope: f64, #[case] expected: MarketRegime) {
    let c = GridConfig {
        enable_volatility_filter: false,
        ..Default::default()
    };
    let mut r = RegimeDetector::default();
    for i in 0..100 {
        let close = 100.0 + slope * i as f64;
        r.update(
            &c,
            Observation {
                ts_ns: i + 1,
                high: close + 0.1,
                low: close - 0.1,
                close,
            },
        )
        .unwrap();
        if i < 25 {
            assert!(!r.snapshot.initialized);
        }
    }
    assert_eq!(r.snapshot.regime, expected);
}

#[rstest]
fn volatility_and_recovery_parity() {
    let c = GridConfig::default();
    let mut r = RegimeDetector::default();
    for i in 0..100 {
        r.update(
            &c,
            Observation {
                ts_ns: i + 1,
                high: 110.0,
                low: 90.0,
                close: 100.0,
            },
        )
        .unwrap();
    }
    assert_eq!(r.snapshot.regime, MarketRegime::HighVolatility);
    let mut restored: RegimeDetector =
        serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    let bar = Observation {
        ts_ns: 101,
        high: 105.0,
        low: 95.0,
        close: 100.0,
    };
    r.update(&c, bar).unwrap();
    restored.update(&c, bar).unwrap();
    assert_eq!(
        serde_json::to_value(r.snapshot).unwrap(),
        serde_json::to_value(restored.snapshot.clone()).unwrap()
    );
    assert!(restored.update(&c, bar).is_err());
}

fn grid(config: &GridConfig) -> GridEngine {
    GridEngine::build(
        config,
        1,
        dec!(100),
        dec!(0.02),
        dec!(10000),
        dec!(0.01),
        dec!(1),
        0,
    )
    .unwrap()
}

#[rstest]
fn geometric_levels_and_rounding() {
    let config = GridConfig {
        grid_levels: 3,
        ..Default::default()
    };
    let grid = grid(&config);
    assert_eq!(grid.levels[0].price, dec!(98.03));
    assert_eq!(grid.levels[0].exit_price, dec!(100));
    assert_eq!(grid.levels[2].price, dec!(96.11));
    assert_eq!(grid.levels[3].exit_price, dec!(104.04));
    assert!(grid.levels.iter().all(|l| l.quantity.fract().is_zero()));
}

#[rstest]
fn crossing_only_once_and_in_path_order() {
    let grid = grid(&GridConfig::default());
    assert_eq!(grid.crossed(dec!(100), dec!(97)), vec![-1]);
    assert!(grid.crossed(dec!(98.03), dec!(98.03)).is_empty());
    assert_eq!(grid.crossed(dec!(99), dec!(100)), vec![-1]);
    assert_eq!(grid.crossed(dec!(100), dec!(95)), vec![-1, -2]);
}

#[rstest]
fn reset_requires_time_and_distance() {
    let c = GridConfig::default();
    let g = grid(&c);
    assert!(!g.can_reset(&c, dec!(120), Decimal::ZERO, 1));
    assert!(!g.can_reset(&c, dec!(100.5), Decimal::ZERO, 400_000_000_000));
    assert!(g.can_reset(&c, dec!(120), Decimal::ZERO, 400_000_000_000));
}

#[rstest]
#[case(PositionSizing::Equal)]
#[case(PositionSizing::Progressive)]
#[case(PositionSizing::Inverse)]
#[case(PositionSizing::VolatilityAdjusted)]
fn sizing_respects_allocated_capital(#[case] sizing: PositionSizing) {
    let c = GridConfig {
        position_sizing: sizing,
        ..Default::default()
    };
    let g = grid(&c);
    let cost: Decimal = g.levels.iter().map(|l| l.quantity * l.price).sum();
    assert!(cost <= dec!(10000));
    let near = &g.levels[0];
    let far = &g.levels[18];
    match sizing {
        PositionSizing::Progressive => {
            assert!(far.quantity * far.price > near.quantity * near.price);
        }
        PositionSizing::Inverse => assert!(far.quantity * far.price < near.quantity * near.price),
        PositionSizing::Equal | PositionSizing::VolatilityAdjusted => {
            assert!((far.quantity * far.price - near.quantity * near.price).abs() < dec!(100));
        }
    }
}

#[rstest]
fn spacing_covers_fees_and_slippage() {
    let c = GridConfig::default();
    assert_eq!(
        spacing(&c, dec!(0.01), dec!(100), Decimal::ONE).unwrap(),
        c.min_spacing_pct
    );
    assert_eq!(
        spacing(&c, dec!(20), dec!(100), Decimal::ONE).unwrap(),
        c.max_spacing_pct
    );
    let invalid = GridConfig {
        taker_fee: dec!(0.1),
        ..c
    };
    assert!(invalid.validate().is_err());
}

#[rstest]
fn invalid_inputs_and_collapsed_ticks() {
    let c = GridConfig::default();
    assert!(GridEngine::build(&c, 1, dec!(1), dec!(0.02), dec!(10), dec!(1), dec!(1), 0).is_err());
    assert!(
        GridConfig {
            grid_levels: 0,
            ..c.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        GridConfig {
            adx_range_max: f64::NAN,
            ..c
        }
        .validate()
        .is_err()
    );
}
