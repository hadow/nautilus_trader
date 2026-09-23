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

use std::{
    cell::RefCell,
    hint::black_box,
    rc::Rc,
    time::{Duration, Instant},
};

use nautilus_common::{cache::Cache, clock::TestClock};
use nautilus_core::UUID4;
use nautilus_model::{
    accounts::{Account, AccountAny, CashAccount},
    enums::{AccountType, OmsType, OrderSide},
    events::{AccountState, order::spec::OrderFilledSpec},
    identifiers::{AccountId, InstrumentId, PositionId, StrategyId, Symbol, TraderId},
    instruments::{Equity, InstrumentAny},
    position::Position,
    types::{AccountBalance, Currency, Money, Price, Quantity},
};
use nautilus_portfolio::portfolio::Portfolio;
use rstest::rstest;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::{DynamicGridConfig, GridStrategyEngine, MultiAssetGridStrategy};

#[rstest]
fn recovery_checks_exact_grid_geometry_against_native_instrument() {
    use super::super::engine::GridEngine;

    let (runtime, mut engine, _) = runtime(1);
    engine.state.generation = 1;
    engine.state.grid = Some(
        GridEngine::build(
            &engine.config.grid,
            1,
            dec!(100),
            dec!(0.02),
            dec!(10000),
            dec!(0.01),
            dec!(1),
            0,
        )
        .unwrap(),
    );
    engine.recover(&runtime).unwrap();
    engine.state.grid.as_mut().unwrap().levels[0].price += dec!(0.01);
    assert!(
        engine.recover(&runtime).is_err(),
        "Changed geometry must not resume trading"
    );
}

#[rstest]
fn checkpoint_restores_geometry_and_rejects_corrupt_levels() {
    use super::super::engine::GridEngine;

    let (mut runtime, mut engine, _) = runtime(1);
    let id = engine.config.instrument_id;
    runtime.config.instruments.get_mut(&id).unwrap().grid = engine.config.grid.clone();
    engine.state.generation = 1;
    engine.state.grid = Some(
        GridEngine::build(
            &engine.config.grid,
            1,
            dec!(100),
            dec!(0.02),
            dec!(10000),
            dec!(0.01),
            dec!(1),
            0,
        )
        .unwrap(),
    );
    runtime.engines.insert(id, engine);
    let saved = runtime.checkpoint(None, None);
    let recovered: super::Checkpoint =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    recovered.validate(&runtime.config).unwrap();
    for mutation in ["index", "duplicate", "quantity"] {
        let mut corrupt = recovered.clone();
        let grid = corrupt
            .instruments
            .get_mut(&id)
            .unwrap()
            .state
            .grid
            .as_mut()
            .unwrap();
        match mutation {
            "index" => grid.levels[0].level_index = 0,
            "duplicate" => grid.levels[1] = grid.levels[0].clone(),
            "quantity" => grid.levels[0].quantity = dec!(-1),
            _ => unreachable!(),
        }
        assert!(corrupt.validate(&runtime.config).is_err(), "{mutation}");
    }
}

#[rstest]
fn hardening_recovery_rejects_forged_regime_snapshot() {
    use super::super::regime::Observation;

    let (runtime, mut engine, _) = runtime(1);
    engine
        .state
        .regime
        .update(
            &engine.config.grid,
            Observation {
                ts_ns: 0,
                high: 101.0,
                low: 99.0,
                close: 100.0,
            },
        )
        .unwrap();
    engine.recover(&runtime).unwrap();
    engine.state.regime.snapshot.atr = 1000.0;
    assert!(engine.recover(&runtime).is_err());
}

#[rstest]
#[case::long_expired(1_000_000_000_000)]
#[case::one_nanosecond_expired(180_000_000_001)]
fn hardening_reset_waits_for_fresh_signal_after_cancellation(#[case] now: u64) {
    use super::super::engine::GridEngine;
    let (mut runtime, mut engine, _) = runtime(1);
    runtime.recovering = false;
    engine.config.grid.spacing_mode = super::super::config::SpacingMode::Percentage;
    engine.config.grid.spacing_pct = dec!(0.02);
    engine.state.generation = 1;
    engine.state.grid = Some(
        GridEngine::build(
            &engine.config.grid,
            1,
            dec!(100),
            dec!(0.02),
            dec!(10000),
            dec!(0.01),
            dec!(1),
            0,
        )
        .unwrap(),
    );
    engine.state.reset_reason = Some("GRID_RESET_UP".into());
    engine.state.state = super::StrategyState::GridResetting;
    engine.state.regime.snapshot.initialized = true;
    engine.drive(&mut runtime, dec!(103), now).unwrap();
    assert_eq!(engine.state.grid.as_ref().unwrap().grid_id, 1);
    assert_eq!(engine.state.risk.total_resets, 0);
    assert!(engine.state.reset_reason.is_some());
    assert!(engine.state.orders.active_ids().is_empty());
}

fn account_event(total: Decimal, locked: Decimal, ts: u64) -> AccountState {
    let currency = Currency::USD();
    AccountState::new(
        AccountId::from("SIM-001"),
        AccountType::Cash,
        vec![AccountBalance::new(
            Money::from_decimal(total, currency).unwrap(),
            Money::from_decimal(locked, currency).unwrap(),
            Money::from_decimal(total - locked, currency).unwrap(),
        )],
        vec![],
        true,
        UUID4::new(),
        ts.into(),
        ts.into(),
        Some(currency),
    )
}

#[rstest]
fn watchdog_queries_unacknowledged_submissions_without_releasing_cash() {
    use nautilus_common::{
        messages::execution::TradingCommand,
        msgbus::{self, TypedIntoHandler, switchboard::MessagingSwitchboard},
        timer::TimeEvent,
    };
    use nautilus_model::{
        enums::OrderType, identifiers::ClientOrderId, orders::builder::OrderTestBuilder,
    };

    use super::super::{engine::GridEngine, orders::OrderPhase};

    let (mut runtime, mut engine, cache) = runtime(1);
    let now = (engine.config.grid.order_timeout_secs + 1) * 1_000_000_000;
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time(now.into(), true);
    engine.state.last_market_ns = now;
    let grid = GridEngine::build(
        &engine.config.grid,
        1,
        dec!(100),
        dec!(0.02),
        dec!(10000),
        dec!(0.01),
        dec!(1),
        0,
    )
    .unwrap();
    let intent = engine
        .state
        .orders
        .entry("901-AAPL.SIM", 1, &grid.levels[0], dec!(1), None, 0)
        .unwrap();
    engine
        .state
        .orders
        .transition(&intent.id, OrderPhase::Submitted, 0);
    let id = ClientOrderId::from(intent.id.as_str());
    let order = OrderTestBuilder::new(OrderType::Limit)
        .instrument_id(engine.config.instrument_id)
        .client_order_id(id)
        .side(OrderSide::Buy)
        .quantity(Quantity::from(1))
        .price(Price::from("98.00"))
        .build();
    cache
        .borrow_mut()
        .add_order(order, None, None, false)
        .unwrap();
    let commands = Rc::new(RefCell::new(Vec::new()));
    let received = Rc::clone(&commands);
    msgbus::register_trading_command_endpoint(
        MessagingSwitchboard::exec_engine_queue_execute(),
        TypedIntoHandler::from(move |command: TradingCommand| received.borrow_mut().push(command)),
    );
    let reserved = engine
        .state
        .orders
        .buy_reservations(&engine.config.grid, dec!(100));
    engine
        .on_time_event(
            &mut runtime,
            &TimeEvent::new("GRID_WATCHDOG".into(), UUID4::new(), now.into(), now.into()),
        )
        .unwrap();
    assert_eq!(engine.state.state, super::StrategyState::RiskOff);
    assert_eq!(
        engine
            .state
            .orders
            .buy_reservations(&engine.config.grid, dec!(100)),
        reserved
    );
    assert!(
        matches!(&commands.borrow()[0], TradingCommand::QueryOrder(query) if query.client_order_id == id && query.venue_order_id.is_none())
    );
    assert_eq!(commands.borrow().len(), 1);
}

#[rstest]
#[case::rejected(true)]
#[case::expired(false)]
fn hardening_native_terminal_events_release_only_their_owned_reservations(#[case] rejected: bool) {
    use nautilus_model::{
        events::order::spec::{OrderExpiredSpec, OrderRejectedSpec},
        identifiers::ClientOrderId,
    };

    use super::super::{engine::GridEngine, orders::OrderPhase};
    use crate::strategy::Strategy;
    let (mut runtime, mut engine, _) = runtime(1);
    let id = engine.config.instrument_id;
    let level = GridEngine::build(
        &engine.config.grid,
        1,
        dec!(100),
        dec!(0.02),
        dec!(10000),
        dec!(0.01),
        dec!(1),
        0,
    )
    .unwrap()
    .levels[0]
        .clone();
    let order = engine
        .state
        .orders
        .entry("901-AAPL.SIM", 1, &level, dec!(1), None, 0)
        .unwrap();
    engine
        .state
        .orders
        .transition(&order.id, OrderPhase::Submitted, 0);
    engine.state.last_price = Some(dec!(100));
    engine.state.state = super::StrategyState::GridActive;
    runtime.engines.insert(id, engine);
    runtime.recovering = false;
    if rejected {
        let event = OrderRejectedSpec::builder()
            .instrument_id(id)
            .client_order_id(ClientOrderId::from(order.id.as_str()))
            .reason("Broker rejected inventory acquisition".into())
            .build();
        Strategy::on_order_rejected(&mut runtime, event.clone());
        Strategy::on_order_rejected(&mut runtime, event);
    } else {
        Strategy::on_order_expired(
            &mut runtime,
            OrderExpiredSpec::builder()
                .instrument_id(id)
                .client_order_id(ClientOrderId::from(order.id.as_str()))
                .build(),
        );
    }
    let engine = &runtime.engines[&id];
    let phase = serde_json::to_value(engine.state.orders.orders()[&order.id].phase).unwrap();
    assert_eq!(phase, if rejected { "Rejected" } else { "Expired" });
    assert!(engine.state.orders.active_ids().is_empty());
    assert_eq!(engine.state.orders.inventory(), Decimal::ZERO);
    assert_eq!(
        engine
            .state
            .orders
            .buy_reservations(&engine.config.grid, dec!(100))
            .1,
        Decimal::ZERO
    );
    if rejected {
        assert!(engine.state.risk.risk_off_reason.is_some());
        let report = engine.report.borrow();
        assert_eq!(report.diagnostics.rejections.len(), 1);
        assert_eq!(
            report.diagnostics.rejections[&order.id].reason,
            "Order rejected: Broker rejected inventory acquisition"
        );
        assert_eq!(report.diagnostics.risk_stops.len(), 1);
        assert_eq!(report.diagnostics.risk_stops.values().next(), Some(&0));
    } else {
        assert!(engine.report.borrow().diagnostics.rejections.is_empty());
    }
}

#[rstest]
fn hardening_disconnect_reconnect_never_automatically_reopens_entry_gate() {
    use nautilus_common::{
        actor::DataActor,
        messages::system::{SocketState, SocketStateChanged},
    };
    use nautilus_model::identifiers::ClientId;
    let (mut runtime, mut engine, _) = runtime(1);
    let id = engine.config.instrument_id;
    engine.state.last_price = Some(dec!(100));
    engine.state.state = super::StrategyState::GridActive;
    runtime.engines.insert(id, engine);
    runtime.recovering = false;
    for state in [SocketState::Disconnected, SocketState::Connected] {
        DataActor::on_socket_state(
            &mut runtime,
            &SocketStateChanged::new(
                TraderId::from("TESTER-001"),
                ClientId::from("LONGBRIDGE"),
                Some(id.venue),
                "test-stream".into(),
                state,
                UUID4::new(),
                0.into(),
                0.into(),
            ),
        )
        .unwrap();
        assert!(runtime.portfolio_blocked());
        assert!(runtime.recovering);
        assert!(runtime.portfolio_risk.risk_off_reason.is_some());
    }
}

#[rstest]
fn diagnostics_cancel_rejection_preserves_reason_and_does_not_release_reservations() {
    use nautilus_model::{
        events::order::spec::OrderCancelRejectedSpec, identifiers::ClientOrderId,
    };

    use super::super::{engine::GridEngine, orders::OrderPhase};
    use crate::strategy::Strategy;

    let (mut runtime, mut engine, _) = runtime(1);
    let id = engine.config.instrument_id;
    let grid = GridEngine::build(
        &engine.config.grid,
        1,
        dec!(100),
        dec!(0.02),
        dec!(10000),
        dec!(0.01),
        dec!(1),
        0,
    )
    .unwrap();
    let order = engine
        .state
        .orders
        .entry("901-AAPL.SIM", 1, &grid.levels[0], dec!(1), None, 0)
        .unwrap();
    engine
        .state
        .orders
        .transition(&order.id, OrderPhase::CancelPending, 0);
    engine.state.last_price = Some(dec!(100));
    runtime.engines.insert(id, engine);
    runtime.recovering = false;
    let event = OrderCancelRejectedSpec::builder()
        .instrument_id(id)
        .client_order_id(ClientOrderId::from(order.id.as_str()))
        .reason("Cancellation outcome unknown".into())
        .build();
    Strategy::on_order_cancel_rejected(&mut runtime, event.clone());
    Strategy::on_order_cancel_rejected(&mut runtime, event);
    let engine = &runtime.engines[&id];
    let report = engine.report.borrow();
    assert_eq!(
        engine.state.orders.orders()[&order.id].phase,
        OrderPhase::Unknown
    );
    assert_eq!(engine.state.orders.active_ids(), vec![order.id.clone()]);
    assert!(
        engine
            .state
            .orders
            .buy_reservations(&engine.config.grid, dec!(100))
            .1
            > Decimal::ZERO
    );
    assert!(report.diagnostics.rejections.is_empty());
    assert_eq!(report.diagnostics.cancel_rejections.len(), 1);
    assert_eq!(
        report.diagnostics.cancel_rejections[&order.id].reason,
        "Cancellation outcome unknown"
    );
    assert_eq!(
        runtime.portfolio_performance.diagnostics.risk_stops.len(),
        1
    );
}

fn runtime(
    history: usize,
) -> (
    MultiAssetGridStrategy,
    GridStrategyEngine,
    Rc<RefCell<Cache>>,
) {
    let id = InstrumentId::from("AAPL.SIM");
    let config = DynamicGridConfig::new(id, "AAPL.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap());
    let instrument = InstrumentAny::Equity(
        Equity::builder()
            .instrument_id(id)
            .raw_symbol(Symbol::from("AAPL"))
            .currency(Currency::USD())
            .price_precision(2)
            .price_increment(Price::from("0.01"))
            .lot_size(Quantity::from(1))
            .ts_event(0.into())
            .ts_init(0.into())
            .build()
            .unwrap(),
    );
    let mut account = CashAccount::new(account_event(dec!(100000), dec!(1234), 0), true, false);
    for ts in 1..history {
        account
            .apply(account_event(dec!(100000), dec!(1234), ts as u64))
            .unwrap();
    }
    let cache = Rc::new(RefCell::new(Cache::default()));
    cache
        .borrow_mut()
        .add_account(AccountAny::Cash(account))
        .unwrap();
    cache
        .borrow_mut()
        .add_instrument(instrument.clone())
        .unwrap();
    let clock = Rc::new(RefCell::new(TestClock::new()));
    let portfolio = Rc::new(RefCell::new(Portfolio::new(
        clock.clone(),
        cache.clone(),
        None,
    )));
    let mut runtime = MultiAssetGridStrategy::new(config.clone()).unwrap();
    runtime
        .core
        .register(
            TraderId::from("TESTER-001"),
            clock,
            cache.clone(),
            portfolio,
        )
        .unwrap();
    let mut engine = GridStrategyEngine::new(config);
    engine.instrument = Some(instrument);
    (runtime, engine, cache)
}

#[rstest]
fn risk_reads_observe_balance_changes_and_release_borrows() {
    let (runtime, engine, cache) = runtime(100);
    let before = engine.snapshot(&runtime, dec!(100)).unwrap();
    let capacity_before = runtime.account_capacity(&engine).unwrap();
    cache
        .borrow_mut()
        .account_mut(&AccountId::from("SIM-001"))
        .unwrap()
        .apply(account_event(dec!(80000), dec!(3000), 101))
        .unwrap();
    let after = engine.snapshot(&runtime, dec!(100)).unwrap();
    let capacity_after = runtime.account_capacity(&engine).unwrap();

    assert_eq!(before.account_free, dec!(98766));
    assert_eq!(before.account_equity, dec!(100000));
    assert_eq!(capacity_before, (dec!(98766), dec!(100000)));
    assert_eq!(after.account_free, dec!(77000));
    assert_eq!(after.account_equity, dec!(80000));
    assert_eq!(capacity_after, (dec!(77000), dec!(80000)));
    assert!(cache.try_borrow_mut().is_ok());
}

#[rstest]
#[case::foreign_strategy("OTHER-001", OrderSide::Buy)]
#[case::short_inventory("DYNAMIC-GRID-901", OrderSide::Sell)]
fn risk_reads_reject_unexpected_positions(#[case] strategy_id: &str, #[case] side: OrderSide) {
    let (runtime, engine, cache) = runtime(10);
    let fill = OrderFilledSpec::builder()
        .instrument_id(engine.config.instrument_id)
        .strategy_id(StrategyId::from(strategy_id))
        .account_id(AccountId::from("SIM-001"))
        .position_id(PositionId::from("GRID-POSITION-1"))
        .order_side(side)
        .last_qty(Quantity::from(1))
        .last_px(Price::from("100.00"))
        .build();
    let position = Position::new(engine.instrument.as_ref().unwrap(), fill);
    cache
        .borrow_mut()
        .add_position(&position, OmsType::Netting)
        .unwrap();

    let capacity = runtime.account_capacity(&engine).unwrap_err();
    let snapshot = engine.snapshot(&runtime, dec!(100)).unwrap_err();

    assert!(capacity.to_string().contains("Unowned account inventory"));
    assert!(
        snapshot
            .to_string()
            .contains("Unexpected broker position ownership")
    );
    assert!(cache.try_borrow_mut().is_ok());
}

// Run explicitly on an otherwise idle machine; setup and history growth are outside the timing
#[rstest]
#[ignore = "Performance diagnosis; run with --ignored --nocapture on an idle machine"]
fn exit_maintenance_does_not_scale_with_terminal_grid_orders() {
    use super::super::{engine::GridEngine, orders::OrderPhase, regime::MarketRegime};

    let measure = |history| {
        let (mut runtime, mut engine, _) = runtime(1);
        let grid = GridEngine::build(
            &engine.config.grid,
            1,
            dec!(100),
            dec!(0.02),
            dec!(10000),
            dec!(0.01),
            dec!(1),
            0,
        )
        .unwrap();
        let level = &grid.levels[0];
        for _ in 0..history {
            let order = engine
                .state
                .orders
                .entry("901-AAPL.SIM", 1, level, dec!(1), None, 0)
                .unwrap();
            engine
                .state
                .orders
                .transition(&order.id, OrderPhase::Cancelled, 0);
        }
        let entry = engine
            .state
            .orders
            .entry("901-AAPL.SIM", 1, level, dec!(1), None, 0)
            .unwrap();
        engine
            .state
            .orders
            .fill(
                &entry.id,
                "FILL-1",
                dec!(1),
                level.price,
                Decimal::ZERO,
                false,
                1,
            )
            .unwrap();
        let exit = engine
            .state
            .orders
            .exit("901-AAPL.SIM", &entry.id, None, 1)
            .unwrap();
        engine
            .state
            .orders
            .transition(&exit.id, OrderPhase::Accepted, 1);
        engine.state.regime.snapshot.initialized = true;
        engine.state.regime.snapshot.regime = MarketRegime::Range;
        runtime.recovering = false;
        // Same single filled lot and covering sell in both cases; only terminal history changes.
        let order_count = engine.state.orders.orders().len();
        let elapsed = (0..3)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..500 {
                    engine.exits(&mut runtime, 2, false).unwrap();
                }
                start.elapsed()
            })
            .min()
            .unwrap();
        assert_eq!(engine.state.orders.orders().len(), order_count);
        assert_eq!(engine.state.orders.inventory(), dec!(1));
        elapsed
    };
    let short = measure(1);
    let long = measure(3000);
    println!(
        "Exit maintenance (500 calls): 1 terminal order={short:?}, 3000 terminal orders={long:?}"
    );
    assert!(
        long <= short * 8 + Duration::from_millis(20),
        "Exit maintenance scans historical orders: {short:?} -> {long:?}"
    );
}

// Run explicitly on an otherwise idle machine; setup and history growth are outside the timing.
#[rstest]
#[ignore = "Performance diagnosis; run with --ignored --nocapture on an idle machine"]
fn nonterminal_order_updates_do_not_rescan_historical_cycles() {
    use super::super::{engine::GridEngine, orders::OrderPhase};

    let measure = |history| {
        let (_, mut engine, _) = runtime(1);
        let level = GridEngine::build(
            &engine.config.grid,
            1,
            dec!(100),
            dec!(0.02),
            dec!(10000),
            dec!(0.01),
            dec!(1),
            0,
        )
        .unwrap()
        .levels[0]
            .clone();
        for _ in 0..history {
            let order = engine
                .state
                .orders
                .entry("901-AAPL.SIM", 1, &level, dec!(1), None, 0)
                .unwrap();
            engine
                .state
                .orders
                .transition(&order.id, OrderPhase::Cancelled, 0);
        }
        let active = engine
            .state
            .orders
            .entry("901-AAPL.SIM", 1, &level, dec!(1), None, 0)
            .unwrap();
        black_box(engine.state.orders.active_ids());
        let elapsed = (0..3)
            .map(|_| {
                let start = Instant::now();
                for now in 1..=500 {
                    engine
                        .state
                        .orders
                        .transition(&active.id, OrderPhase::Accepted, now);
                    black_box(engine.state.orders.active_ids());
                }
                start.elapsed()
            })
            .min()
            .unwrap();
        assert_eq!(
            engine.state.orders.orders()[&active.id].phase,
            OrderPhase::Accepted
        );
        elapsed
    };
    let short = measure(1);
    let long = measure(3000);
    println!(
        "Nonterminal updates (500 calls): 1 historical lot={short:?}, 3000 historical lots={long:?}"
    );
    assert!(
        long <= short * 8 + Duration::from_millis(20),
        "Nonterminal order updates scan historical cycles: {short:?} -> {long:?}"
    );
}

#[rstest]
#[ignore = "Performance regression check; run with --ignored --nocapture"]
fn risk_reads_do_not_scale_with_account_history() {
    let measure = |history| {
        let (runtime, engine, _) = runtime(history);
        (0..3)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..16 {
                    black_box(engine.snapshot(&runtime, dec!(100)).unwrap());
                    black_box(runtime.account_capacity(&engine).unwrap());
                }
                start.elapsed()
            })
            .min()
            .unwrap()
    };
    let short = measure(1);
    let long = measure(10000);
    println!("Risk reads: 1 event={short:?}, 10000 events={long:?}");
    assert!(
        long <= short * 8 + Duration::from_millis(20),
        "Risk queries scale with historical account events: {short:?} -> {long:?}"
    );
}

#[rstest]
#[ignore = "Performance regression check; run with --ignored --nocapture"]
fn portfolio_monitoring_does_not_scale_with_terminal_grid_orders() {
    use super::super::{engine::GridEngine, orders::OrderPhase};

    let measure = |history| {
        let (mut runtime, mut engine, _) = runtime(1);
        let level = GridEngine::build(
            &engine.config.grid,
            1,
            dec!(100),
            dec!(0.02),
            dec!(10000),
            dec!(0.01),
            dec!(1),
            0,
        )
        .unwrap()
        .levels[0]
            .clone();
        for _ in 0..history {
            let order = engine
                .state
                .orders
                .entry("901-AAPL.SIM", 1, &level, dec!(1), None, 0)
                .unwrap();
            engine
                .state
                .orders
                .transition(&order.id, OrderPhase::Cancelled, 0);
        }
        engine.state.last_price = Some(dec!(100));
        runtime.engines.insert(engine.config.instrument_id, engine);
        runtime.recovering = false;
        (0..3)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..500 {
                    runtime.observe_portfolio(false).unwrap();
                }
                start.elapsed()
            })
            .min()
            .unwrap()
    };
    let short = measure(1);
    let long = measure(3000);
    println!("Portfolio monitor: 1 terminal order={short:?}, 3000 terminal orders={long:?}");
    assert!(
        long <= short * 8 + Duration::from_millis(20),
        "Idle portfolio monitoring scales with terminal grid history: {short:?} -> {long:?}"
    );
}
