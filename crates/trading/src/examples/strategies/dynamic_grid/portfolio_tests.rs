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

use nautilus_model::identifiers::InstrumentId;
use rstest::rstest;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::{
    orders::OrderPhase,
    portfolio::{InstrumentExposure, OrderDecision, PortfolioConfig, PortfolioRiskManager},
    regime::MarketRegime,
};

fn exposure(id: &str, sector: &str) -> InstrumentExposure {
    InstrumentExposure {
        id: InstrumentId::from(id),
        sector: Some(sector.to_string()),
        base_allocation: dec!(0.2),
        max_position_pct: dec!(0.2),
        enabled: true,
        cash_delta: Decimal::ZERO,
        exposure: Decimal::ZERO,
        pending: Decimal::ZERO,
        net_pnl: Decimal::ZERO,
        regime: MarketRegime::Range,
        atr_pct: dec!(0.01),
        risk_off: false,
        mark_ns: 1,
        max_age_secs: 180,
    }
}

fn admit(
    risk: &mut PortfolioRiskManager,
    config: &PortfolioConfig,
    views: &[InstrumentExposure],
    id: InstrumentId,
) -> (OrderDecision, Decimal) {
    risk.admit(
        config,
        views,
        id,
        dec!(100000),
        dec!(100000),
        dec!(100000),
        dec!(200),
        dec!(100),
        dec!(1),
        1,
    )
}

#[rstest]
fn simultaneous_buys_reserve_shared_capacity_before_any_fill() {
    let c = PortfolioConfig::default();
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views = vec![
        exposure("AAPL.SIM", "Technology"),
        exposure("NVDA.SIM", "Semiconductor"),
        exposure("TSLA.SIM", "Automotive"),
    ];
    let first = admit(&mut r, &c, &views, views[0].id);
    views[0].pending = first.1 * dec!(100);
    let second = admit(&mut r, &c, &views, views[1].id);
    views[1].pending = second.1 * dec!(100);
    let third = admit(&mut r, &c, &views, views[2].id);
    assert_eq!(first, (OrderDecision::Allow, dec!(200)));
    assert_eq!(second, (OrderDecision::Reduce, dec!(100)));
    assert_eq!(third, (OrderDecision::Defer, Decimal::ZERO));
    assert_eq!(r.decisions, [1, 1, 1, 0]);
}

#[rstest]
fn sector_exposure_includes_other_instruments_and_pending_orders() {
    let c = PortfolioConfig {
        max_sector_exposure: dec!(0.15),
        ..Default::default()
    };
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views = vec![exposure("NVDA.SIM", "Semi"), exposure("AMD.SIM", "Semi")];
    views[0].exposure = dec!(10000);
    views[0].pending = dec!(2000);
    assert_eq!(
        admit(&mut r, &c, &views, views[1].id),
        (OrderDecision::Reduce, dec!(30))
    );
}

#[rstest]
fn five_simultaneous_symbols_obey_order_value_and_concurrency_before_fills() {
    let c = PortfolioConfig {
        max_order_value: dec!(5000),
        max_concurrent_symbols: 3,
        ..Default::default()
    };
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views: Vec<_> = ["AAPL.SIM", "MSFT.SIM", "NVDA.SIM", "TSLA.SIM", "AMZN.SIM"]
        .into_iter()
        .map(|id| exposure(id, id))
        .collect();
    for index in 0..views.len() {
        let (decision, quantity) = admit(&mut r, &c, &views, views[index].id);
        if index < 3 {
            assert_eq!((decision, quantity), (OrderDecision::Reduce, dec!(50)));
        } else {
            assert_eq!((decision, quantity), (OrderDecision::Defer, Decimal::ZERO));
        }
        views[index].pending += quantity * dec!(100);
    }
    assert_eq!(
        views.iter().map(|v| v.pending).sum::<Decimal>(),
        dec!(15000)
    );
    // An already-funded symbol may continue within its budget; an unresolved cancel still counts.
    assert_eq!(admit(&mut r, &c, &views, views[0].id).1, dec!(50));
    let restored: PortfolioRiskManager =
        serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    let mut restored = restored;
    assert_eq!(
        admit(&mut restored, &c, &views, views[4].id).1,
        Decimal::ZERO
    );
    views[0].pending = Decimal::ZERO;
    assert_eq!(admit(&mut restored, &c, &views, views[4].id).1, dec!(50));
}

#[rstest]
#[case(dec!(0.60), dec!(0.60), dec!(0.60), dec!(0.90), dec!(100))]
#[case(dec!(0.05), dec!(0.60), dec!(0.60), dec!(0.30), dec!(50))]
#[case(dec!(0.60), dec!(0.04), dec!(0.60), dec!(0.30), dec!(40))]
#[case(dec!(0.60), dec!(0.60), dec!(0.03), dec!(0.30), dec!(30))]
fn every_portfolio_exposure_and_cash_limit_is_a_final_gate(
    #[case] total: Decimal,
    #[case] grid: Decimal,
    #[case] equities: Decimal,
    #[case] reserve: Decimal,
    #[case] expected: Decimal,
) {
    let c = PortfolioConfig {
        max_total_exposure: total,
        max_total_grid_exposure: grid,
        max_total_equity_exposure: equities,
        min_cash_reserve: reserve,
        ..Default::default()
    };
    let mut r = PortfolioRiskManager::new(c.capital);
    let views = vec![exposure("AAPL.SIM", "Tech")];
    assert_eq!(
        admit(&mut r, &c, &views, views[0].id),
        (OrderDecision::Reduce, expected)
    );
}

#[rstest]
fn portfolio_drawdown_overrides_independently_healthy_instrument() {
    let c = PortfolioConfig {
        max_portfolio_daily_loss: dec!(0.5),
        ..Default::default()
    };
    let mut r = PortfolioRiskManager::new(c.capital);
    r.observe(&c, dec!(100000), 1);
    r.observe(&c, dec!(84000), 2);
    let views = vec![exposure("AAPL.SIM", "Tech")];
    assert_eq!(
        admit(&mut r, &c, &views, views[0].id),
        (OrderDecision::Reject, Decimal::ZERO)
    );
    assert_eq!(
        r.risk_off_reason.as_deref(),
        Some("Maximum portfolio drawdown")
    );
    r.observe(&c, dec!(110000), 86_400_000_000_001);
    assert!(r.risk_off_reason.is_some());
}

#[rstest]
fn portfolio_daily_loss_includes_overnight_gap_and_survives_restart() {
    let c = PortfolioConfig::default();
    let mut r = PortfolioRiskManager::new(c.capital);
    r.observe(&c, dec!(100000), 1);
    r.observe(&c, dec!(94000), 86_400_000_000_001);
    let saved = serde_json::to_string(&r).unwrap();
    let recovered: PortfolioRiskManager = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        recovered.risk_off_reason.as_deref(),
        Some("Maximum portfolio daily loss")
    );
    assert_eq!(recovered.day_start_equity, dec!(100000));
}

#[rstest]
fn operator_kill_switch_latches_across_restart() {
    let mut c = PortfolioConfig {
        kill_switch: true,
        ..Default::default()
    };
    let mut risk = PortfolioRiskManager::new(c.capital);
    let views = vec![exposure("AAPL.SIM", "Technology")];
    assert_eq!(
        admit(&mut risk, &c, &views, views[0].id).0,
        OrderDecision::Reject
    );
    risk.observe(&c, c.capital, 1);
    let mut restored: PortfolioRiskManager =
        serde_json::from_slice(&serde_json::to_vec(&risk).unwrap()).unwrap();
    c.kill_switch = false;
    restored.observe(&c, c.capital, 86_400_000_000_001);
    assert_eq!(
        admit(&mut restored, &c, &views, views[0].id).0,
        OrderDecision::Reject
    );
    assert_eq!(
        restored.risk_off_reason.as_deref(),
        Some("Operator kill switch")
    );
}

#[rstest]
fn reallocation_is_throttled_but_hard_risk_reduction_is_immediate() {
    let c = PortfolioConfig::default();
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views = vec![exposure("AAPL.SIM", "Tech")];
    let id = views[0].id;
    r.reallocate(&c, &views, 1);
    views[0].regime = MarketRegime::TrendDown;
    r.reallocate(&c, &views, 2);
    assert_eq!(r.allocations[&id], dec!(0.05));
    views[0].regime = MarketRegime::Range;
    r.reallocate(&c, &views, 3);
    assert_eq!(r.allocations[&id], dec!(0.05));
    r.reallocate(&c, &views, 3_600_000_000_003);
    assert_eq!(r.allocations[&id], dec!(0.2));
    views[0].risk_off = true;
    r.reallocate(&c, &views, 3_600_000_000_004);
    assert_eq!(r.allocations[&id], Decimal::ZERO);
}

#[rstest]
fn correlation_uses_aligned_completed_days_and_ignores_future_closes() {
    let c = PortfolioConfig {
        correlation_min_observations: 2,
        correlation_lookback_days: 10,
        ..Default::default()
    };
    let mut r = PortfolioRiskManager::new(c.capital);
    let a = InstrumentId::from("NVDA.SIM");
    let b = InstrumentId::from("AMD.SIM");
    for (i, price) in [100, 102, 101, 104].iter().enumerate() {
        r.close(
            &c,
            a,
            (i as u64 + 1) * 86_400_000_000_000,
            Decimal::from(*price),
        );
        r.close(
            &c,
            b,
            (i as u64 + 1) * 86_400_000_000_000,
            Decimal::from(price * 2),
        );
    }
    let now = 5 * 86_400_000_000_000;
    let original = r.correlation(&c, a, b, now).unwrap();
    r.close(&c, b, now, dec!(1));
    r.close(&c, a, now, dec!(105));
    assert!((original - 1.0).abs() < 1e-12);
    assert_eq!(r.correlation(&c, a, b, now), Some(original));
    assert!(r.correlation(&c, a, b, now + 86_400_000_000_000).unwrap() < original);
}

#[rstest]
#[case::current(1, OrderDecision::Allow, dec!(1))]
#[case::exact_limit(180_000_000_001, OrderDecision::Allow, dec!(1))]
#[case::one_nanosecond_expired(180_000_000_002, OrderDecision::Defer, Decimal::ZERO)]
#[case::future_mark(0, OrderDecision::Defer, Decimal::ZERO)]
fn portfolio_entry_admission_respects_exact_mark_age(
    #[case] now: u64,
    #[case] decision: OrderDecision,
    #[case] quantity: Decimal,
) {
    let config = PortfolioConfig::default();
    let mut risk = PortfolioRiskManager::new(config.capital);
    let mut views = vec![exposure("AAPL.SIM", "Tech"), exposure("TSLA.SIM", "Auto")];
    views[0].exposure = dec!(100);
    let result = risk.admit(
        &config,
        &views,
        views[1].id,
        dec!(100000),
        dec!(99900),
        dec!(99900),
        dec!(1),
        dec!(100),
        dec!(1),
        now,
    );
    assert_eq!(result, (decision, quantity));
}

#[rstest]
fn stale_held_instrument_blocks_other_instruments_from_spending() {
    let c = PortfolioConfig::default();
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views = vec![exposure("AAPL.SIM", "Tech"), exposure("TSLA.SIM", "Auto")];
    views[0].exposure = dec!(100);
    let result = r.admit(
        &c,
        &views,
        views[1].id,
        dec!(100000),
        dec!(99900),
        dec!(99900),
        dec!(1),
        dec!(100),
        dec!(1),
        181_000_000_002,
    );
    assert_eq!(result, (OrderDecision::Defer, Decimal::ZERO));
}

#[rstest]
fn instrument_position_fraction_is_relative_to_shared_equity() {
    let c = PortfolioConfig::default();
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views = vec![exposure("TSLA.SIM", "Auto")];
    views[0].max_position_pct = dec!(0.05);
    assert_eq!(
        admit(&mut r, &c, &views, views[0].id),
        (OrderDecision::Reduce, dec!(50))
    );
}

#[rstest]
#[case(OrderPhase::Unknown)]
#[case(OrderPhase::CancelPending)]
fn uncertain_cancellation_never_retries_inside_the_event_callback(
    #[case] phase: super::orders::OrderPhase,
) {
    let id = InstrumentId::from("AAPL.SIM");
    let config =
        super::DynamicGridConfig::new(id, "AAPL.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap());
    let mut engine = super::strategy::GridStrategyEngine::new(config.clone());
    let mut runtime = super::MultiAssetGridStrategy::new(config).unwrap();
    let level = super::engine::GridLevel {
        price: dec!(98),
        side: nautilus_model::enums::OrderSide::Buy,
        level_index: -1,
        status: super::engine::LevelStatus::Pending,
        quantity: dec!(1),
        exit_price: dec!(100),
        entry_order_id: None,
        exit_order_id: None,
    };
    let intent = engine
        .state
        .orders
        .entry("901-AAPL.SIM", 1, &level, dec!(1), None, 1)
        .unwrap();
    engine.state.orders.transition(&intent.id, phase, 2);
    // An attempted native cancellation would fail on this unregistered runtime
    engine.cancel(&mut runtime, true, "TEST_CANCEL").unwrap();
    engine.halt(&mut runtime, anyhow::anyhow!("Simulated recovery failure"));
    assert_eq!(engine.state.orders.orders()[&intent.id].phase, phase);
    assert!(
        engine
            .state
            .orders
            .buy_reservations(&engine.config.grid, dec!(98))
            .1
            >= dec!(98)
    );
}

#[rstest]
fn correlation_limits_the_entire_connected_cluster_not_only_direct_pairs() {
    let c = PortfolioConfig {
        correlation_threshold: 0.6,
        correlation_min_observations: 2,
        correlation_lookback_days: 20,
        max_sector_exposure: Decimal::ONE,
        ..Default::default()
    };
    let mut r = PortfolioRiskManager::new(c.capital);
    let mut views = vec![
        exposure("NVDA.SIM", "A"),
        exposure("AMD.SIM", "B"),
        exposure("AVGO.SIM", "C"),
    ];
    let mut prices = [dec!(100); 3];
    for (day, (x, y)) in [
        (0, 0),
        (1, 0),
        (0, 1),
        (-1, 0),
        (0, -1),
        (1, 0),
        (0, 1),
        (-1, 0),
        (0, -1),
    ]
    .into_iter()
    .enumerate()
    {
        for (i, change) in [x, x + y, y].into_iter().enumerate() {
            prices[i] *= Decimal::ONE + Decimal::new(change, 2);
            r.close(
                &c,
                views[i].id,
                (day as u64 + 1) * 86_400_000_000_000,
                prices[i],
            );
        }
    }
    let now = 10 * 86_400_000_000_000;
    assert!(
        r.correlation(&c, views[0].id, views[2].id, now)
            .unwrap()
            .abs()
            < 0.01
    );
    assert!(r.correlation(&c, views[0].id, views[1].id, now).unwrap() > c.correlation_threshold);
    assert!(r.correlation(&c, views[1].id, views[2].id, now).unwrap() > c.correlation_threshold);
    views[0].exposure = dec!(15000);
    views[2].exposure = dec!(15000);
    for v in &mut views {
        v.mark_ns = now;
    }
    assert_eq!(
        r.admit(
            &c,
            &views,
            views[0].id,
            dec!(100000),
            dec!(70000),
            dec!(70000),
            dec!(10),
            dec!(100),
            dec!(1),
            now
        ),
        (OrderDecision::Defer, Decimal::ZERO)
    );
}
