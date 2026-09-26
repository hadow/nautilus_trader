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

#[path = "entry_tests.rs"]
mod entries;

fn isolated_runtime() -> (MultiAssetGridStrategy, Rc<RefCell<Cache>>, InstrumentId) {
    let (original, engine, cache) = runtime(1);
    let id = InstrumentId::from("MSFT.SIM");
    let mut config = original.config.clone();
    let mut external = config.instruments[&engine.config.instrument_id].clone();
    external.bar_type = "MSFT.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap();
    external.capital_allocation = dec!(0.4);
    external.sector = Some("Technology".into());
    config
        .instruments
        .get_mut(&engine.config.instrument_id)
        .unwrap()
        .capital_allocation = dec!(0.4);
    config
        .instruments
        .get_mut(&engine.config.instrument_id)
        .unwrap()
        .sector = external.sector.clone();
    config.instruments.insert(id, external);
    config.isolated_instruments.insert(id);
    let instrument = InstrumentAny::Equity(
        Equity::builder()
            .instrument_id(id)
            .raw_symbol(Symbol::from("MSFT"))
            .currency(Currency::USD())
            .price_precision(2)
            .price_increment(Price::from("0.01"))
            .lot_size(Quantity::from(1))
            .ts_event(0.into())
            .ts_init(0.into())
            .build()
            .unwrap(),
    );
    cache
        .borrow_mut()
        .add_instrument(instrument.clone())
        .unwrap();
    let fill = OrderFilledSpec::builder()
        .instrument_id(id)
        .strategy_id(StrategyId::from("EXTERNAL"))
        .account_id(AccountId::from("SIM-001"))
        .position_id(PositionId::from("MSFT.SIM-EXTERNAL"))
        .order_side(OrderSide::Buy)
        .last_qty(Quantity::from(200))
        .last_px(Price::from("80.00"))
        .build();
    cache
        .borrow_mut()
        .add_position(&Position::new(&instrument, fill), OmsType::Netting)
        .unwrap();
    let mut strategy = MultiAssetGridStrategy::new(config).unwrap();
    strategy.core = original.core;
    strategy.core.clock_mut().register_default_handler(
        nautilus_common::timer::TimeEventCallback::from(|_: nautilus_common::timer::TimeEvent| {}),
    );
    (strategy, cache, id)
}

fn external_quote(
    strategy: &mut MultiAssetGridStrategy,
    cache: &Rc<RefCell<Cache>>,
    id: InstrumentId,
    price: &str,
    ts: u64,
) {
    use nautilus_common::actor::DataActor;
    use nautilus_model::data::QuoteTick;
    let quote = QuoteTick::new(
        id,
        Price::from(price),
        Price::from(price),
        Quantity::from(10),
        Quantity::from(10),
        ts.into(),
        ts.into(),
    );
    strategy
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time(ts.into(), true);
    cache.borrow_mut().add_quote(quote).unwrap();
    strategy.on_quote(&quote).unwrap();
}

#[rstest]
fn isolated_inventory_is_risk_only_and_never_generates_orders() {
    use nautilus_common::{
        actor::DataActor,
        messages::execution::TradingCommand,
        msgbus::{self, TypedIntoHandler, switchboard::MessagingSwitchboard},
    };
    let (mut strategy, cache, id) = isolated_runtime();
    let commands = Rc::new(RefCell::new(Vec::new()));
    let received = Rc::clone(&commands);
    msgbus::register_trading_command_endpoint(
        MessagingSwitchboard::risk_engine_queue_execute(),
        TypedIntoHandler::from(move |command: TradingCommand| received.borrow_mut().push(command)),
    );
    strategy.on_start().unwrap();
    assert!(!strategy.external_ready(1));
    external_quote(&mut strategy, &cache, id, "100.00", 100_000_000_000);
    let views = strategy.exposures(None);
    let external = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(external.exposure, dec!(20000));
    assert_eq!(external.net_pnl, dec!(0)); // 旧持仓成本 80，不是本策略启动风险基线。
    assert!(!strategy.engines.contains_key(&id));
    assert!(strategy.external_ready(100_000_000_000));
    assert!(!strategy.external_ready(1_000_000_000_000));
    external_quote(&mut strategy, &cache, id, "90.00", 101_000_000_000);
    assert_eq!(strategy.portfolio_risk.last_equity, dec!(98000));
    assert_eq!(
        strategy.portfolio_performance.equity.last().unwrap().equity,
        dec!(100000)
    );
    strategy.kill_switch("test isolation").unwrap();
    strategy.on_stop().unwrap();
    assert!(commands.borrow().is_empty());
    assert_eq!(
        cache
            .borrow()
            .positions_open(None, Some(&id), None, None, None)[0]
            .quantity
            .as_decimal(),
        dec!(200)
    );
    assert!(strategy.report.borrow().instruments.get(&id).is_none());
    assert!(
        cache
            .borrow()
            .orders(None, None, None, None, None)
            .is_empty()
    );
}

#[rstest]
#[case::total("total")]
#[case::sector("sector")]
#[case::correlation("correlation")]
fn isolated_inventory_consumes_shared_risk_capacity(#[case] limit: &str) {
    use nautilus_common::actor::DataActor;
    let (mut strategy, cache, id) = isolated_runtime();
    match limit {
        "total" => strategy.config.portfolio.max_total_exposure = dec!(0.21),
        "sector" => strategy.config.portfolio.max_sector_exposure = dec!(0.21),
        _ => strategy.config.portfolio.max_correlated_exposure = dec!(0.21),
    }
    strategy.on_start().unwrap();
    external_quote(&mut strategy, &cache, id, "100.00", 100_000_000_000);
    let tradable = InstrumentId::from("AAPL.SIM");
    let allowed = strategy
        .with_engine(tradable, |engine, runtime| {
            runtime.buy_quantity(engine, dec!(100), dec!(100), Some(dec!(100)), false)
        })
        .unwrap();
    assert!(allowed > dec!(0) && allowed <= dec!(10));
    let views = strategy.exposures(None);
    assert_eq!(views.iter().map(|v| v.cash_delta).sum::<Decimal>(), dec!(0));
}

#[rstest]
fn isolated_checkpoint_keeps_baseline_but_requires_new_prices_and_matching_broker_quantity() {
    use nautilus_common::actor::DataActor;
    let (mut strategy, cache, id) = isolated_runtime();
    strategy.on_start().unwrap();
    external_quote(&mut strategy, &cache, id, "100.00", 100_000_000_000);
    external_quote(&mut strategy, &cache, id, "200.00", 101_000_000_000);
    let saved = strategy.on_save().unwrap();
    let (mut restored, restored_cache, _) = isolated_runtime();
    restored.on_load(saved.clone()).unwrap();
    restored.on_start().unwrap();
    assert!(!restored.external_ready(100_000_000_000));
    assert_eq!(restored.portfolio_risk.last_equity, dec!(120000));
    assert!(restored.portfolio_risk.risk_off_reason.is_none());
    assert_eq!(
        restored.external_positions[&id].reference_price,
        Some(dec!(100))
    );
    assert_eq!(restored.external_positions[&id].quantity, dec!(200));
    let mut position = restored_cache
        .borrow()
        .positions_open(None, Some(&id), None, None, None)[0]
        .clone();
    position.apply(
        &OrderFilledSpec::builder()
            .instrument_id(id)
            .strategy_id(StrategyId::from("EXTERNAL"))
            .account_id(AccountId::from("SIM-001"))
            .position_id(position.id)
            .trade_id(nautilus_model::identifiers::TradeId::from("MANUAL-CHANGE"))
            .order_side(OrderSide::Buy)
            .last_qty(Quantity::from(1))
            .last_px(Price::from("100.00"))
            .build(),
    );
    restored_cache
        .borrow_mut()
        .update_position(&position)
        .unwrap();
    assert!(
        restored
            .on_start()
            .unwrap_err()
            .to_string()
            .contains("Isolated inventory changed")
    );
    let engine = restored.engines.values().next().unwrap();
    assert!(restored.account_capacity(engine).is_err());
    let mut changed_config = strategy.config.clone();
    changed_config.isolated_instruments.clear();
    let mut changed = MultiAssetGridStrategy::new(changed_config).unwrap();
    assert!(changed.on_load(saved).is_err()); // 不允许重启时静默接管原隔离库存。
}

#[rstest]
fn isolated_config_rejects_claims_unknown_symbols_and_empty_tradable_universe() {
    let (strategy, _, id) = isolated_runtime();
    let mut config = strategy.config.clone();
    config.base.external_order_claims = Some(vec![id]);
    assert!(config.validate().is_err());
    config.base.external_order_claims = None;
    config
        .isolated_instruments
        .insert(InstrumentId::from("UNKNOWN.SIM"));
    assert!(config.validate().is_err());
    config.isolated_instruments = config.instruments.keys().copied().collect();
    assert!(config.validate().is_err());
}

#[rstest]
#[case::native_locks(true, dec!(100000))]
#[case::broker_snapshot(false, dec!(94000))]
fn cash_capacity_only_credits_proven_native_reservations(
    #[case] calculate: bool,
    #[case] expected: Decimal,
) {
    use super::super::engine::GridEngine;

    let (runtime, mut engine, cache) = runtime(1);
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
    engine.state.last_price = Some(dec!(100));
    engine
        .state
        .orders
        .entry("cash-test", 1, &grid.levels[0], dec!(70), None, 1)
        .unwrap();
    let mut account = CashAccount::new(account_event(dec!(100000), dec!(0), 1), calculate, false);
    account.update_balance_locked(engine.config.instrument_id, Money::from("6000 USD"));
    cache
        .borrow_mut()
        .update_account_owned(AccountAny::Cash(account))
        .unwrap();

    assert_eq!(
        engine.snapshot(&runtime, dec!(100)).unwrap().account_free,
        expected
    );
    assert_eq!(runtime.account_capacity(&engine).unwrap().0, expected);
}

#[rstest]
fn regime_clocks_are_independent_and_recover_through_native_bar_routing(
    #[values(false, true)] grid_scale: bool,
) {
    use nautilus_common::actor::DataActor;
    use nautilus_model::data::Bar;

    use super::super::{
        config::StrategyMode,
        regime::{Observation, RegimeDetector},
    };

    let (mut runtime, original, cache) = runtime(1);
    let aapl = original.config.instrument_id;
    let msft = InstrumentId::from("MSFT.SIM");
    let mut second = runtime.config.instruments[&aapl].clone();
    second.bar_type = "MSFT.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap();
    runtime.config.instruments.insert(msft, second);
    for (id, c) in &mut runtime.config.instruments {
        c.capital_allocation = dec!(0.4);
        c.grid.strategy_mode = StrategyMode::StockAdaptive;
        c.grid.grid_scale_regime =
            grid_scale.then(super::super::grid_scale::GridScaleConfig::default);
        let mut config = DynamicGridConfig::new(*id, c.bar_type);
        config.grid = c.grid.clone();
        config.grid.capital = runtime.config.portfolio.capital * c.capital_allocation;
        let mut engine = GridStrategyEngine::new(config);
        let instrument = InstrumentAny::Equity(
            Equity::builder()
                .instrument_id(*id)
                .raw_symbol(Symbol::from(id.symbol.as_str()))
                .currency(Currency::USD())
                .price_precision(2)
                .price_increment(Price::from("0.01"))
                .lot_size(Quantity::from(1))
                .ts_event(0.into())
                .ts_init(0.into())
                .build()
                .unwrap(),
        );
        cache
            .borrow_mut()
            .add_instrument(instrument.clone())
            .unwrap();
        engine.instrument = Some(instrument);
        runtime.engines.insert(*id, engine);
    }
    runtime.config.validate().unwrap();
    let open = 1_735_828_200_000_000_000_u64;
    let mut fast = RegimeDetector::default();
    for minute in 1..=44_u64 {
        let now = open + minute * 60_000_000_000;
        runtime
            .core
            .clock_mut()
            .as_any_mut()
            .downcast_mut::<TestClock>()
            .unwrap()
            .advance_time(now.into(), true);
        for id in [aapl, msft] {
            // MSFT 故意只收到第一桶，不能借 AAPL 的分钟事件推进它的慢时钟。
            if id == msft && minute > 15 {
                continue;
            }
            let price = if id == aapl { "100.00" } else { "200.00" };
            let bar = Bar::new(
                runtime.config.instruments[&id].bar_type,
                Price::from(price),
                Price::from(price),
                Price::from(price),
                Price::from(price),
                Quantity::from(100),
                now.into(),
                now.into(),
            );
            runtime.on_bar(&bar).unwrap();
            if id == aapl {
                fast.update(
                    &runtime.config.instruments[&id].grid,
                    Observation {
                        ts_ns: now,
                        high: 100.0,
                        low: 100.0,
                        close: 100.0,
                    },
                )
                .unwrap();
                assert_eq!(
                    serde_json::to_value(&fast).unwrap(),
                    serde_json::to_value(&runtime.engines[&id].state.regime).unwrap()
                );
                let before = serde_json::to_value(&runtime.engines[&id].state).unwrap();
                runtime.on_bar(&bar).unwrap();
                assert_eq!(
                    before,
                    serde_json::to_value(&runtime.engines[&id].state).unwrap()
                );
            }
        }
    }
    for (id, minute) in [(aapl, 30), (msft, 15)] {
        let engine = &runtime.engines[&id];
        engine
            .state
            .validate_signal(&engine.config.grid, engine.config.bar_type)
            .unwrap();
        assert_eq!(
            engine.state.regime_filter.as_ref().unwrap().source().ts_ns,
            open + minute * 60_000_000_000
        );
    }
    let checkpoint = runtime.checkpoint(None, None);
    let checkpoint: super::Checkpoint =
        serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    checkpoint.validate(&runtime.config).unwrap();
    let mut restored = MultiAssetGridStrategy::new(runtime.config.clone()).unwrap();
    restored.on_load(runtime.on_save().unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&runtime.engines[&aapl].state).unwrap(),
        serde_json::to_value(&restored.engines[&aapl].state).unwrap()
    );
    let mut corrupt = checkpoint;
    corrupt
        .instruments
        .get_mut(&aapl)
        .unwrap()
        .state
        .regime_filter = None;
    assert!(corrupt.validate(&runtime.config).is_err());
}

#[rstest]
fn regime_research_defaults_restore_old_checkpoints_without_enabling_new_behavior() {
    let (runtime, _, _) = runtime(1);
    let mut document = serde_json::to_value(runtime.checkpoint(None, None)).unwrap();
    let grid = document["config"]["instruments"]["AAPL.SIM"]["grid"]
        .as_object_mut()
        .unwrap();
    grid.remove("regime_bar_minutes");
    grid.remove("regime_confirmation_bars");
    let saved: super::Checkpoint = serde_json::from_value(document).unwrap();
    saved.validate(&runtime.config).unwrap();
    assert!(
        saved.instruments[&InstrumentId::from("AAPL.SIM")]
            .state
            .regime_filter
            .is_none()
    );
}

#[rstest]
fn gap_simplification_restores_old_config_without_loosening_active_limits() {
    let (runtime, _, _) = runtime(1);
    let mut document = serde_json::to_value(runtime.checkpoint(None, None)).unwrap();
    document["config"]["instruments"]["AAPL.SIM"]["grid"]["max_gap_atr_multiple"] =
        serde_json::json!("3");
    let saved: super::Checkpoint = serde_json::from_value(document).unwrap();
    saved.validate(&runtime.config).unwrap();
    let canonical = serde_json::to_value(&saved).unwrap();
    assert!(
        canonical["config"]["instruments"]["AAPL.SIM"]["grid"]
            .get("max_gap_atr_multiple")
            .is_none()
    );
    let mut changed = runtime.config.clone();
    changed
        .instruments
        .values_mut()
        .next()
        .unwrap()
        .grid
        .max_gap_pct += dec!(0.01);
    assert!(saved.validate(&changed).is_err());
}

#[rstest]
#[case(dec!(0.7))]
#[case(dec!(0.500))]
#[case(dec!(0.9))]
fn review_checkpoint_allows_only_equivalent_exposure_migration(#[case] legacy: Decimal) {
    let (mut runtime, _, _) = runtime(1);
    runtime.config.portfolio.max_total_exposure = dec!(0.7);
    runtime.config.portfolio.max_total_grid_exposure = Some(legacy);
    runtime.config.portfolio.max_total_equity_exposure = Some(dec!(0.8));
    let saved = runtime.checkpoint(None, None);
    let saved: super::Checkpoint =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    let mut canonical = runtime.config.clone();
    canonical.portfolio.max_total_exposure = dec!(0.7).min(legacy).normalize();
    canonical.portfolio.max_total_grid_exposure = None;
    canonical.portfolio.max_total_equity_exposure = None;
    saved.validate(&canonical).unwrap();
    let mut changed = canonical.clone();
    changed.portfolio.max_total_exposure += dec!(0.01);
    assert!(saved.validate(&changed).is_err());
    let mut changed = canonical;
    changed
        .instruments
        .values_mut()
        .next()
        .unwrap()
        .grid
        .grid_levels += 1;
    assert!(saved.validate(&changed).is_err());
}

#[rstest]
#[case::expired(-181_000_000_000, true)]
#[case::future(1_000_000_000, true)]
#[case::out_of_order(-1_000_000_000, true)]
#[case::fresh(0, false)]
fn review_rejected_quote_cannot_change_spread_gate(#[case] offset: i64, #[case] blocked: bool) {
    use nautilus_model::data::QuoteTick;

    use super::super::{config::StrategyMode, stock::StockGate};

    let (mut runtime, mut engine, _) = runtime(1);
    let now = 1_735_828_260_000_000_000_u64;
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time(now.into(), true);
    engine.config.tick_execution = true;
    engine.config.grid.strategy_mode = StrategyMode::StockAdaptive;
    engine.state.last_tick_event_ns = now;
    engine.state.stock.observe_quote(dec!(99), dec!(101), now);
    let quote = QuoteTick::new(
        engine.config.instrument_id,
        Price::from("99.99"),
        Price::from("100.01"),
        Quantity::from(1),
        Quantity::from(1),
        now.checked_add_signed(offset).unwrap().into(),
        now.into(),
    );
    engine.on_quote(&mut runtime, &quote).unwrap();
    assert_eq!(
        engine.state.stock.gate(&engine.config.grid, now, dec!(100)),
        blocked.then_some(StockGate::WideSpread)
    );
}

#[rstest]
#[case::reduce_filled_inventory(false, false)]
#[case::trim_pending_buys_only(true, false)]
#[case::soft_budget_keeps_covered_inventory(false, true)]
#[case::soft_budget_trims_pending_buys(true, true)]
fn review_target_reduction_replaces_distant_take_profit_only_after_cancel(
    #[case] pending_only: bool,
    #[case] soft_budget: bool,
) {
    use nautilus_common::{
        messages::execution::TradingCommand,
        msgbus::{self, TypedIntoHandler, switchboard::MessagingSwitchboard},
    };
    use nautilus_model::{
        enums::OrderType,
        events::{
            OrderEventAny,
            order::spec::{OrderAcceptedSpec, OrderCanceledSpec},
        },
        identifiers::ClientOrderId,
        orders::{Order, builder::OrderTestBuilder},
    };

    use super::super::{
        config::StrategyMode,
        engine::GridEngine,
        orders::{OrderPhase, PositionComponent},
        regime::MarketRegime,
    };

    let (mut runtime, mut engine, cache) = runtime(1);
    runtime.recovering = false;
    engine.config.grid.strategy_mode = StrategyMode::StockAdaptive;
    if soft_budget {
        engine.config.grid.grid_scale_regime = Some(super::super::grid_scale::GridScaleConfig {
            mode: super::super::grid_scale::GridScaleMode::Adaptive,
            ..Default::default()
        });
    }
    engine.state.state = super::StrategyState::GridActive;
    engine.state.regime.snapshot.initialized = true;
    engine.state.regime.snapshot.regime = MarketRegime::TrendDown;
    engine.state.regime.snapshot.ts_ns = 1;
    let grid = GridEngine::build(
        &engine.config.grid,
        1,
        dec!(110),
        dec!(0.02),
        dec!(10000),
        dec!(0.01),
        dec!(1),
        0,
    )
    .unwrap();
    let buy = engine
        .state
        .orders
        .entry("901-AAPL.SIM", 1, &grid.levels[0], dec!(5), None, 0)
        .unwrap();
    let fill = OrderFilledSpec::builder()
        .instrument_id(engine.config.instrument_id)
        .strategy_id(StrategyId::from("DYNAMIC-GRID-901"))
        .client_order_id(ClientOrderId::from(buy.id.as_str()))
        .account_id(AccountId::from("SIM-001"))
        .position_id(PositionId::from("GRID-POSITION-REVIEW"))
        .order_side(OrderSide::Buy)
        .last_qty(Quantity::from(5))
        .last_px(Price::from("107.84"))
        .build();
    engine.apply_fill(&fill, fill.ts_init.as_u64()).unwrap();
    let position = Position::new(engine.instrument.as_ref().unwrap(), fill);
    cache
        .borrow_mut()
        .add_position(&position, OmsType::Netting)
        .unwrap();
    let sell = engine
        .state
        .orders
        .exit("901-AAPL.SIM", &buy.id, None, 1)
        .unwrap();
    assert_eq!(sell.limit, Some(dec!(110)));
    engine
        .state
        .orders
        .transition(&sell.id, OrderPhase::Accepted, 1);
    let id = ClientOrderId::from(sell.id.as_str());
    let mut order = OrderTestBuilder::new(OrderType::Limit)
        .instrument_id(engine.config.instrument_id)
        .strategy_id(StrategyId::from("DYNAMIC-GRID-901"))
        .client_order_id(id)
        .side(OrderSide::Sell)
        .quantity(Quantity::from(5))
        .price(Price::from("110.00"))
        .build();
    order
        .apply(OrderEventAny::Accepted(
            OrderAcceptedSpec::builder()
                .instrument_id(engine.config.instrument_id)
                .strategy_id(StrategyId::from("DYNAMIC-GRID-901"))
                .client_order_id(id)
                .build(),
        ))
        .unwrap();
    cache
        .borrow_mut()
        .add_order(order, None, None, false)
        .unwrap();
    let mut buys = Vec::new();
    if pending_only {
        engine.state.regime.snapshot.regime = MarketRegime::Range;
        engine.config.grid.capital_allocation = dec!(0.017);
        for (index, qty) in [(2, 3), (4, 5)] {
            let buy = engine
                .state
                .orders
                .entry(
                    "901-AAPL.SIM",
                    1,
                    &grid.levels[index],
                    Decimal::from(qty),
                    None,
                    1,
                )
                .unwrap();
            engine
                .state
                .orders
                .transition(&buy.id, OrderPhase::Accepted, 1);
            let buy_id = ClientOrderId::from(buy.id.as_str());
            let mut native = OrderTestBuilder::new(OrderType::Limit)
                .instrument_id(engine.config.instrument_id)
                .strategy_id(StrategyId::from("DYNAMIC-GRID-901"))
                .client_order_id(buy_id)
                .side(OrderSide::Buy)
                .quantity(Quantity::from(qty))
                .price(Price::from_decimal_dp(buy.reference, 2).unwrap())
                .build();
            native
                .apply(OrderEventAny::Accepted(
                    OrderAcceptedSpec::builder()
                        .instrument_id(engine.config.instrument_id)
                        .strategy_id(StrategyId::from("DYNAMIC-GRID-901"))
                        .client_order_id(buy_id)
                        .venue_order_id(nautilus_model::identifiers::VenueOrderId::from(
                            buy.id.as_str(),
                        ))
                        .build(),
                ))
                .unwrap();
            cache
                .borrow_mut()
                .add_order(native, None, None, false)
                .unwrap();
            buys.push(buy);
        }
    }
    let commands = Rc::new(RefCell::new(Vec::new()));
    let received = Rc::clone(&commands);
    msgbus::register_trading_command_endpoint(
        MessagingSwitchboard::exec_engine_queue_execute(),
        TypedIntoHandler::from(move |command: TradingCommand| received.borrow_mut().push(command)),
    );
    engine.drive(&mut runtime, dec!(100), 1).unwrap();
    if soft_budget {
        // 未建立候选观测时预算为零，也不能把柔性禁买误用成市价清仓。
        assert_eq!(engine.state.position_target.grid, Decimal::ZERO);
        assert_eq!(
            engine.state.orders.orders()[&sell.id].phase,
            OrderPhase::Accepted
        );
        assert!(
            engine
                .state
                .orders
                .orders()
                .values()
                .filter(|o| !o.buy)
                .all(|o| o.limit.is_some())
        );
        for buy in &buys {
            assert_eq!(
                engine.state.orders.orders()[&buy.id].phase,
                OrderPhase::CancelPending
            );
        }
        assert_eq!(commands.borrow().len(), buys.len());
        engine.drive(&mut runtime, dec!(100), 1).unwrap();
        assert_eq!(commands.borrow().len(), buys.len());
        assert_eq!(
            engine
                .state
                .orders
                .component_reservations(PositionComponent::Grid),
            (if pending_only { dec!(8) } else { dec!(0) }, dec!(5))
        );
        return;
    }
    if pending_only {
        assert_eq!(engine.state.position_target.grid, dec!(10));
        assert_eq!(
            engine.state.orders.orders()[&sell.id].phase,
            OrderPhase::Accepted
        );
        assert_eq!(
            engine.state.orders.orders()[&buys[0].id].phase,
            OrderPhase::Accepted
        );
        assert_eq!(
            engine.state.orders.orders()[&buys[1].id].phase,
            OrderPhase::CancelPending
        );
        assert_eq!(commands.borrow().len(), 1);
        engine.drive(&mut runtime, dec!(100), 1).unwrap();
        assert_eq!(commands.borrow().len(), 1);
        assert_eq!(
            engine
                .state
                .orders
                .component_reservations(PositionComponent::Grid),
            (dec!(8), dec!(5))
        );
        return;
    }
    assert_eq!(
        engine.state.orders.orders()[&sell.id].phase,
        OrderPhase::CancelPending
    );
    assert!(
        matches!(&commands.borrow()[0], TradingCommand::CancelOrder(command) if command.client_order_id == id)
    );
    assert_eq!(
        engine
            .state
            .orders
            .component_reservations(PositionComponent::Grid)
            .1,
        dec!(5)
    );
    let count = engine.state.orders.orders().len();
    engine.drive(&mut runtime, dec!(100), 1).unwrap();
    assert_eq!(engine.state.orders.orders().len(), count);
    assert_eq!(commands.borrow().len(), 1);
    engine
        .state
        .orders
        .transition(&sell.id, OrderPhase::Unknown, 1);
    engine.drive(&mut runtime, dec!(100), 1).unwrap();
    assert_eq!(engine.state.orders.orders().len(), count);
    assert_eq!(
        engine
            .state
            .orders
            .component_reservations(PositionComponent::Grid)
            .1,
        dec!(5)
    );
    engine
        .state
        .orders
        .transition(&sell.id, OrderPhase::Cancelled, 1);
    cache
        .borrow_mut()
        .update_order(&OrderEventAny::Canceled(
            OrderCanceledSpec::builder()
                .instrument_id(engine.config.instrument_id)
                .strategy_id(StrategyId::from("DYNAMIC-GRID-901"))
                .client_order_id(id)
                .build(),
        ))
        .unwrap();
    engine.drive(&mut runtime, dec!(100), 1).unwrap();
    let reductions: Vec<_> = engine
        .state
        .orders
        .orders()
        .values()
        .filter(|o| !o.buy && o.limit.is_none())
        .collect();
    assert_eq!(reductions.len(), 1);
    assert_eq!(reductions[0].quantity, dec!(5));
    engine.drive(&mut runtime, dec!(100), 1).unwrap();
    assert_eq!(engine.state.orders.orders().len(), count + 1);
}

#[rstest]
#[case::core_only(false)]
#[case::core_and_grid(true)]
fn flatten_covers_core_and_grid_without_duplicating_partial_exits(#[case] mixed: bool) {
    use nautilus_common::{
        messages::execution::TradingCommand,
        msgbus::{self, TypedIntoHandler, switchboard::MessagingSwitchboard},
    };

    use super::super::{
        engine::GridEngine,
        orders::{OrderPhase, PositionComponent},
    };

    let (mut runtime, mut engine, _) = runtime(1);
    runtime.recovering = false;
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
    engine.state.last_price = Some(dec!(90));
    for component in [PositionComponent::Core, PositionComponent::Grid] {
        if component == PositionComponent::Grid && !mixed {
            continue;
        }
        let mut level = grid.levels[0].clone();
        if component == PositionComponent::Core {
            level.level_index = 0;
        }
        let order = engine
            .state
            .orders
            .entry_component("901-AAPL.SIM", 1, &level, dec!(8), None, component, 0)
            .unwrap();
        engine
            .state
            .orders
            .fill(&order.id, "buy", dec!(8), dec!(98), dec!(0), false, 0)
            .unwrap();
    }
    let commands = Rc::new(RefCell::new(Vec::new()));
    let received = Rc::clone(&commands);
    msgbus::register_trading_command_endpoint(
        MessagingSwitchboard::risk_engine_queue_execute(),
        TypedIntoHandler::from(move |command: TradingCommand| received.borrow_mut().push(command)),
    );
    engine.exits(&mut runtime, 1, true).unwrap();
    let exits: Vec<_> = engine
        .state
        .orders
        .orders()
        .values()
        .filter(|order| !order.buy)
        .cloned()
        .collect();
    assert_eq!(exits.len(), if mixed { 2 } else { 1 });
    assert!(exits.iter().all(|order| order.limit.is_none()));
    for order in &exits {
        engine
            .state
            .orders
            .fill(&order.id, "partial", dec!(3), dec!(90), dec!(0), false, 2)
            .unwrap();
        engine
            .state
            .orders
            .transition(&order.id, OrderPhase::CancelPending, 2);
    }
    engine.exits(&mut runtime, 3, true).unwrap();
    assert_eq!(
        engine
            .state
            .orders
            .orders()
            .values()
            .filter(|order| !order.buy)
            .count(),
        exits.len()
    );
    assert_eq!(commands.borrow().len(), exits.len());
    for order in &exits {
        engine
            .state
            .orders
            .fill(&order.id, "rest", dec!(5), dec!(90), dec!(0), false, 4)
            .unwrap();
    }
    assert_eq!(engine.state.orders.inventory(), Decimal::ZERO);
}

#[rstest]
#[case::bar_budget(false)]
#[case::tick_requires_quote(true)]
fn stock_grid_plan_uses_the_position_target_budget(#[case] tick_execution: bool) {
    use nautilus_model::data::Bar;

    use super::super::{
        config::StrategyMode,
        regime_filter::{RegimeFilter, observation},
    };
    let (mut runtime, mut engine, _) = runtime(1);
    runtime.recovering = false;
    engine.config.grid.strategy_mode = StrategyMode::StockAdaptive;
    engine.config.tick_execution = tick_execution;
    engine.config.grid.regular_session_only = true;
    engine.config.grid.minimum_average_dollar_volume = Decimal::ZERO;
    engine.config.grid.core_target_pct = Decimal::ZERO;
    engine.config.grid.grid_max_pct = Decimal::ONE;
    engine.config.grid.initial_inventory_fraction = Decimal::ZERO;
    engine.state.state = super::StrategyState::WaitingForRange;
    engine.config.grid.atr_period = 2;
    engine.config.grid.adx_period = 2;
    engine.config.grid.ma_period = 2;
    engine.config.grid.slope_period = 2;
    engine.config.grid.volatility_period = 2;
    engine.config.grid.enable_trend_filter = false;
    engine.config.grid.enable_volatility_filter = false;
    let mut filter = RegimeFilter::default();
    let mut now = 0;
    for minute in 1..=375 {
        now = 1_735_828_200_000_000_000 + minute * 60_000_000_000;
        let bar = Bar::new(
            engine.config.bar_type,
            Price::from("100"),
            Price::from("101"),
            Price::from("99"),
            Price::from("100"),
            Quantity::from(1000),
            now.into(),
            now.into(),
        );
        filter.update(&engine.config.grid, &bar, None).unwrap();
        engine
            .state
            .regime
            .update(&engine.config.grid, observation(&bar))
            .unwrap();
    }
    engine.state.regime_filter = Some(filter);
    // 与生产 with_engine 一致：当前引擎执行期间从组合映射临时移出。
    runtime.engines.remove(&engine.config.instrument_id);
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time(now.into(), true);
    engine.drive(&mut runtime, dec!(100), now).unwrap();
    if tick_execution {
        assert!(
            engine.state.orders.active_ids().is_empty(),
            "Trade-only marks cannot establish executable spread"
        );
        return;
    }
    let grid = engine.state.grid.as_ref().unwrap();
    let planned: Decimal = grid.levels.iter().map(|level| level.quantity).sum();
    assert!(planned > Decimal::ZERO);
    assert!(
        planned <= engine.state.position_target.grid,
        "Plan {planned} exceeds target {:?}",
        engine.state.position_target
    );
}

#[rstest]
#[case::opening_buy(true)]
#[case::opening_sell(false)]
fn gap_pnl_uses_inventory_observed_before_the_open(#[case] opening_buy: bool) {
    use nautilus_model::data::Bar;

    use super::super::{config::StrategyMode, engine::GridEngine};
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
    let entry = engine
        .state
        .orders
        .entry("gap", 1, &grid.levels[0], dec!(5), None, 0)
        .unwrap();
    engine
        .state
        .orders
        .fill(&entry.id, "old", dec!(5), dec!(100), dec!(0), false, 0)
        .unwrap();
    engine.config.grid.strategy_mode = StrategyMode::StockAdaptive;
    let close_ns = 1_735_851_600_000_000_000_u64;
    for (ts, price) in [(close_ns, "100"), (close_ns + 63_060_000_000_000, "90")] {
        if ts != close_ns {
            let order = if opening_buy {
                engine
                    .state
                    .orders
                    .entry("gap", 1, &grid.levels[2], dec!(5), None, ts)
                    .unwrap()
            } else {
                engine
                    .state
                    .orders
                    .exit("gap", &entry.id, Some(dec!(90)), ts)
                    .unwrap()
            };
            engine
                .state
                .orders
                .fill(&order.id, "open", dec!(5), dec!(90), dec!(0), false, ts)
                .unwrap();
        }
        runtime
            .core
            .clock_mut()
            .as_any_mut()
            .downcast_mut::<TestClock>()
            .unwrap()
            .advance_time(ts.into(), true);
        let bar = Bar::new(
            engine.config.bar_type,
            Price::from(price),
            Price::from(price),
            Price::from(price),
            Price::from(price),
            Quantity::from(1000),
            ts.into(),
            ts.into(),
        );
        engine.on_bar(&mut runtime, &bar).unwrap();
    }
    assert_eq!(engine.report.borrow().metrics.gap_pnl, dec!(-50));
    assert_eq!(engine.report.borrow().metrics.gap_loss, dec!(50));
}

#[rstest]
fn bar_checkpoint_contains_both_instrument_and_portfolio_observations() {
    use nautilus_common::actor::DataActor;
    use nautilus_model::data::Bar;
    let (mut runtime, engine, _) = runtime(1);
    runtime
        .engines
        .get_mut(&engine.config.instrument_id)
        .unwrap()
        .instrument = engine.instrument.clone();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("grid.json");
    runtime.config.state_path = Some(path.clone());
    let now = 1_735_828_260_000_000_000_u64;
    runtime
        .core
        .clock_mut()
        .as_any_mut()
        .downcast_mut::<TestClock>()
        .unwrap()
        .advance_time(now.into(), true);
    let bar = Bar::new(
        engine.config.bar_type,
        Price::from("100"),
        Price::from("100"),
        Price::from("100"),
        Price::from("100"),
        Quantity::from(1000),
        now.into(),
        now.into(),
    );
    runtime.on_bar(&bar).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let saved: super::Checkpoint = serde_json::from_slice(&bytes).unwrap();
    saved.validate(&runtime.config).unwrap();
    assert_eq!(
        saved.instruments[&engine.config.instrument_id]
            .performance
            .equity
            .last()
            .unwrap()
            .ts_ns,
        now
    );
    assert_eq!(
        saved.portfolio_performance.equity.last().unwrap().ts_ns,
        now
    );
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        serde_json::to_value(runtime.checkpoint(None, None)).unwrap()
    );
}

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
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        serde_json::to_value(&recovered).unwrap()
    );
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
