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

use nautilus_model::{
    data::Bar,
    identifiers::{ClientOrderId, TradeId},
};

use super::*;
use crate::examples::strategies::dynamic_grid::{
    config::{GridEntryMode, PositionSizing, SpacingMode},
    orders::{OrderPhase, PositionComponent},
    strategy::StrategyState,
};

const MINUTE: u64 = 60_000_000_000;

fn ready() -> (
    MultiAssetGridStrategy,
    GridStrategyEngine,
    Rc<RefCell<Cache>>,
) {
    let (mut runtime, mut engine, cache) = runtime(1);
    runtime.recovering = false;
    runtime.engines.remove(&engine.config.instrument_id);
    engine.config.grid.entry_mode = GridEntryMode::Sequential;
    engine.config.grid.grid_levels = 5;
    engine.config.grid.spacing_mode = SpacingMode::Percentage;
    engine.config.grid.spacing_pct = dec!(0.02);
    engine.config.grid.enable_trend_filter = false;
    engine.config.grid.enable_volatility_filter = false;
    engine.config.grid.volatility_reset_ratio = dec!(100);
    engine.state.state = StrategyState::WaitingForRange;
    for minute in 1..=60 {
        let now = minute * MINUTE;
        engine
            .state
            .regime
            .update(
                &engine.config.grid,
                crate::examples::strategies::dynamic_grid::regime::Observation {
                    ts_ns: now,
                    high: 101.0,
                    low: 99.0,
                    close: 100.0,
                },
            )
            .unwrap();
    }
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time((60 * MINUTE).into(), true);
    (runtime, engine, cache)
}

#[rstest]
fn sequential_entries_submit_only_the_nearest_grid_buy() {
    let (mut runtime, mut engine, _) = ready();
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let buys: Vec<_> = engine
        .state
        .orders
        .orders()
        .values()
        .filter(|o| o.buy && o.component == PositionComponent::Grid)
        .collect();
    assert_eq!(buys.len(), 1, "Only one grid buy may reach execution");
    assert_eq!(buys[0].level, -1);
}

#[rstest]
fn sequential_diagnostics_count_first_entry_decision_per_bar_not_ticks() {
    let (mut runtime, mut engine, _) = ready();
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let id = engine.state.orders.active_ids()[0].clone();
    engine.control(&mut runtime, &id, OrderPhase::Accepted);
    close_bar(&mut runtime, &mut engine, dec!(99), 61 * MINUTE);
    for _ in 0..10 {
        engine.drive(&mut runtime, dec!(99), 61 * MINUTE).unwrap();
    }
    let diagnostic = serde_json::to_value(&engine.report.borrow().diagnostics).unwrap();
    assert_eq!(diagnostic["sequential_entries"]["SUBMITTED"]["count"], 1);
    assert_eq!(diagnostic["sequential_entries"]["WORKING_BUY"]["count"], 1);
    assert_eq!(engine.state.orders.orders().len(), 1);
}

fn fill_buy(
    engine: &mut GridStrategyEngine,
    cache: &Rc<RefCell<Cache>>,
    id: &str,
    quantity: Decimal,
    price: Decimal,
    now: u64,
) -> nautilus_model::events::OrderFilled {
    let fill = OrderFilledSpec::builder()
        .instrument_id(engine.config.instrument_id)
        .strategy_id(engine.config.base.strategy_id.unwrap())
        .client_order_id(ClientOrderId::from(id))
        .account_id(AccountId::from("SIM-001"))
        .position_id(PositionId::from("ENTRY-POSITION"))
        .trade_id(TradeId::from(format!("FILL-{now}")))
        .order_side(OrderSide::Buy)
        .last_qty(Quantity::from_decimal_dp(quantity, 0).unwrap())
        .last_px(Price::from_decimal_dp(price, 2).unwrap())
        .ts_event(now.into())
        .ts_init(now.into())
        .build();
    engine.apply_fill(&fill, now).unwrap();
    let position = cache
        .borrow()
        .positions_open(None, Some(&engine.config.instrument_id), None, None, None)
        .first()
        .map(|position| Position::clone(position));
    if let Some(mut position) = position {
        position.apply(&fill);
        cache.borrow_mut().update_position(&position).unwrap();
    } else {
        cache
            .borrow_mut()
            .add_position(
                &Position::new(engine.instrument.as_ref().unwrap(), fill.clone()),
                OmsType::Netting,
            )
            .unwrap();
    }
    fill
}

fn close_bar(
    runtime: &mut MultiAssetGridStrategy,
    engine: &mut GridStrategyEngine,
    price: Decimal,
    now: u64,
) {
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time(now.into(), true);
    let bar = Bar::new(
        engine.config.bar_type,
        Price::from_decimal_dp(price, 2).unwrap(),
        Price::from_decimal_dp(price + dec!(1), 2).unwrap(),
        Price::from_decimal_dp(price - dec!(1), 2).unwrap(),
        Price::from_decimal_dp(price, 2).unwrap(),
        Quantity::from(1000),
        now.into(),
        now.into(),
    );
    engine.completed_bar(runtime, &bar).unwrap();
}

fn accept_buy(
    runtime: &mut MultiAssetGridStrategy,
    engine: &mut GridStrategyEngine,
    cache: &Rc<RefCell<Cache>>,
    id: &str,
) {
    use nautilus_model::{
        events::{OrderEventAny, order::spec::OrderAcceptedSpec},
        orders::Order,
    };
    let id = ClientOrderId::from(id);
    let accepted = OrderAcceptedSpec::builder()
        .instrument_id(engine.config.instrument_id)
        .strategy_id(engine.config.base.strategy_id.unwrap())
        .client_order_id(id)
        .build();
    cache
        .borrow_mut()
        .order_mut(&id)
        .unwrap()
        .apply(OrderEventAny::Accepted(accepted))
        .unwrap();
    engine.control(runtime, id.as_str(), OrderPhase::Accepted);
}

#[rstest]
#[case::cancel_ack_only(None)]
#[case::partial_fill_during_cancel(Some(false))]
#[case::full_fill_during_cancel(Some(true))]
fn sequential_requote_waits_for_two_closes_then_cancel_ack_and_a_new_bar(
    #[case] late_fill: Option<bool>,
) {
    use nautilus_common::{
        messages::execution::TradingCommand,
        msgbus::{self, TypedIntoHandler, switchboard::MessagingSwitchboard},
    };
    let (mut runtime, mut engine, cache) = ready();
    let mut config = serde_json::to_value(&engine.config.grid).unwrap();
    config["sequential_requote_bars"] = 2.into();
    engine.config.grid = serde_json::from_value(config).unwrap();
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let buy = engine
        .state
        .orders
        .orders()
        .values()
        .next()
        .unwrap()
        .clone();
    fill_buy(
        &mut engine,
        &cache,
        &buy.id,
        buy.quantity,
        dec!(93),
        60 * MINUTE,
    );
    engine.exits(&mut runtime, 60 * MINUTE, false).unwrap();
    for id in engine.state.orders.active_ids() {
        engine.control(&mut runtime, &id, OrderPhase::Accepted);
    }
    close_bar(&mut runtime, &mut engine, dec!(93), 61 * MINUTE);
    let far = engine
        .state
        .orders
        .orders()
        .values()
        .find(|o| o.buy && !o.phase.terminal())
        .unwrap()
        .clone();
    assert_eq!(far.level, -4);
    accept_buy(&mut runtime, &mut engine, &cache, &far.id);
    let commands = Rc::new(RefCell::new(Vec::new()));
    let received = Rc::clone(&commands);
    msgbus::register_trading_command_endpoint(
        MessagingSwitchboard::exec_engine_queue_execute(),
        TypedIntoHandler::from(move |command: TradingCommand| received.borrow_mut().push(command)),
    );
    close_bar(&mut runtime, &mut engine, dec!(97), 62 * MINUTE);
    for _ in 0..5 {
        engine.drive(&mut runtime, dec!(97), 62 * MINUTE).unwrap();
    }
    assert_eq!(
        engine.state.orders.orders()[&far.id].phase,
        OrderPhase::Accepted
    );
    let reserved = engine
        .state
        .orders
        .component_reservations(PositionComponent::Grid)
        .0;
    close_bar(&mut runtime, &mut engine, dec!(97), 63 * MINUTE);
    assert_eq!(
        engine.state.orders.orders()[&far.id].phase,
        OrderPhase::CancelPending
    );
    assert_eq!(
        engine
            .state
            .orders
            .component_reservations(PositionComponent::Grid)
            .0,
        reserved
    );
    assert_eq!(
        commands
            .borrow()
            .iter()
            .filter(|c| matches!(c, TradingCommand::CancelOrder(_)))
            .count(),
        1
    );
    if let Some(full) = late_fill {
        let quantity = if full { far.quantity } else { dec!(1) };
        let fill = fill_buy(
            &mut engine,
            &cache,
            &far.id,
            quantity,
            dec!(92),
            63 * MINUTE + 1,
        );
        engine.apply_fill(&fill, 63 * MINUTE + 2).unwrap();
        engine.exits(&mut runtime, 63 * MINUTE + 2, false).unwrap();
        let covered: Decimal = engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| !o.buy && o.lot_id == far.id && !o.phase.terminal())
            .map(|o| o.quantity - o.filled)
            .sum();
        assert_eq!(
            covered, quantity,
            "Late fills remain covered, duplicates do not add inventory"
        );
        // 本场景确认新覆盖卖单；不把缺少卖单回报导致的正常超时停买误当作换层失败
        for id in engine.state.orders.active_ids() {
            if !engine.state.orders.orders()[&id].buy {
                engine.control(&mut runtime, &id, OrderPhase::Accepted);
            }
        }
    }
    engine.control(&mut runtime, &far.id, OrderPhase::Cancelled);
    engine.drive(&mut runtime, dec!(97), 63 * MINUTE).unwrap();
    assert_eq!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| o.buy)
            .count(),
        2
    );
    close_bar(&mut runtime, &mut engine, dec!(97), 64 * MINUTE);
    let next = engine
        .state
        .orders
        .orders()
        .values()
        .find(|o| o.buy && !o.phase.terminal())
        .unwrap();
    assert_eq!(next.level, -2);
    assert_ne!(next.id, far.id);
    assert_eq!(engine.state.grid.as_ref().unwrap().center, dec!(100));
    engine.state.orders.validate().unwrap();
}

#[rstest]
#[case(dec!(0))]
#[case(dec!(0.5))]
fn sequential_entries_gap_fill_waits_for_a_new_bar_and_skips_crossed_levels(
    #[case] seed_fraction: Decimal,
) {
    let (mut runtime, mut engine, cache) = ready();
    engine.config.grid.initial_inventory_fraction = seed_fraction;
    let now = 60 * MINUTE;
    engine.drive(&mut runtime, dec!(100), now).unwrap();
    let buy = engine
        .state
        .orders
        .orders()
        .values()
        .find(|o| o.buy)
        .unwrap()
        .clone();
    let fill = fill_buy(
        &mut engine,
        &cache,
        &buy.id,
        buy.quantity,
        dec!(93),
        now + 5_000_000_000,
    );
    engine.apply_fill(&fill, now + 30_000_000_000).unwrap();
    assert_eq!(
        engine.state.grid_entry_after_ns,
        Some(now + 5_000_000_000),
        "Duplicate fill must not extend pacing"
    );
    assert_eq!(engine.state.orders.inventory(), buy.quantity);
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time((now + 30_000_000_000).into(), true);
    engine
        .drive(&mut runtime, dec!(93), now + 30_000_000_000)
        .unwrap();
    assert_eq!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| o.buy)
            .count(),
        1,
        "Fill/next tick must not chain another buy from the same completed bar"
    );
    assert!(
        engine
            .state
            .orders
            .orders()
            .values()
            .any(|o| !o.buy && o.lot_id == buy.id),
        "Entry pacing must not delay covered exits"
    );
    for id in engine.state.orders.active_ids() {
        engine.control(&mut runtime, &id, OrderPhase::Accepted);
    }
    close_bar(&mut runtime, &mut engine, dec!(93), now + MINUTE);
    let buys: Vec<_> = engine
        .state
        .orders
        .orders()
        .values()
        .filter(|o| o.buy)
        .collect();
    assert_eq!(buys.len(), 2);
    let next = buys.iter().find(|o| o.id != buy.id).unwrap();
    assert_eq!(next.level, -4);
    assert!(next.limit.unwrap() < dec!(93));
    assert_eq!(engine.state.grid.as_ref().unwrap().center, dec!(100));
}

#[rstest]
#[case(OrderPhase::Cancelled)]
#[case(OrderPhase::Expired)]
fn sequential_entries_terminal_report_does_not_rearm_in_the_same_bar(#[case] phase: OrderPhase) {
    let (mut runtime, mut engine, _) = ready();
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let id = engine.state.orders.active_ids()[0].clone();
    engine.control(&mut runtime, &id, OrderPhase::Accepted);
    close_bar(&mut runtime, &mut engine, dec!(100), 61 * MINUTE);
    engine.control(&mut runtime, &id, phase);
    engine.drive(&mut runtime, dec!(100), 61 * MINUTE).unwrap();
    assert_eq!(
        engine.state.orders.orders().len(),
        1,
        "Terminal report cannot immediately recycle the buy slot"
    );
    close_bar(&mut runtime, &mut engine, dec!(100), 62 * MINUTE);
    assert_eq!(engine.state.orders.orders().len(), 2);
}

#[rstest]
#[case(OrderPhase::Accepted)]
#[case(OrderPhase::PartiallyFilled)]
#[case(OrderPhase::CancelPending)]
#[case(OrderPhase::Unknown)]
fn sequential_entries_unresolved_buy_keeps_its_slot(#[case] phase: OrderPhase) {
    let (mut runtime, mut engine, cache) = ready();
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let buy = engine
        .state
        .orders
        .orders()
        .values()
        .next()
        .unwrap()
        .clone();
    if phase == OrderPhase::PartiallyFilled {
        assert!(buy.quantity > dec!(1));
        fill_buy(&mut engine, &cache, &buy.id, dec!(1), dec!(98), 60 * MINUTE);
    }
    engine.control(&mut runtime, &buy.id, phase);
    close_bar(&mut runtime, &mut engine, dec!(99), 61 * MINUTE);
    assert_eq!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| o.buy)
            .count(),
        1
    );
    assert!(
        engine
            .state
            .orders
            .component_reservations(PositionComponent::Grid)
            .0
            > Decimal::ZERO
    );
}

#[rstest]
#[case(PositionSizing::Equal)]
#[case(PositionSizing::EqualLots)]
fn sequential_entries_checkpoint_retains_wait_and_rejects_missing_wait(
    #[case] sizing: PositionSizing,
) {
    use nautilus_common::actor::DataActor;
    let (mut runtime, mut engine, _) = ready();
    engine.config.grid.position_sizing = sizing;
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let id = engine.config.instrument_id;
    runtime.config.instruments.get_mut(&id).unwrap().grid = engine.config.grid.clone();
    runtime.engines.insert(id, engine);
    let mut restored = MultiAssetGridStrategy::new(runtime.config.clone()).unwrap();
    restored.on_load(runtime.on_save().unwrap()).unwrap();
    assert_eq!(
        restored.engines[&id].state.grid_entry_after_ns,
        Some(60 * MINUTE)
    );
    assert_eq!(
        serde_json::to_value(&restored.engines[&id].state.grid).unwrap(),
        serde_json::to_value(&runtime.engines[&id].state.grid).unwrap(),
    );
    let mut corrupt: super::super::Checkpoint =
        serde_json::from_value(serde_json::to_value(runtime.checkpoint(None, None)).unwrap())
            .unwrap();
    corrupt
        .instruments
        .get_mut(&id)
        .unwrap()
        .state
        .grid_entry_after_ns = None;
    assert!(
        corrupt.validate(&runtime.config).is_err(),
        "Lost entry pacing must not silently resume"
    );
}

#[rstest]
#[case::partial(OrderPhase::PartiallyFilled)]
#[case::unknown(OrderPhase::Unknown)]
#[case::cancel_pending(OrderPhase::CancelPending)]
fn sequential_requote_never_replaces_unresolved_or_partial_buys(#[case] phase: OrderPhase) {
    let (mut runtime, mut engine, cache) = ready();
    engine.config.grid.sequential_requote_bars = Some(2);
    engine.config.grid.order_timeout_secs = 600;
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let first = engine
        .state
        .orders
        .orders()
        .values()
        .next()
        .unwrap()
        .clone();
    fill_buy(
        &mut engine,
        &cache,
        &first.id,
        first.quantity,
        dec!(93),
        60 * MINUTE,
    );
    engine.exits(&mut runtime, 60 * MINUTE, false).unwrap();
    for id in engine.state.orders.active_ids() {
        engine.control(&mut runtime, &id, OrderPhase::Accepted);
    }
    close_bar(&mut runtime, &mut engine, dec!(93), 61 * MINUTE);
    let far = engine
        .state
        .orders
        .orders()
        .values()
        .find(|o| o.buy && !o.phase.terminal())
        .unwrap()
        .clone();
    accept_buy(&mut runtime, &mut engine, &cache, &far.id);
    if phase == OrderPhase::PartiallyFilled {
        fill_buy(&mut engine, &cache, &far.id, dec!(1), dec!(92), 61 * MINUTE);
    } else {
        engine.control(&mut runtime, &far.id, phase);
    }
    close_bar(&mut runtime, &mut engine, dec!(97), 62 * MINUTE);
    close_bar(&mut runtime, &mut engine, dec!(97), 63 * MINUTE);
    assert_eq!(engine.state.orders.orders()[&far.id].phase, phase);
    assert!(
        !engine
            .report
            .borrow()
            .diagnostics
            .cancellations
            .iter()
            .any(|c| c.reason == "SEQUENTIAL_REQUOTE")
    );
    assert_eq!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| o.buy)
            .count(),
        2
    );
}

#[rstest]
fn sequential_requote_cancel_timeout_keeps_reservations_and_stops_entries() {
    let (mut runtime, mut engine, cache) = ready();
    engine.config.grid.sequential_requote_bars = Some(2);
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let id = engine.state.orders.active_ids()[0].clone();
    accept_buy(&mut runtime, &mut engine, &cache, &id);
    let reserved = engine
        .state
        .orders
        .buy_reservations(&engine.config.grid, dec!(100));
    engine
        .cancel_ids(&mut runtime, vec![id.clone()], "SEQUENTIAL_REQUOTE")
        .unwrap();
    close_bar(&mut runtime, &mut engine, dec!(100), 61 * MINUTE);

    assert_eq!(engine.state.state, StrategyState::RiskOff);
    assert_eq!(
        engine.state.orders.orders()[&id].phase,
        OrderPhase::CancelPending
    );
    assert_eq!(
        engine
            .state
            .orders
            .buy_reservations(&engine.config.grid, dec!(100)),
        reserved
    );
    assert_eq!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| o.buy)
            .count(),
        1
    );
    assert_eq!(
        engine.state.risk.risk_off_reason.as_deref(),
        Some("Unknown order outcome or cancellation timeout")
    );
}

#[rstest]
#[case::candidate_changes(false, 63, dec!(95))]
#[case::restart(true, 63, dec!(97))]
#[case::missing_bar(false, 64, dec!(97))]
fn sequential_requote_confirmation_is_not_carried_to_a_different_candidate_or_restart(
    #[case] restart: bool,
    #[case] minute: u64,
    #[case] price: Decimal,
) {
    let (mut runtime, mut engine, cache) = ready();
    engine.config.grid.sequential_requote_bars = Some(2);
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let first = engine
        .state
        .orders
        .orders()
        .values()
        .next()
        .unwrap()
        .clone();
    fill_buy(
        &mut engine,
        &cache,
        &first.id,
        first.quantity,
        dec!(93),
        60 * MINUTE,
    );
    engine.exits(&mut runtime, 60 * MINUTE, false).unwrap();
    for id in engine.state.orders.active_ids() {
        engine.control(&mut runtime, &id, OrderPhase::Accepted);
    }
    close_bar(&mut runtime, &mut engine, dec!(93), 61 * MINUTE);
    let far = engine
        .state
        .orders
        .orders()
        .values()
        .find(|o| o.buy && !o.phase.terminal())
        .unwrap()
        .clone();
    accept_buy(&mut runtime, &mut engine, &cache, &far.id);
    close_bar(&mut runtime, &mut engine, dec!(97), 62 * MINUTE);
    if restart {
        let saved = serde_json::to_value(&engine.state).unwrap();
        let mut restored = GridStrategyEngine::new(engine.config.clone());
        restored.instrument = engine.instrument.clone();
        restored.state = serde_json::from_value(saved).unwrap();
        restored
            .state
            .validate_signal(&restored.config.grid, restored.config.bar_type)
            .unwrap();
        engine = restored;
    }
    close_bar(&mut runtime, &mut engine, price, minute * MINUTE);
    assert_eq!(
        engine.state.orders.orders()[&far.id].phase,
        OrderPhase::Accepted
    );
}

#[rstest]
#[case::nearer(dec!(100), dec!(93), dec!(0), dec!(94.23))]
#[case::small_improvement(dec!(100), dec!(98), dec!(0), dec!(100))]
#[case::entry_fees(dec!(100), dec!(93), dec!(4), dec!(96.11))]
#[case::rounding_is_not_a_grid_step(dec!(100.001), dec!(98.04), dec!(0), dec!(100.01))]
fn sequential_fill_cost_exit_accounts_for_actual_entry_fees(
    #[case] center: Decimal,
    #[case] price: Decimal,
    #[case] fee: Decimal,
    #[case] target: Decimal,
) {
    use crate::examples::strategies::dynamic_grid::{engine::GridEngine, orders::OrderManager};
    let (_, engine, _) = ready();
    let grid = GridEngine::build(
        &engine.config.grid,
        1,
        center,
        dec!(0.02),
        dec!(10000),
        dec!(0.01),
        dec!(1),
        0,
    )
    .unwrap();
    let level = grid.levels.iter().find(|l| l.level_index == -1).unwrap();
    let mut book = OrderManager::new(dec!(10000));
    let buy = book.entry("cost", 1, level, dec!(2), None, 0).unwrap();
    book.fill(&buy.id, "fill", dec!(2), price, fee, false, 1)
        .unwrap();
    book.adapt_exit_target(&buy.id, &grid, &engine.config.grid);
    let sell = book.exit("cost", &buy.id, None, 2).unwrap();
    assert_eq!(sell.limit, Some(target));
    let snapshot = serde_json::to_value(&book).unwrap();
    let recovered: OrderManager = serde_json::from_value(snapshot).unwrap();
    recovered.validate().unwrap();
    assert_eq!(recovered.lots()[&buy.id].target, target);
}

#[rstest]
#[case::core(PositionComponent::Core, false, false)]
#[case::seed(PositionComponent::Grid, true, false)]
#[case::old_grid(PositionComponent::Grid, false, true)]
fn sequential_fill_cost_exit_keeps_core_seed_and_old_grid_targets(
    #[case] component: PositionComponent,
    #[case] seed: bool,
    #[case] old_grid: bool,
) {
    use crate::examples::strategies::dynamic_grid::{engine::GridEngine, orders::OrderManager};
    let (_, engine, _) = ready();
    let mut grid = GridEngine::build(
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
    let level = grid.levels.iter().find(|l| l.level_index == -1).unwrap();
    let mut book = OrderManager::new(dec!(10000));
    let buy = book
        .entry_component(
            "cost",
            1,
            level,
            dec!(2),
            seed.then_some(dec!(98)),
            component,
            0,
        )
        .unwrap();
    book.fill(&buy.id, "fill", dec!(2), dec!(93), dec!(0), false, 1)
        .unwrap();
    if old_grid {
        grid.grid_id = 2;
    }
    assert_eq!(
        book.adapt_exit_target(&buy.id, &grid, &engine.config.grid),
        None
    );
    assert_eq!(book.lots()[&buy.id].target, dec!(100));
}

#[rstest]
fn sequential_fill_cost_exit_does_not_wait_for_or_reprice_partial_coverage() {
    let (mut runtime, mut engine, cache) = ready();
    engine.config.grid.fill_cost_exits = true;
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let buy = engine
        .state
        .orders
        .orders()
        .values()
        .next()
        .unwrap()
        .clone();
    fill_buy(
        &mut engine,
        &cache,
        &buy.id,
        dec!(1),
        dec!(93),
        60 * MINUTE + 1,
    );
    engine.exits(&mut runtime, 60 * MINUTE + 1, false).unwrap();
    assert!(
        engine
            .state
            .orders
            .orders()
            .values()
            .any(|o| !o.buy && o.quantity == dec!(1) && o.limit == Some(dec!(100)))
    );
    fill_buy(
        &mut engine,
        &cache,
        &buy.id,
        buy.quantity - dec!(1),
        dec!(93),
        60 * MINUTE + 2,
    );
    engine.exits(&mut runtime, 60 * MINUTE + 2, false).unwrap();
    assert!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|o| !o.buy)
            .all(|o| o.limit == Some(dec!(100)))
    );
    assert_eq!(
        engine
            .state
            .orders
            .component_reservations(PositionComponent::Grid)
            .1,
        buy.quantity
    );
    assert!(
        engine
            .report
            .borrow()
            .diagnostics
            .exit_target_changes
            .is_empty()
    );
}

#[rstest]
fn sequential_fill_cost_exit_uses_a_nearer_profitable_frozen_level() {
    let (mut runtime, mut engine, cache) = ready();
    let mut config = serde_json::to_value(&engine.config.grid).unwrap();
    config["fill_cost_exits"] = true.into();
    engine.config.grid = serde_json::from_value(config).unwrap();
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let buy = engine
        .state
        .orders
        .orders()
        .values()
        .next()
        .unwrap()
        .clone();
    let fill = fill_buy(
        &mut engine,
        &cache,
        &buy.id,
        buy.quantity,
        dec!(93),
        60 * MINUTE,
    );
    engine.apply_fill(&fill, 60 * MINUTE).unwrap();
    engine.exits(&mut runtime, 60 * MINUTE, false).unwrap();
    let sell = engine
        .state
        .orders
        .orders()
        .values()
        .find(|o| !o.buy)
        .unwrap();
    assert_eq!(sell.limit, Some(dec!(94.23)));
    assert_eq!(sell.quantity, buy.quantity);
    assert_eq!(engine.state.orders.lots()[&buy.id].target, dec!(94.23));
    engine.state.orders.validate().unwrap();
}

#[rstest]
fn sequential_entries_recovery_rejects_future_wait() {
    let (runtime, mut engine, _) = ready();
    engine.state.grid_entry_after_ns = Some(61 * MINUTE);
    assert!(engine.recover(&runtime).is_err());
}

#[rstest]
#[case(GridEntryMode::AllLevels, 5)]
#[case(GridEntryMode::Sequential, 1)]
fn sequential_entries_seed_inventory_is_paced_too(
    #[case] mode: GridEntryMode,
    #[case] count: usize,
) {
    let (mut runtime, mut engine, _) = ready();
    engine.config.grid.entry_mode = mode;
    engine.config.grid.initial_inventory_fraction = dec!(0.5);
    engine.drive(&mut runtime, dec!(100), 60 * MINUTE).unwrap();
    let buys: Vec<_> = engine
        .state
        .orders
        .orders()
        .values()
        .filter(|o| o.buy && o.limit.is_none())
        .collect();
    assert_eq!(buys.len(), count);
    assert!(buys.iter().all(|o| o.limit.is_none()));
}

#[rstest]
#[case::full_rebound(dec!(100), false, 1, 3)]
#[case::limited_rebound(dec!(95), false, 0, 0)]
#[case::cost_aware_rebound(dec!(95), true, 1, 0)]
fn sequential_entries_native_gap_and_rebound_fills_only_one_level(
    #[case] rebound: Decimal,
    #[case] fill_cost: bool,
    #[case] sequential_cycles: usize,
    #[case] all_cycles: usize,
) {
    use nautilus_backtest::dynamic_grid::{Benchmark, GridBacktestConfig, run_grid_backtest};
    let (_, mut engine, _) = ready();
    engine.config.grid.fill_cost_exits = fill_cost;
    let mut config = GridBacktestConfig::default();
    config.grid =
        serde_json::from_value(serde_json::to_value(&engine.config.grid).unwrap()).unwrap();
    config.slippage_probability = 0.0;
    let mut prices = vec![dec!(100); 60];
    prices.extend([dec!(93), rebound, rebound]);
    let bars: Vec<_> = prices
        .into_iter()
        .enumerate()
        .map(|(index, price)| {
            let ts = 1_735_828_200_000_000_000 + (index as u64 + 1) * MINUTE;
            let price = Price::from_decimal_dp(price, 2).unwrap();
            // Gap Bar 的 OHLC 全为 93，不虚构 100 到 93 之间可逐层成交的路径。
            Bar::new(
                config.bar_type().unwrap(),
                price,
                price,
                price,
                price,
                Quantity::from(10000),
                ts.into(),
                ts.into(),
            )
        })
        .collect();
    let sequential = run_grid_backtest(&bars, &[], &config, Benchmark::Dynamic).unwrap();
    config.grid = serde_json::from_value({
        let mut grid = serde_json::to_value(&config.grid).unwrap();
        grid["entry_mode"] = "AllLevels".into();
        grid["fill_cost_exits"] = false.into();
        grid
    })
    .unwrap();
    let all = run_grid_backtest(&bars, &[], &config, Benchmark::Dynamic).unwrap();
    assert!(
        sequential.risk_off_reason.is_none(),
        "{:?}",
        sequential.risk_off_reason
    );
    assert!(all.risk_off_reason.is_none(), "{:?}", all.risk_off_reason);
    assert_eq!(sequential.metrics.number_of_grid_cycles, sequential_cycles);
    assert_eq!(all.metrics.number_of_grid_cycles, all_cycles);
    assert!(sequential.metrics.maximum_position < all.metrics.maximum_position);
    assert!(sequential.metrics.fees > Decimal::ZERO);
}
