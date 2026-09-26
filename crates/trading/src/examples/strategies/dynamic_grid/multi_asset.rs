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

//! 一个原生策略持有相互隔离的标的引擎，并由强制共享的组合风控门统一准入订单。

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    rc::Rc,
};

use indexmap::IndexMap;
use nautilus_common::{
    actor::DataActor, cache::Cache, messages::system::SocketStateChanged, timer::TimeEvent,
};
use nautilus_model::{
    accounts::Account,
    data::{
        Bar, BarType, CustomData, CustomDataTrait, DataType, QuoteTick, TradeTick,
        bar_vwap::BarWithVwap,
    },
    enums::{OmsType, OrderSide, PositionSide},
    events::{
        OrderAccepted, OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired,
        OrderFillVoided, OrderFilled, OrderRejected, OrderSubmitted,
    },
    identifiers::{InstrumentId, StrategyId},
    instruments::Instrument,
    orders::{Order, OrderAny},
    position::Position,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    analytics::{EquityPoint, PerformanceTracker, number},
    config::GridConfig,
    orders::{GridOrder, OrderPhase},
    portfolio::{InstrumentExposure, OrderDecision, PortfolioConfig, PortfolioRiskManager},
    regime::MarketRegime,
    regime_filter::RegimeFilter,
    strategy::{DynamicGridConfig, GridState, GridStrategyEngine, StrategyState},
};
use crate::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};

#[cfg(test)]
mod tests;

/// 单标的独立信号与仓位配置；资金分配为共享初始资金的比例。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentConfig {
    /// 禁用标的仍参与恢复，并允许卖出已有库存。
    pub enabled: bool,
    /// 该标的资金上限；未使用或动态缩减的预算继续保留为共享现金。
    pub capital_allocation: Decimal,
    /// 单标的库存与待买金额占组合总权益的最大比例。
    #[serde(default = "default_position_fraction")]
    pub max_position_pct: Decimal,
    /// 显式行业标签；None 统一归入 Unknown 行业。
    pub sector: Option<String>,
    /// 该标的独立的已完成 K 线信号流。
    pub bar_type: BarType,
    /// 单标的网格与风险参数；实际资金在构建时由组合分配。
    #[serde(default)]
    pub grid: GridConfig,
    /// Adapter 是否发布已确认的自定义 K 线。
    #[serde(default)]
    pub confirmed_custom_bars: bool,
    /// 是否由 Quote/Trade Tick 而非 K 线触发执行判断。
    #[serde(default)]
    pub tick_execution: bool,
}

/// 使用单一账户和报价币种、且只有一个原生策略身份的多标的组合。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiAssetGridConfig {
    /// 原生策略身份、订单归属与执行设置。
    pub base: StrategyConfig,
    /// 稳定的标的键；各标的之间不共享信号、网格或订单账本。
    pub instruments: BTreeMap<InstrumentId, InstrumentConfig>,
    /// 每笔新增风险订单都必须经过的组合级限制。
    #[serde(default)]
    pub portfolio: PortfolioConfig,
    /// 覆盖全部标的和共享风险状态的单一原子检查点。
    pub state_path: Option<PathBuf>,
    /// 由 runner 提供的环境/账户身份。
    pub recovery_context: Option<String>,
    /// 只估值、不交易的外部持仓标的；必须属于配置池，且不能被本策略认领。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub isolated_instruments: BTreeSet<InstrumentId>,
}

fn default_position_fraction() -> Decimal {
    Decimal::new(20, 2)
}

impl MultiAssetGridConfig {
    /// 校验订单归属、标的键、现金分配及单标的/组合两层风险配置。
    ///
    /// # Errors
    ///
    /// 预算无效、K 线标的不一致、混用多个交易场所或生命周期设置不安全时返回错误。
    pub fn validate(&self) -> anyhow::Result<()> {
        self.base.validate()?;
        self.portfolio.validate()?;
        anyhow::ensure!(
            !self.instruments.is_empty(),
            "At least one instrument is required"
        );
        anyhow::ensure!(
            self.isolated_instruments
                .iter()
                .all(|id| self.instruments.contains_key(id))
                && self.isolated_instruments.len() < self.instruments.len()
                && self
                    .base
                    .external_order_claims
                    .as_ref()
                    .is_none_or(|claims| claims
                        .iter()
                        .all(|id| !self.isolated_instruments.contains(id))),
            "Isolated instruments must be configured, unclaimed, and leave a tradable universe"
        );
        anyhow::ensure!(
            self.base.strategy_id.is_some()
                && self.base.order_id_tag.is_some()
                && !self.base.manage_stop,
            "Explicit strategy identity required; automatic unmanaged flattening is unsupported"
        );
        let venue = self
            .instruments
            .keys()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty instrument universe"))?
            .venue;
        let mut total = Decimal::ZERO;
        for (id, instrument) in &self.instruments {
            instrument.grid.validate()?;
            anyhow::ensure!(
                instrument.grid.sequential_requote_bars.is_none()
                    || instrument.bar_type.spec().is_time_aggregated(),
                "Sequential requote requires time-aggregated signal bars"
            );
            RegimeFilter::validate_bar_type(&instrument.grid, instrument.bar_type)?;
            anyhow::ensure!(
                id.venue == venue && instrument.bar_type.instrument_id() == *id,
                "Portfolio requires matching signal instruments on one account venue"
            );
            anyhow::ensure!(
                instrument.capital_allocation > Decimal::ZERO
                    && instrument.capital_allocation <= self.portfolio.max_instrument_allocation,
                "Invalid allocation for {id}"
            );
            anyhow::ensure!(
                instrument.max_position_pct > Decimal::ZERO
                    && instrument.max_position_pct <= Decimal::ONE
                    && self.portfolio.capital * instrument.capital_allocation > Decimal::ZERO,
                "Invalid instrument position fraction or funded capital"
            );
            anyhow::ensure!(
                instrument
                    .sector
                    .as_ref()
                    .is_none_or(|s| !s.trim().is_empty()),
                "Empty sector label"
            );
            total += instrument.capital_allocation;
        }
        anyhow::ensure!(
            total <= Decimal::ONE - self.portfolio.min_cash_reserve,
            "Initial allocations consume the portfolio cash reserve"
        );
        Ok(())
    }
}

impl From<DynamicGridConfig> for MultiAssetGridConfig {
    fn from(c: DynamicGridConfig) -> Self {
        let portfolio = PortfolioConfig {
            capital: c.grid.capital,
            max_instrument_allocation: Decimal::ONE,
            max_total_exposure: Decimal::ONE,
            min_cash_reserve: Decimal::ZERO,
            max_sector_exposure: Decimal::ONE,
            max_correlated_exposure: Decimal::ONE,
            max_portfolio_drawdown: c.grid.max_drawdown,
            max_portfolio_daily_loss: c.grid.max_daily_loss,
            trend_up_allocation_factor: Decimal::ONE,
            trend_down_allocation_factor: Decimal::ONE,
            allocation_pnl_weight: Decimal::ZERO,
            allocation_volatility_target: Decimal::ONE,
            ..Default::default()
        };
        Self {
            base: c.base,
            instruments: BTreeMap::from([(
                c.instrument_id,
                InstrumentConfig {
                    enabled: true,
                    capital_allocation: Decimal::ONE,
                    max_position_pct: Decimal::ONE,
                    sector: None,
                    bar_type: c.bar_type,
                    grid: c.grid,
                    confirmed_custom_bars: c.confirmed_custom_bars,
                    tick_execution: c.tick_execution,
                },
            )]),
            portfolio,
            state_path: c.state_path,
            recovery_context: c.recovery_context,
            isolated_instruments: BTreeSet::new(),
        }
    }
}

/// 组合与单标的盈亏分别可审计；不会错误地把各标的 Sharpe 相加。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioReport {
    /// 从共享组合权益曲线计算的统计结果。
    pub portfolio: PerformanceTracker,
    /// 各标的独立归因结果，包含尚未卖出的库存。
    pub instruments: BTreeMap<InstrumentId, PerformanceTracker>,
    /// 共享亏损限制、预算决策与相关性历史。
    pub risk: PortfolioRiskManager,
    /// 使用已完成交易日计算的滚动 Pearson 相关矩阵；None 表示对齐样本不足。
    pub correlations: BTreeMap<InstrumentId, BTreeMap<InstrumentId, Option<f64>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstrumentCheckpoint {
    state: GridState,
    performance: PerformanceTracker,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    config: MultiAssetGridConfig,
    instruments: BTreeMap<InstrumentId, InstrumentCheckpoint>,
    portfolio_risk: PortfolioRiskManager,
    portfolio_performance: PerformanceTracker,
    orders: Vec<OrderAny>,
    positions: Vec<Position>,
    #[serde(default)]
    external_positions: BTreeMap<InstrumentId, ExternalPosition>,
}

/// 只保存隔离归属与风险基线，不伪造成交或网格库存。数量变化必须重新人工核对。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ExternalPosition {
    quantity: Decimal,
    reference_price: Option<Decimal>,
    // 重启后必须等新报价，不能用检查点中的旧行情开放买入。
    #[serde(skip)]
    price: Decimal,
    #[serde(skip)]
    ts_ns: u64,
}

// 与读取格式保持一致；写盘借用策略账本和分析历史，不为每次 fsync 克隆全部历史。
#[derive(Serialize)]
struct InstrumentCheckpointRef<'a> {
    state: &'a GridState,
    performance: &'a RefCell<PerformanceTracker>,
}

#[derive(Serialize)]
struct CheckpointRef<'a> {
    version: u32,
    config: &'a MultiAssetGridConfig,
    instruments: BTreeMap<InstrumentId, InstrumentCheckpointRef<'a>>,
    portfolio_risk: &'a PortfolioRiskManager,
    portfolio_performance: &'a PerformanceTracker,
    orders: Vec<OrderAny>,
    positions: Vec<Position>,
    external_positions: &'a BTreeMap<InstrumentId, ExternalPosition>,
}

/// 全组合只使用一个原生 `StrategyCore`；单标的引擎只是状态机，不嵌套 Strategy 实例。
#[derive(Debug)]
pub struct MultiAssetGridStrategy {
    core: StrategyCore,
    config: MultiAssetGridConfig,
    engines: BTreeMap<InstrumentId, GridStrategyEngine>,
    portfolio_risk: PortfolioRiskManager,
    portfolio_performance: PerformanceTracker,
    report: Rc<RefCell<PortfolioReport>>,
    loaded: Option<Checkpoint>,
    _state_lock: Option<File>,
    recovering: bool,
    stopped: bool,
    last_account_query: u64,
    entries_resume_ns: u64,
    external_positions: BTreeMap<InstrumentId, ExternalPosition>,
}

impl MultiAssetGridStrategy {
    /// 构建全部独立标的引擎，并独占锁定统一的原子恢复文件。
    ///
    /// # Errors
    ///
    /// 配置无效、状态不兼容/损坏，或已有其他进程写入检查点时返回错误。
    pub fn new(config: impl Into<MultiAssetGridConfig>) -> anyhow::Result<Self> {
        let config = config.into();
        config.validate()?;
        let mut loaded = None;
        let mut state_lock = None;
        if let Some(path) = &config.state_path {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.with_extension("lock"))?;
            file.try_lock()
                .map_err(|e| anyhow::anyhow!("Checkpoint is already in use: {e}"))?;
            state_lock = Some(file);
            if path.exists() {
                let saved: Checkpoint = serde_json::from_reader(File::open(path)?)?;
                saved.validate(&config)?;
                loaded = Some(saved);
            }
        }
        let engines = config
            .instruments
            .iter()
            .filter(|(id, _)| !config.isolated_instruments.contains(id))
            .map(|(id, c)| {
                let mut single = DynamicGridConfig::new(*id, c.bar_type);
                single.base = config.base.clone();
                single.grid = c.grid.clone();
                single.grid.capital = config.portfolio.capital * c.capital_allocation;
                single.confirmed_custom_bars = c.confirmed_custom_bars;
                single.tick_execution = c.tick_execution;
                single.state_path.clone_from(&config.state_path);
                let mut engine = GridStrategyEngine::new(single);
                if let Some(saved) = &loaded {
                    engine.state = saved.instruments[id].state.clone();
                    *engine.report.borrow_mut() = saved.instruments[id].performance.clone();
                }
                (*id, engine)
            })
            .collect();
        let risk = loaded.as_ref().map_or_else(
            || PortfolioRiskManager::new(config.portfolio.capital),
            |s| s.portfolio_risk.clone(),
        );
        let performance = loaded
            .as_ref()
            .map(|s| s.portfolio_performance.clone())
            .unwrap_or_default();
        let report = Rc::new(RefCell::new(PortfolioReport {
            portfolio: performance.clone(),
            instruments: BTreeMap::new(),
            risk: risk.clone(),
            correlations: BTreeMap::new(),
        }));
        Ok(Self {
            external_positions: loaded
                .as_ref()
                .map(|s| s.external_positions.clone())
                .unwrap_or_default(),
            core: StrategyCore::new_checked(config.base.clone())?,
            config,
            engines,
            portfolio_risk: risk,
            portfolio_performance: performance,
            report,
            loaded,
            _state_lock: state_lock,
            recovering: true,
            stopped: false,
            last_account_query: 0,
            entries_resume_ns: 0,
        })
    }

    /// 返回兼容旧接口的单标的报告句柄。
    ///
    /// # Panics
    ///
    /// 多标的组合调用会 panic；此时必须使用 `portfolio_report_handle` 或 `instrument_report_handle`。
    #[must_use]
    pub fn report_handle(&self) -> Rc<RefCell<PerformanceTracker>> {
        assert_eq!(
            self.engines.len(),
            1,
            "Use portfolio_report_handle for multiple instruments"
        );
        Rc::clone(
            &self
                .engines
                .values()
                .next()
                .expect("Nonempty portfolio")
                .report,
        )
    }

    /// 返回指定标的的独立报告，不改变当前事件处理上下文。
    #[must_use]
    pub fn instrument_report_handle(
        &self,
        id: InstrumentId,
    ) -> Option<Rc<RefCell<PerformanceTracker>>> {
        self.engines.get(&id).map(|e| Rc::clone(&e.report))
    }

    /// 返回完整共享报告；原生停止并排空事件后才会完成最终统计。
    #[must_use]
    pub fn portfolio_report_handle(&self) -> Rc<RefCell<PortfolioReport>> {
        Rc::clone(&self.report)
    }

    /// 返回组合生命周期屏障状态，单标的模式则返回兼容状态。
    #[must_use]
    pub fn state(&self) -> StrategyState {
        if self.stopped {
            StrategyState::Stopped
        } else if self
            .engines
            .values()
            .all(|e| e.state.state == StrategyState::Initializing)
        {
            StrategyState::Initializing
        } else if self.recovering {
            StrategyState::Recovering
        } else if self.portfolio_risk.risk_off_reason.is_some() {
            StrategyState::RiskOff
        } else if self.engines.len() == 1 {
            self.engines
                .values()
                .next()
                .map_or(StrategyState::Recovering, |e| e.state.state)
        } else {
            StrategyState::GridActive
        }
    }

    /// 返回指定标的引擎状态；其他标的事件不能覆盖它。
    #[must_use]
    pub fn instrument_state(&self, id: InstrumentId) -> Option<StrategyState> {
        self.engines.get(&id).map(|e| e.state.state)
    }

    /// 在券商对账开始前恢复全部 Nautilus 原生订单与持仓。
    ///
    /// # Errors
    ///
    /// 目标缓存非空或 Nautilus 缓存拒绝快照时返回错误。
    pub fn restore_cache(&self, cache: &mut Cache) -> anyhow::Result<()> {
        if let Some(saved) = &self.loaded {
            for order in &saved.orders {
                anyhow::ensure!(
                    cache.order_ref(&order.client_order_id()).is_none(),
                    "Recovery destination already contains order"
                );
                cache.add_order(order.clone(), order.position_id(), None, false)?;
            }
            for position in &saved.positions {
                cache.add_position(position, OmsType::Netting)?;
            }
        }
        Ok(())
    }

    fn with_engine<T>(
        &mut self,
        id: InstrumentId,
        f: impl FnOnce(&mut GridStrategyEngine, &mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut engine = self
            .engines
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("Unknown instrument {id}"))?;
        let result = f(&mut engine, self);
        engine.report.borrow_mut().diagnostics.observe_stop(
            engine.state.risk.risk_off_reason.as_deref(),
            self.clock().timestamp_ns().as_u64(),
        );
        self.engines.insert(id, engine);
        result
    }

    fn exposures(&self, current: Option<&GridStrategyEngine>) -> Vec<InstrumentExposure> {
        let mut views: Vec<_> = self
            .engines
            .values()
            .chain(current)
            .map(|e| {
                let c = &self.config.instruments[&e.config.instrument_id];
                let mark = e.state.last_price.unwrap_or(Decimal::ZERO);
                let exposure = e.state.orders.inventory() * mark;
                InstrumentExposure {
                    id: e.config.instrument_id,
                    sector: c.sector.clone(),
                    base_allocation: c.capital_allocation,
                    max_position_pct: c.max_position_pct,
                    enabled: c.enabled,
                    cash_delta: e.state.orders.cash - e.config.grid.capital,
                    exposure,
                    pending: e.state.orders.buy_reservations(&e.config.grid, mark).1,
                    net_pnl: e.state.orders.cash + exposure - e.config.grid.capital,
                    regime: e.regime(),
                    atr_pct: if mark > Decimal::ZERO {
                        Decimal::from_f64_retain(e.state.regime.snapshot.atr)
                            .unwrap_or(Decimal::ZERO)
                            / mark
                    } else {
                        Decimal::ZERO
                    },
                    risk_off: e.state.risk.risk_off_reason.is_some(),
                    mark_ns: e.state.last_market_ns,
                    max_age_secs: e.config.grid.max_signal_age_secs,
                }
            })
            .collect();
        for (id, position) in &self.external_positions {
            let config = &self.config.instruments[id];
            let price = if position.price > Decimal::ZERO {
                position.price
            } else {
                position.reference_price.unwrap_or(Decimal::ZERO)
            };
            views.push(InstrumentExposure {
                id: *id,
                sector: config.sector.clone(),
                base_allocation: Decimal::ZERO,
                max_position_pct: Decimal::ZERO,
                enabled: false,
                cash_delta: Decimal::ZERO,
                exposure: position.quantity * price,
                pending: Decimal::ZERO,
                net_pnl: position.quantity * (price - position.reference_price.unwrap_or(price)),
                regime: MarketRegime::Disabled,
                atr_pct: Decimal::ZERO,
                risk_off: false,
                mark_ns: position.ts_ns,
                max_age_secs: config.grid.max_signal_age_secs,
            });
        }
        views
    }

    fn external_ready(&self, now: u64) -> bool {
        self.config.isolated_instruments.iter().all(|id| {
            self.external_positions.get(id).is_some_and(|p| {
                p.quantity.is_zero()
                    || (p.reference_price.is_some()
                        && p.price > Decimal::ZERO
                        && super::regime::is_fresh(
                            p.ts_ns,
                            now,
                            self.config.instruments[id].grid.max_signal_age_secs,
                        ))
            })
        })
    }

    /// 外部市值占用风险额度，但不能扩大网格现金预算，也不能算作策略收益。
    fn risk_equity(
        &self,
        strategy_equity: Decimal,
        broker_equity: Decimal,
        views: &[InstrumentExposure],
    ) -> Decimal {
        (strategy_equity
            + views
                .iter()
                .filter(|v| self.config.isolated_instruments.contains(&v.id))
                .map(|v| v.net_pnl)
                .sum::<Decimal>())
        .min(broker_equity)
    }

    fn owned_or_isolated(&self, position: &Position) -> bool {
        position.side == PositionSide::Long
            && if self
                .config
                .isolated_instruments
                .contains(&position.instrument_id)
            {
                position.strategy_id == StrategyId::from("EXTERNAL")
            } else {
                Some(position.strategy_id) == self.config.base.strategy_id
                    && self
                        .config
                        .instruments
                        .contains_key(&position.instrument_id)
            }
    }

    fn account_capacity(&self, current: &GridStrategyEngine) -> anyhow::Result<(Decimal, Decimal)> {
        let sid = self.config.base.strategy_id.expect("Validated identity");

        // 在限定作用域内读取原生状态，既保留所有权检查，也避免复制不断增长的事件历史。
        let cache = self.core.cache_ref();
        for position in cache.positions_open_refs(None, None, None, None, None) {
            anyhow::ensure!(
                self.owned_or_isolated(&position),
                "Unowned account inventory requires reconciliation"
            );
        }
        for id in &self.config.isolated_instruments {
            let quantity: Decimal = cache
                .positions_open_refs(None, Some(id), None, None, None)
                .iter()
                .map(|p| p.quantity.as_decimal())
                .sum();
            anyhow::ensure!(
                self.external_positions
                    .get(id)
                    .is_some_and(|p| p.quantity == quantity),
                "Isolated inventory changed; reconcile before resuming: {id}"
            );
        }
        for order in cache.orders_open(None, None, None, None, None) {
            anyhow::ensure!(
                order.strategy_id() == sid
                    && self.config.instruments.contains_key(&order.instrument_id()),
                "Unowned account orders require reconciliation"
            );
            let owner = if order.instrument_id() == current.config.instrument_id {
                Some(current)
            } else {
                self.engines.get(&order.instrument_id())
            };
            anyhow::ensure!(
                owner.is_some_and(|e| e
                    .state
                    .orders
                    .orders()
                    .contains_key(order.client_order_id().as_str())),
                "Untracked account order requires reconciliation"
            );
        }
        let instrument = current
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let account = cache
            .account_for_venue(&current.config.instrument_id.venue)
            .ok_or_else(|| anyhow::anyhow!("Account unavailable"))?;
        let free = account
            .balance_free(Some(instrument.quote_currency()))
            .ok_or_else(|| anyhow::anyhow!("Account cash unavailable"))?
            .as_decimal();
        let overlap: Decimal = self
            .engines
            .values()
            .filter(|e| e.config.instrument_id != current.config.instrument_id)
            .chain(std::iter::once(current))
            .map(|e| {
                super::risk::reservation_overlap(
                    &account,
                    e.config.instrument_id,
                    instrument.quote_currency(),
                    e.state
                        .orders
                        .buy_reservations(
                            &e.config.grid,
                            e.state.last_price.unwrap_or(Decimal::ZERO),
                        )
                        .1,
                )
            })
            .sum();
        let account_id = account.id();
        drop(account);
        drop(cache);
        let equity = self
            .portfolio()
            .equity(&current.config.instrument_id.venue, Some(&account_id))
            .get(&instrument.quote_currency())
            .ok_or_else(|| anyhow::anyhow!("Account equity unavailable"))?
            .as_decimal();
        Ok((free + overlap, equity))
    }

    pub(super) fn gate(
        &mut self,
        current: &GridStrategyEngine,
        intent: &GridOrder,
    ) -> anyhow::Result<Decimal> {
        anyhow::ensure!(
            !self
                .config
                .isolated_instruments
                .contains(&current.config.instrument_id),
            "Isolated instrument cannot submit orders"
        );
        if self.recovering || self.stopped {
            return Ok(Decimal::ZERO);
        }
        if !intent.buy {
            let instrument = current
                .instrument
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
            let lot = instrument
                .lot_size()
                .unwrap_or(instrument.size_increment())
                .as_decimal()
                .max(instrument.size_increment().as_decimal());
            let price = intent
                .reference
                .max(current.state.last_price.unwrap_or(intent.reference));
            anyhow::ensure!(price > Decimal::ZERO, "Invalid reduction reference price");
            return Ok(super::engine::floor_tick(
                intent
                    .quantity
                    .min(self.config.portfolio.max_order_value / price),
                lot,
            ));
        }
        self.buy_quantity(
            current,
            intent.quantity,
            intent.reference,
            intent.limit,
            true,
        )
    }

    pub(super) fn buy_quantity(
        &mut self,
        current: &GridStrategyEngine,
        desired: Decimal,
        reference: Decimal,
        limit: Option<Decimal>,
        includes_intent: bool,
    ) -> anyhow::Result<Decimal> {
        let now = self.clock().timestamp_ns().as_u64();
        if self.recovering
            || self.stopped
            || now < self.entries_resume_ns
            || !self.external_ready(now)
        {
            return Ok(Decimal::ZERO);
        }
        let previous_peak = self.portfolio_risk.peak_equity;
        let previous_day = self.portfolio_risk.day;
        let previous_reason = self.portfolio_risk.risk_off_reason.clone();
        let previous_allocations = self.portfolio_risk.allocations.clone();
        let mut views = self.exposures(Some(current));
        let cash =
            self.config.portfolio.capital + views.iter().map(|v| v.cash_delta).sum::<Decimal>();
        let equity = cash
            + views
                .iter()
                .filter(|v| !self.config.isolated_instruments.contains(&v.id))
                .map(|v| v.exposure)
                .sum::<Decimal>();
        let (free, broker_equity) = self.account_capacity(current)?;
        let risk_equity = self.risk_equity(equity, broker_equity, &views);
        self.portfolio_risk
            .observe(&self.config.portfolio, risk_equity, now);
        self.portfolio_risk
            .reallocate(&self.config.portfolio, &views, now);
        let c = &current.config.grid;
        let unit = limit.unwrap_or(reference.max(current.state.last_price.unwrap_or(reference)))
            * (Decimal::ONE + c.maker_fee.max(c.taker_fee) + c.commission + c.slippage);
        // 刚创建的意图已经进入账本；计算既有预留时只排除它自身。
        let view = views
            .iter_mut()
            .find(|v| v.id == current.config.instrument_id)
            .expect("Current engine included");
        if includes_intent {
            view.pending -= desired * unit;
        }
        anyhow::ensure!(
            view.pending >= Decimal::ZERO,
            "Portfolio reservation accounting mismatch"
        );
        let instrument = current
            .instrument
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Instrument unavailable"))?;
        let lot = instrument
            .lot_size()
            .unwrap_or(instrument.size_increment())
            .as_decimal()
            .max(instrument.size_increment().as_decimal());
        let (decision, quantity) = self.portfolio_risk.admit(
            &self.config.portfolio,
            &views,
            current.config.instrument_id,
            risk_equity,
            cash,
            free,
            desired,
            unit,
            lot,
            now,
        );
        if decision != OrderDecision::Allow {
            log::info!(
                "PORTFOLIO_ORDER_GATE instrument={} decision={decision:?} quantity={quantity}",
                current.config.instrument_id,
            );
        }
        if !includes_intent
            && (previous_peak != self.portfolio_risk.peak_equity
                || previous_day != self.portfolio_risk.day
                || previous_reason != self.portfolio_risk.risk_off_reason
                || previous_allocations != self.portfolio_risk.allocations)
        {
            self.persist_engine(current, None)?;
        }
        Ok(quantity)
    }

    pub(super) fn portfolio_blocked(&self) -> bool {
        self.recovering || self.portfolio_risk.risk_off_reason.is_some()
    }

    pub(super) fn portfolio_flatten(&self) -> bool {
        !self.recovering
            && self.portfolio_risk.risk_off_reason.is_some()
            && self.config.portfolio.risk_policy == super::config::RiskPolicy::Flatten
    }

    /// 锁存操作员停机指令，并立即请求撤销所有新增库存订单。
    ///
    /// # Errors
    ///
    /// 撤单或持久化失败时返回错误，但停机状态仍保持锁存。
    pub fn kill_switch(&mut self, reason: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !reason.trim().is_empty(),
            "Kill switch requires an audit reason"
        );
        self.portfolio_risk
            .trip(format!("Operator kill switch: {reason}"));
        self.persist(None, None)?;
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, |e, r| e.cancel(r, true, "OPERATOR_KILL_SWITCH"))?;
        }
        Ok(())
    }

    pub(super) fn halt_portfolio(&mut self, reason: String) {
        self.recovering = true;
        self.portfolio_risk.trip(reason);
    }

    fn halt_all(&mut self, reason: &str) {
        self.halt_portfolio(reason.to_string());
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            if let Err(e) = self.with_engine(id, |engine, runtime| {
                engine.halt(runtime, anyhow::anyhow!(reason.to_string()));
                Ok(())
            }) {
                log::error!("Portfolio emergency cancellation failed: {e}");
            }
        }
    }

    pub(super) fn observe_close(&mut self, id: InstrumentId, now: u64, price: Decimal) {
        self.portfolio_risk
            .close(&self.config.portfolio, id, now, price);
    }

    fn checkpoint<'a>(
        &'a self,
        current: Option<&'a GridStrategyEngine>,
        pending: Option<&OrderAny>,
    ) -> CheckpointRef<'a> {
        let mut orders = self.cache().orders(
            None,
            None,
            self.config.base.strategy_id.as_ref(),
            None,
            None,
        );
        if let Some(order) = pending
            && !orders
                .iter()
                .any(|o| o.client_order_id() == order.client_order_id())
        {
            orders.push(order.clone());
        }
        orders.sort_by_key(Order::client_order_id);
        let instruments = self
            .engines
            .values()
            .chain(current)
            .map(|e| {
                (
                    e.config.instrument_id,
                    InstrumentCheckpointRef {
                        state: &e.state,
                        performance: &e.report,
                    },
                )
            })
            .collect();
        CheckpointRef {
            version: 6,
            config: &self.config,
            external_positions: &self.external_positions,
            instruments,
            portfolio_risk: &self.portfolio_risk,
            portfolio_performance: &self.portfolio_performance,
            orders,
            positions: self.cache().positions_open(
                None,
                None,
                self.config.base.strategy_id.as_ref(),
                None,
                None,
            ),
        }
    }

    fn persist(
        &self,
        current: Option<&GridStrategyEngine>,
        pending: Option<&OrderAny>,
    ) -> anyhow::Result<()> {
        let Some(path) = &self.config.state_path else {
            return Ok(());
        };
        let temporary = path.with_extension("next");
        let mut file = File::create(&temporary)?;
        serde_json::to_writer(&mut file, &self.checkpoint(current, pending))?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }

    pub(super) fn persist_engine(
        &self,
        engine: &GridStrategyEngine,
        pending: Option<&OrderAny>,
    ) -> anyhow::Result<()> {
        self.persist(Some(engine), pending)
    }

    fn observe_portfolio(&mut self, record: bool) -> anyhow::Result<()> {
        let now = self.clock().timestamp_ns().as_u64();
        let previous_peak = self.portfolio_risk.peak_equity;
        let previous_day = self.portfolio_risk.day;
        let previous_reason = self.portfolio_risk.risk_off_reason.clone();
        let previous_allocations = self.portfolio_risk.allocations.clone();
        let views = self.exposures(None);
        let cash =
            self.config.portfolio.capital + views.iter().map(|v| v.cash_delta).sum::<Decimal>();
        let exposure = views
            .iter()
            .filter(|v| !self.config.isolated_instruments.contains(&v.id))
            .map(|v| v.exposure)
            .sum::<Decimal>();
        let total_exposure: Decimal = views.iter().map(|v| v.exposure).sum();
        let position = self
            .engines
            .values()
            .map(|engine| engine.state.orders.inventory())
            .sum();
        let equity = cash + exposure;
        let account = self
            .engines
            .values()
            .next()
            .map(|e| self.account_capacity(e))
            .transpose()?;
        // 重启等待外部报价时不能把风险基线价当作现价，否则可能虚构回撤。
        // 此时仍核对账户所有权/数量并撤买单，但沿用已持久化的最后风险权益。
        let risk_equity = if self.external_ready(now) {
            account.map_or(equity, |(_, value)| self.risk_equity(equity, value, &views))
        } else {
            self.portfolio_risk.last_equity
        };
        self.portfolio_risk
            .observe(&self.config.portfolio, risk_equity, now);
        self.portfolio_performance
            .diagnostics
            .observe_stop(self.portfolio_risk.risk_off_reason.as_deref(), now);
        self.portfolio_risk
            .reallocate(&self.config.portfolio, &views, now);
        self.portfolio_performance.observe_mark(
            self.config.portfolio.capital,
            equity,
            exposure,
            position,
            now,
        );
        if record {
            let pending: Decimal = views.iter().map(|v| v.pending).sum();
            let point = EquityPoint {
                trend_inventory: Some(
                    views
                        .iter()
                        .filter(|v| {
                            !self.config.isolated_instruments.contains(&v.id)
                                && matches!(
                                    v.regime,
                                    MarketRegime::TrendUp | MarketRegime::TrendDown
                                )
                        })
                        .map(|v| v.exposure)
                        .sum(),
                ),
                cumulative_fees: self.engines.values().map(|e| e.state.orders.fees).sum(),
                cumulative_turnover: self.engines.values().map(|e| e.state.orders.turnover).sum(),
                ts_ns: now,
                price: Decimal::ONE,
                equity,
                exposure,
                position,
                utilization: if equity > Decimal::ZERO {
                    number((exposure + pending) / equity)
                } else {
                    0.0
                },
                regime: MarketRegime::Disabled,
            };
            if self
                .portfolio_performance
                .equity
                .last()
                .is_some_and(|p| p.ts_ns == now)
            {
                self.portfolio_performance.equity.pop();
            }
            self.portfolio_performance.equity.push(point);
        }
        // 预算缩减只撤销新增库存订单；撤单结果未确认前仍保留全部资金占用。
        let pending: Decimal = views.iter().map(|v| v.pending).sum();
        let global_limit = self.config.portfolio.exposure_limit();
        let cancel_all = self.portfolio_blocked()
            || !self.external_ready(now)
            || total_exposure + pending > risk_equity * global_limit
            || cash - pending < risk_equity * self.config.portfolio.min_cash_reserve
            || account.is_some_and(|(free, _)| pending > free);
        let concentration_breaches = self.portfolio_risk.concentration_breaches(
            &self.config.portfolio,
            &views,
            risk_equity,
            now,
        );
        for view in views {
            if self.config.isolated_instruments.contains(&view.id) {
                continue; // 风控可撤本策略买单，但绝不触碰外部标的。
            }
            let allocation = self
                .portfolio_risk
                .allocations
                .get(&view.id)
                .copied()
                .unwrap_or(view.base_allocation)
                .min(view.max_position_pct)
                .min(self.config.portfolio.max_instrument_allocation);
            if cancel_all
                || !view.enabled
                || concentration_breaches.contains(&view.id)
                || view.exposure + view.pending > risk_equity * allocation
            {
                self.with_engine(view.id, |e, runtime| {
                    e.cancel(runtime, true, "PORTFOLIO_CAPACITY")
                })?;
            }
        }
        if record
            || previous_peak != self.portfolio_risk.peak_equity
            || previous_day != self.portfolio_risk.day
            || previous_reason != self.portfolio_risk.risk_off_reason
            || previous_allocations != self.portfolio_risk.allocations
        {
            self.persist(None, None)?;
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        id: InstrumentId,
        record: bool,
        f: impl FnOnce(&mut GridStrategyEngine, &mut Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if !self.engines.contains_key(&id) {
            return Ok(());
        }
        let result = self.with_engine(id, |engine, runtime| {
            if let Err(e) = f(engine, runtime) {
                engine.halt(runtime, e);
            }
            Ok(())
        });
        if result.is_ok()
            && let Err(e) = self.observe_portfolio(record)
        {
            self.halt_all(&e.to_string());
            return Err(e);
        }
        result
    }

    fn order_control(&mut self, id: InstrumentId, order: &str, phase: OrderPhase) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if !self.engines.contains_key(&id) {
            self.halt_all(&format!("Unowned order instrument {id}"));
            return;
        }
        let result = self.dispatch(id, false, |e, runtime| {
            anyhow::ensure!(
                e.state.orders.orders().contains_key(order),
                "Unowned order event"
            );
            e.control(runtime, order, phase);
            Ok(())
        });
        if let Err(e) = result {
            self.portfolio_risk.trip(e.to_string());
        }
    }

    fn finish_reports(&mut self) {
        let mut instruments = BTreeMap::new();
        for (id, engine) in &self.engines {
            let orders = &engine.state.orders;
            engine.report.borrow_mut().finish(
                engine.config.grid.capital,
                orders,
                &engine.state.risk,
            );
            instruments.insert(*id, engine.report.borrow().clone());
        }
        let ledgers: Vec<_> = self
            .engines
            .values()
            .map(|e| (&e.state.orders, &e.state.risk))
            .collect();
        let instrument_reports: Vec<_> = instruments.values().collect();
        self.portfolio_performance.finish_portfolio(
            self.config.portfolio.capital,
            &ledgers,
            &instrument_reports,
            self.portfolio_risk.risk_off_reason.clone(),
        );
        let views = self.exposures(None);
        let now = self.clock().timestamp_ns().as_u64();
        let correlations =
            self.portfolio_risk
                .correlation_matrix(&self.config.portfolio, &views, now);
        *self.report.borrow_mut() = PortfolioReport {
            portfolio: self.portfolio_performance.clone(),
            instruments,
            risk: self.portfolio_risk.clone(),
            correlations,
        };
    }

    /// Nautilus 停止后事件排空完毕，再逐标的对账，且不会重新提交订单。
    ///
    /// # Errors
    ///
    /// 策略尚未停止，或任何恢复后的库存/订单历史不一致时返回错误。
    pub fn finalize_after_stop(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.stopped, "Finalization requires a stopped strategy");
        let now = self.clock().timestamp_ns().as_u64();
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, |e, runtime| {
                let result = e.recover(runtime);
                e.state.state = StrategyState::Stopped;
                result?;
                e.record_equity(now);
                Ok(())
            })?;
        }
        self.observe_portfolio(true)?;
        self.finish_reports();
        self.persist(None, None)
    }

    /// 仅在全部订单、库存与现金对账一致后，显式重置单标的和组合两层风险。
    /// Runner 绝不会自动调用这一操作员接口。
    ///
    /// # Errors
    ///
    /// 存在未终结订单、行情过期，或仍违反任一单标的/组合限制时返回错误。
    pub fn reset_risk(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.stopped
                && self
                    .engines
                    .values()
                    .all(|e| e.state.orders.active_ids().is_empty()),
            "Risk reset requires a running portfolio with terminal orders"
        );
        self.recovering = true;
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, GridStrategyEngine::reset_risk)?;
        }
        let now = self.clock().timestamp_ns().as_u64();
        let views = self.exposures(None);
        let cash =
            self.config.portfolio.capital + views.iter().map(|v| v.cash_delta).sum::<Decimal>();
        let exposure = views.iter().map(|v| v.exposure).sum::<Decimal>();
        let (free, broker_equity) = self.account_capacity(
            self.engines
                .values()
                .next()
                .ok_or_else(|| anyhow::anyhow!("Empty instrument universe"))?,
        )?;
        let strategy_exposure: Decimal = views
            .iter()
            .filter(|v| !self.config.isolated_instruments.contains(&v.id))
            .map(|v| v.exposure)
            .sum();
        let equity = self.risk_equity(cash + strategy_exposure, broker_equity, &views);
        let c = &self.config.portfolio;
        anyhow::ensure!(
            self.external_ready(now)
                && views
                    .iter()
                    .all(|v| now.saturating_sub(v.mark_ns) / 1_000_000_000 <= v.max_age_secs)
                && equity > Decimal::ZERO
                && cash.min(free) >= equity * c.min_cash_reserve
                && exposure <= equity * c.exposure_limit()
                && self
                    .portfolio_risk
                    .concentration_breaches(c, &views, equity, now)
                    .is_empty(),
            "Portfolio risk reset requires fresh marks and compliant exposures"
        );
        self.portfolio_risk.peak_equity = equity;
        self.portfolio_risk.last_equity = equity;
        self.portfolio_risk.day_start_equity = equity;
        self.portfolio_risk.day = Some(now / 86_400_000_000_000);
        self.portfolio_risk.risk_off_reason = None;
        self.portfolio_risk.last_reallocation_ns.clear();
        self.recovering = false;
        if let Err(e) = self.persist(None, None) {
            self.halt_portfolio(e.to_string());
            return Err(e);
        }
        Ok(())
    }
}

impl DataActor for MultiAssetGridStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.recovering = true;
        // 原生提交限速器只存在于当前进程。重启后等待完整窗口结束，不能额外获得一份买入额度；
        // 有真实库存覆盖的减仓仍可继续执行。
        let now = self.clock().timestamp_ns().as_u64();
        self.entries_resume_ns = now.saturating_add(nautilus_core::datetime::NANOSECONDS_IN_MINUTE);
        let sid = self.config.base.strategy_id.expect("Validated identity");
        for order in self.cache().orders(None, None, None, None, None) {
            anyhow::ensure!(
                order.is_closed()
                    || (order.strategy_id() == sid
                        && self.engines.contains_key(&order.instrument_id())),
                "Unowned account order blocks portfolio recovery"
            );
        }
        for position in self.cache().positions_open(None, None, None, None, None) {
            anyhow::ensure!(
                self.owned_or_isolated(&position),
                "Unowned account position blocks portfolio recovery"
            );
        }
        for id in self.config.isolated_instruments.clone() {
            let quantity = self
                .cache()
                .positions_open(None, Some(&id), None, None, None)
                .iter()
                .map(|p| p.quantity.as_decimal())
                .sum();
            let position = self
                .external_positions
                .entry(id)
                .or_insert(ExternalPosition {
                    quantity,
                    ..Default::default()
                });
            anyhow::ensure!(
                position.quantity == quantity,
                "Isolated inventory changed on restart: {id}"
            );
            self.subscribe_quotes(id, None, None);
        }
        let ids: Vec<_> = self.engines.keys().copied().collect();
        let mut currency = None;
        for id in &ids {
            let instrument = self.cache().try_instrument(id)?;
            anyhow::ensure!(
                currency.is_none_or(|c| c == instrument.quote_currency()),
                "Portfolio requires a single quote currency"
            );
            currency = Some(instrument.quote_currency());
            self.with_engine(*id, GridStrategyEngine::on_start)?;
        }
        for id in &self.config.isolated_instruments {
            let instrument = self.cache().try_instrument(id)?;
            anyhow::ensure!(
                matches!(
                    instrument,
                    nautilus_model::instruments::InstrumentAny::Equity(_)
                ) && currency == Some(instrument.quote_currency()),
                "Isolation supports same-currency long equities only"
            );
        }
        if self
            .config
            .instruments
            .values()
            .any(|c| c.confirmed_custom_bars)
        {
            self.subscribe_data(
                DataType::new(BarWithVwap::type_name_static(), None, None),
                None,
                None,
            );
        }
        self.recovering = self
            .engines
            .values()
            .any(|e| e.state.state == StrategyState::Recovering);
        if self.recovering {
            self.portfolio_risk.trip("Instrument recovery incomplete");
        }
        self.loaded = None;
        let interval = self
            .engines
            .values()
            .map(|e| {
                e.config
                    .grid
                    .order_timeout_secs
                    .min(e.config.grid.max_signal_age_secs)
            })
            .min()
            .expect("Nonempty portfolio")
            .max(1)
            * 1_000_000_000;
        self.clock()
            .set_timer_ns("GRID_WATCHDOG", interval, None, None, None, None, None)?;
        self.observe_portfolio(true)
    }
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        self.portfolio_performance.diagnostics.bar_events = self
            .portfolio_performance
            .diagnostics
            .bar_events
            .saturating_add(1);
        self.dispatch(bar.bar_type.instrument_id(), true, |e, r| e.on_bar(r, bar))
    }
    fn on_data(&mut self, data: &CustomData) -> anyhow::Result<()> {
        if let Some(value) = data.data.as_any().downcast_ref::<BarWithVwap>() {
            self.dispatch(value.bar.bar_type.instrument_id(), true, |e, r| {
                e.on_data(r, data)
            })?;
        }
        Ok(())
    }
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        // 无效报价既不能推进组合时间，也不能更新估值价格。
        if quote.bid_price.as_decimal() <= Decimal::ZERO
            || quote.ask_price < quote.bid_price
            || quote.bid_size.is_zero()
            || quote.ask_size.is_zero()
        {
            return Ok(());
        }
        if self
            .config
            .isolated_instruments
            .contains(&quote.instrument_id)
        {
            let now = self.clock().timestamp_ns().as_u64();
            let Some(position) = self.external_positions.get_mut(&quote.instrument_id) else {
                return Ok(()); // 启动对账完成之前不建立风险基线。
            };
            if quote.ts_event.as_u64() <= position.ts_ns
                || !super::regime::is_fresh(
                    quote.ts_event.as_u64(),
                    now,
                    self.config.instruments[&quote.instrument_id]
                        .grid
                        .max_signal_age_secs,
                )
            {
                return Ok(());
            }
            position.price = quote.bid_price.as_decimal();
            position.ts_ns = quote.ts_event.as_u64();
            let initialize = position.reference_price.is_none();
            position.reference_price.get_or_insert(position.price);
            self.observe_close(quote.instrument_id, now, quote.bid_price.as_decimal());
            if let Err(error) = self.observe_portfolio(false) {
                self.halt_all(&error.to_string());
                return Err(error);
            }
            if initialize && let Err(error) = self.persist(None, None) {
                self.halt_all(&error.to_string());
                return Err(error);
            }
            return Ok(());
        }
        self.portfolio_performance.diagnostics.quote_events = self
            .portfolio_performance
            .diagnostics
            .quote_events
            .saturating_add(1);
        self.dispatch(quote.instrument_id, false, |e, r| e.on_quote(r, quote))
    }
    fn on_trade(&mut self, trade: &TradeTick) -> anyhow::Result<()> {
        self.dispatch(trade.instrument_id, false, |e, r| e.on_trade(r, trade))
    }
    fn on_time_event(&mut self, event: &TimeEvent) -> anyhow::Result<()> {
        self.portfolio_performance.diagnostics.timer_events = self
            .portfolio_performance
            .diagnostics
            .timer_events
            .saturating_add(1);
        let now = self.clock().timestamp_ns().as_u64();
        if self.config.state_path.is_none()
            && self
                .engines
                .values()
                .all(|engine| !engine.watchdog_required(now))
        {
            return Ok(());
        }
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, |engine, runtime| {
                if let Err(e) = engine.on_time_event(runtime, event) {
                    engine.halt(runtime, e);
                }
                Ok(())
            })?;
        }
        let interval = self
            .config
            .instruments
            .values()
            .map(|c| c.grid.order_timeout_secs)
            .min()
            .expect("Nonempty portfolio");
        let venue = self
            .config
            .instruments
            .keys()
            .next()
            .expect("Nonempty portfolio")
            .venue;
        if self.config.state_path.is_some()
            && now.saturating_sub(self.last_account_query) / 1_000_000_000 >= interval
            && let Some(account_id) = self.cache().account_id(&venue)
        {
            self.query_account(account_id, None, None)?;
            self.last_account_query = now;
        }
        self.observe_portfolio(false)
    }
    fn on_socket_state(&mut self, event: &SocketStateChanged) -> anyhow::Result<()> {
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.dispatch(id, false, |e, r| e.on_socket_state(r, event))?;
        }
        Ok(())
    }
    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.stopped = true;
        let ids: Vec<_> = self.engines.keys().copied().collect();
        for id in ids {
            self.with_engine(id, GridStrategyEngine::on_stop)?;
        }
        self.observe_portfolio(true)?;
        self.finish_reports();
        self.persist(None, None)
    }
    fn on_save(&self) -> anyhow::Result<IndexMap<String, Vec<u8>>> {
        Ok(IndexMap::from([(
            "dynamic_grid".to_string(),
            serde_json::to_vec(&self.checkpoint(None, None))?,
        )]))
    }
    fn on_load(&mut self, state: IndexMap<String, Vec<u8>>) -> anyhow::Result<()> {
        if let Some(bytes) = state.get("dynamic_grid") {
            let saved: Checkpoint = serde_json::from_slice(bytes)?;
            saved.validate(&self.config)?;
            for (id, entry) in &saved.instruments {
                let engine = self.engines.get_mut(id).expect("Validated instrument map");
                engine.state = entry.state.clone();
                *engine.report.borrow_mut() = entry.performance.clone();
            }
            self.portfolio_risk = saved.portfolio_risk.clone();
            self.portfolio_performance = saved.portfolio_performance.clone();
            self.external_positions = saved.external_positions.clone();
            self.loaded = Some(saved);
            self.recovering = true;
        }
        Ok(())
    }
}

nautilus_strategy!(MultiAssetGridStrategy, {
    fn on_order_submitted(&mut self, event: OrderSubmitted) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Submitted,
        );
    }
    fn on_order_accepted(&mut self, event: OrderAccepted) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Accepted,
        );
    }
    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Cancelled,
        );
    }
    fn on_order_expired(&mut self, event: OrderExpired) {
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Expired,
        );
    }
    fn on_order_rejected(&mut self, event: OrderRejected) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if let Err(e) = self.dispatch(event.instrument_id, false, |e, r| {
            e.rejected(
                r,
                event.client_order_id.as_str(),
                format!("Order rejected: {}", event.reason),
            );
            Ok(())
        }) {
            self.portfolio_risk.trip(e.to_string());
        }
    }
    fn on_order_denied(&mut self, event: OrderDenied) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if let Err(e) = self.dispatch(event.instrument_id, false, |e, r| {
            e.rejected(
                r,
                event.client_order_id.as_str(),
                format!("Order denied: {}", event.reason),
            );
            Ok(())
        }) {
            self.portfolio_risk.trip(e.to_string());
        }
    }
    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if let Some(engine) = self.engines.get(&event.instrument_id) {
            engine
                .report
                .borrow_mut()
                .diagnostics
                .cancel_rejections
                .entry(event.client_order_id.to_string())
                .or_insert_with(|| super::diagnostics::RejectionObservation {
                    ts_ns: self.clock().timestamp_ns().as_u64(),
                    reason: event.reason.to_string(),
                });
        }
        self.order_control(
            event.instrument_id,
            event.client_order_id.as_str(),
            OrderPhase::Unknown,
        );
        self.portfolio_risk
            .trip(format!("Cancel failed: {}", event.reason));
        if let Err(e) = self.observe_portfolio(false) {
            log::error!("Portfolio cancellation failed: {e}");
        }
    }
    fn on_order_filled(&mut self, event: &OrderFilled) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        if !self.engines.contains_key(&event.instrument_id) {
            self.halt_all(&format!("Unowned fill instrument {}", event.instrument_id));
            return;
        }
        let result = self.dispatch(event.instrument_id, false, |e, r| {
            let now = r.clock().timestamp_ns().as_u64();
            e.apply_fill(event, now)?;
            r.persist_engine(e, None)?;
            if matches!(
                e.state.state,
                StrategyState::GridActive
                    | StrategyState::WaitingForRange
                    | StrategyState::BreakoutPending
                    | StrategyState::Paused
                    | StrategyState::RiskOff
            ) && !r.portfolio_flatten()
                && !(e.state.risk.risk_off_reason.is_some()
                    && e.config.grid.risk_policy == super::config::RiskPolicy::Flatten)
            {
                e.exits(r, now, false)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            self.portfolio_risk.trip(e.to_string());
        }
    }
    fn on_order_fill_voided(&mut self, _event: &OrderFillVoided) {
        self.portfolio_performance.diagnostics.order_events = self
            .portfolio_performance
            .diagnostics
            .order_events
            .saturating_add(1);
        self.recovering = true;
        self.portfolio_risk
            .trip("Broker voided a fill; portfolio inventory requires audit");
        if let Err(e) = self.observe_portfolio(false) {
            log::error!("Portfolio cancellation failed: {e}");
        }
    }
});

impl Checkpoint {
    fn validate(&self, expected: &MultiAssetGridConfig) -> anyhow::Result<()> {
        // 仅兼容等价的旧总暴露字段迁移，其他配置及真实风险上限变化仍须人工审计。
        self.config.validate()?;
        let canonical = |config: &MultiAssetGridConfig| {
            let mut config = config.clone();
            config.portfolio.max_total_exposure = config.portfolio.exposure_limit().normalize();
            config.portfolio.max_total_grid_exposure = None;
            config.portfolio.max_total_equity_exposure = None;
            serde_json::to_value(config)
        };
        anyhow::ensure!(
            self.version == 6 && canonical(&self.config)? == canonical(expected)?,
            "Checkpoint version/configuration mismatch; audit before migration"
        );
        anyhow::ensure!(
            self.instruments.keys().eq(expected
                .instruments
                .keys()
                .filter(|id| !expected.isolated_instruments.contains(id))),
            "Recovered instrument set mismatch"
        );
        anyhow::ensure!(
            self.external_positions
                .keys()
                .eq(expected.isolated_instruments.iter())
                && self
                    .external_positions
                    .values()
                    .all(|p| p.quantity >= Decimal::ZERO
                        && p.reference_price.is_none_or(|price| price > Decimal::ZERO)),
            "Invalid isolated position checkpoint"
        );
        self.portfolio_risk.validate(
            &expected.portfolio,
            &expected.instruments.keys().copied().collect(),
        )?;
        let mut ids = BTreeSet::new();
        for (id, saved) in &self.instruments {
            saved.state.orders.validate()?;
            let config = &expected.instruments[id];
            saved.state.validate_signal(&config.grid, config.bar_type)?;
            anyhow::ensure!(
                saved.state.completed_cycles <= saved.state.orders.cycles.len(),
                "Invalid cycle cursor"
            );
            for order in saved.state.orders.orders().keys() {
                anyhow::ensure!(
                    ids.insert(order),
                    "Duplicate cross-instrument order identity"
                );
                let prefix = format!(
                    "DG-{}-{id}-",
                    expected
                        .base
                        .order_id_tag
                        .as_deref()
                        .expect("Validated tag")
                );
                anyhow::ensure!(
                    order.starts_with(&prefix),
                    "Order namespace differs from instrument owner"
                );
            }
            if let Some(grid) = &saved.state.grid {
                let config = &expected.instruments[id].grid;
                let count = config.grid_levels * 2;
                anyhow::ensure!(
                    grid.grid_id <= saved.state.generation
                        && grid.center > Decimal::ZERO
                        && grid.spacing > Decimal::ZERO
                        && grid.lower_bound > Decimal::ZERO
                        && grid.upper_bound > grid.lower_bound
                        && grid.lower_bound <= grid.center
                        && grid.center <= grid.upper_bound
                        && grid.levels.len() == count,
                    "Invalid recovered grid"
                );
                let mut indices = BTreeSet::new();
                for level in &grid.levels {
                    let identity_valid = level.level_index != 0
                        && level.level_index.unsigned_abs() as usize <= config.grid_levels;
                    anyhow::ensure!(
                        identity_valid
                            && indices.insert(level.level_index)
                            && level.side == OrderSide::Buy
                            && level.quantity >= Decimal::ZERO
                            && level.price >= grid.lower_bound
                            && level.exit_price > level.price
                            && level.exit_price <= grid.upper_bound,
                        "Invalid recovered grid level for {id}"
                    );
                }
            }
        }
        for order in &self.orders {
            anyhow::ensure!(
                Some(order.strategy_id()) == expected.base.strategy_id
                    && self
                        .instruments
                        .get(&order.instrument_id())
                        .is_some_and(|e| e
                            .state
                            .orders
                            .orders()
                            .contains_key(order.client_order_id().as_str())),
                "Native order snapshot ownership mismatch"
            );
        }
        for position in &self.positions {
            anyhow::ensure!(
                Some(position.strategy_id) == expected.base.strategy_id
                    && self.instruments.contains_key(&position.instrument_id)
                    && position.side == PositionSide::Long,
                "Native position snapshot ownership mismatch"
            );
        }
        for (id, weight) in &self.portfolio_risk.allocations {
            anyhow::ensure!(
                expected
                    .instruments
                    .get(id)
                    .is_some_and(|c| *weight >= Decimal::ZERO && *weight <= c.capital_allocation),
                "Invalid recovered allocation"
            );
        }
        anyhow::ensure!(
            self.portfolio_risk.peak_equity > Decimal::ZERO,
            "Invalid portfolio high-water mark"
        );
        Ok(())
    }
}
