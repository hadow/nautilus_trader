// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! 使用与实时策略相同的因果特征和结构状态机执行轻量研究回测。
//!
//! 信号只在 Bar close 产生，市价入场延迟到下一根 Bar open。同根 Bar 同时触及止损和目标时
//! 一律按止损先发生，避免用 5 分钟 OHLC 猜测更有利的盘中路径。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use jiff::{Timestamp, civil::Date};
use nautilus_core::UnixNanos;
use nautilus_model::{data::Bar, enums::OrderSide, instruments::Instrument};
use rust_decimal::{Decimal, prelude::ToPrimitive};

use super::{
    data::{PreparedSymbol, ResearchConfig},
    feature::{CompletedBar, TrendRegime, VolatilityRegime, WyckoffFeatureEngine},
    structure::{EntryVariant, StudyLayer, WyckoffRules, WyckoffSignal, WyckoffStructureDetector},
};

const FIVE_MINUTE_NANOS: u64 = 5 * 60 * 1_000_000_000;
const BASIS_POINTS: i64 = 10_000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub(crate) label: &'static str,
    pub(crate) start: Timestamp,
    pub(crate) end: Timestamp,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SimulationSpec {
    pub(crate) layer: StudyLayer,
    pub(crate) variant: EntryVariant,
    pub(crate) risk_reward: Decimal,
    pub(crate) cost_multiple: Decimal,
}

#[derive(Clone, Debug)]
pub(crate) struct TradeRecord {
    pub(crate) symbol: String,
    pub(crate) side: OrderSide,
    pub(crate) regime: TrendRegime,
    pub(crate) entry_minute: u16,
    pub(crate) date: Date,
    pub(crate) initial_risk: Decimal,
    pub(crate) net_pnl: Decimal,
    pub(crate) holding_bars: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SimulationResult {
    pub(crate) sample: Sample,
    pub(crate) spec: SimulationSpec,
    pub(crate) trades: Vec<TradeRecord>,
    pub(crate) metrics: Metrics,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Metrics {
    pub(crate) trades: usize,
    pub(crate) win_rate_pct: Option<f64>,
    pub(crate) average_win: Option<f64>,
    pub(crate) average_loss: Option<f64>,
    pub(crate) profit_factor: Option<f64>,
    pub(crate) expectancy: Option<f64>,
    pub(crate) expectancy_r: Option<f64>,
    pub(crate) sharpe: Option<f64>,
    pub(crate) sortino: Option<f64>,
    pub(crate) max_drawdown_pct: Option<f64>,
    pub(crate) cagr_pct: Option<f64>,
    pub(crate) total_pnl: Decimal,
    pub(crate) average_holding_bars: Option<f64>,
    pub(crate) largest_win: Option<f64>,
    pub(crate) largest_loss: Option<f64>,
}

impl SimulationResult {
    pub(crate) fn summary(&self) -> String {
        format!(
            "sample={}, layer={}, entry={}, target={}R, cost={}x, trades={}, win_rate_pct={}, average_win={}, average_loss={}, profit_factor={}, expectancy={}, expectancy_r={}, sharpe={}, sortino={}, max_drawdown_pct={}, cagr_pct={}, total_pnl={}, average_holding_bars={}, largest_win={}, largest_loss={}",
            self.sample.label,
            self.spec.layer.label(),
            self.spec.variant.label(),
            self.spec.risk_reward,
            self.spec.cost_multiple,
            self.metrics.trades,
            optional(self.metrics.win_rate_pct),
            optional(self.metrics.average_win),
            optional(self.metrics.average_loss),
            optional(self.metrics.profit_factor),
            optional(self.metrics.expectancy),
            optional(self.metrics.expectancy_r),
            optional(self.metrics.sharpe),
            optional(self.metrics.sortino),
            optional(self.metrics.max_drawdown_pct),
            optional(self.metrics.cagr_pct),
            self.metrics.total_pnl,
            optional(self.metrics.average_holding_bars),
            optional(self.metrics.largest_win),
            optional(self.metrics.largest_loss),
        )
    }

    pub(crate) fn monthly_returns(&self) -> BTreeMap<String, Decimal> {
        let mut returns = BTreeMap::new();
        for trade in &self.trades {
            let month = trade.date.to_string()[..7].to_string();
            *returns.entry(month).or_default() += trade.net_pnl;
        }
        returns
    }

    pub(crate) fn cohort_lines(&self) -> Vec<String> {
        let mut cohorts = BTreeMap::<String, Vec<&TradeRecord>>::new();
        for trade in &self.trades {
            for key in [
                format!("side={}", trade.side),
                format!("symbol={}", trade.symbol),
                format!("regime={:?}", trade.regime),
                format!("time={:02}:00", trade.entry_minute / 60),
            ] {
                cohorts.entry(key).or_default().push(trade);
            }
        }
        cohorts
            .into_iter()
            .map(|(key, trades)| {
                let pnl: Decimal = trades.iter().map(|trade| trade.net_pnl).sum();
                let wins = trades
                    .iter()
                    .filter(|trade| trade.net_pnl > Decimal::ZERO)
                    .count();
                let win_rate = 100.0 * wins as f64 / trades.len() as f64;
                format!(
                    "cohort {key}: trades={}, win_rate_pct={win_rate:.4}, net_pnl={pnl}",
                    trades.len(),
                )
            })
            .collect()
    }
}

#[derive(Debug)]
struct SymbolState {
    feature: WyckoffFeatureEngine,
    detector: WyckoffStructureDetector,
    pending: Option<WyckoffSignal>,
    active: Option<ActiveTrade>,
    trades_today: usize,
    date: Option<Date>,
    session_disabled: bool,
    last_bar: Option<Bar>,
}

impl SymbolState {
    fn new(layer: StudyLayer) -> Self {
        Self {
            feature: WyckoffFeatureEngine::new(14, 12, 24),
            detector: WyckoffStructureDetector::new(WyckoffRules::default(), layer),
            pending: None,
            active: None,
            trades_today: 0,
            date: None,
            session_disabled: false,
            last_bar: None,
        }
    }

    fn observe(&mut self, completed: CompletedBar) {
        if self.date != Some(completed.date) {
            self.date = Some(completed.date);
            self.session_disabled = false;
            self.last_bar = None;
            self.detector.reset_session();
        }
        if self.last_bar.is_some_and(|previous| {
            previous
                .ts_event
                .checked_add(FIVE_MINUTE_NANOS)
                .is_none_or(|expected| expected != completed.bar.ts_event)
        }) {
            self.session_disabled = true;
            self.pending = None;
        }
        self.last_bar = Some(completed.bar);
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveTrade {
    signal: WyckoffSignal,
    entry_price: Decimal,
    stop: Decimal,
    target: Decimal,
    quantity: Decimal,
    initial_risk: Decimal,
    holding_bars: u64,
}

#[derive(Debug, Default)]
struct PortfolioState {
    date: Option<Date>,
    realized_today: Decimal,
    trades_today: usize,
}

impl PortfolioState {
    fn roll_date(&mut self, date: Date, states: &mut [SymbolState]) {
        if self.date == Some(date) {
            return;
        }
        self.date = Some(date);
        self.realized_today = Decimal::ZERO;
        self.trades_today = 0;
        for state in states {
            state.trades_today = 0;
        }
    }
}

/// 对指定样本、信号级别和成本场景执行一次确定性回测
pub(crate) fn simulate(
    config: &ResearchConfig,
    prepared: &[PreparedSymbol],
    sample: Sample,
    spec: SimulationSpec,
) -> anyhow::Result<SimulationResult> {
    let mut states = (0..prepared.len())
        .map(|_| SymbolState::new(spec.layer))
        .collect::<Vec<_>>();
    let sample_start = UnixNanos::from(sample.start);
    let sample_end = UnixNanos::from(sample.end);

    for (index, input) in prepared.iter().enumerate() {
        for bar in input
            .bars
            .iter()
            .copied()
            .filter(|bar| bar.ts_event < sample_start)
        {
            let completed = completed_bar(config, bar)?;
            states[index].observe(completed);
            if let Some(features) = states[index].feature.on_bar(completed) {
                states[index].detector.on_bar(features);
            }
        }
    }

    let mut events = BTreeMap::<UnixNanos, Vec<(usize, Bar)>>::new();
    let mut trading_dates = BTreeSet::new();
    for (index, input) in prepared.iter().enumerate() {
        for bar in input
            .bars
            .iter()
            .copied()
            .filter(|bar| bar.ts_event >= sample_start && bar.ts_event < sample_end)
        {
            let completed = completed_bar(config, bar)?;
            trading_dates.insert(completed.date);
            events.entry(bar.ts_event).or_default().push((index, bar));
        }
    }
    anyhow::ensure!(
        !events.is_empty(),
        "{} sample contains no bars",
        sample.label
    );

    let mut portfolio = PortfolioState::default();
    let mut trades = Vec::new();
    for (_timestamp, bars) in events {
        let first = completed_bar(config, bars[0].1)?;
        portfolio.roll_date(first.date, &mut states);
        for (index, bar) in bars.iter().copied() {
            states[index].observe(completed_bar(config, bar)?);
        }

        let mut candidates = bars
            .iter()
            .filter_map(|(index, bar)| {
                let pending = states[*index].pending?;
                let expected = pending.ts_event.checked_add(FIVE_MINUTE_NANOS)?;
                (bar.ts_event == expected).then_some((*index, pending))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|(left_index, left), (right_index, right)| {
            right
                .evidence_count
                .cmp(&left.evidence_count)
                .then_with(|| {
                    prepared[*left_index]
                        .configured
                        .instrument_id
                        .cmp(&prepared[*right_index].configured.instrument_id)
                })
        });
        for (index, pending) in candidates {
            states[index].pending = None;
            if open_positions(&states) >= config.max_open_positions
                || portfolio.trades_today >= config.max_trades_per_day
                || states[index].trades_today >= config.max_trades_per_symbol
                || portfolio.realized_today <= -config.daily_loss_limit
                || states[index].active.is_some()
                || states[index].session_disabled
            {
                continue;
            }
            let bar = bars
                .iter()
                .find_map(|(bar_index, bar)| (*bar_index == index).then_some(*bar))
                .expect("entry candidate came from this timestamp group");
            if let Some(active) = prepare_entry(config, &prepared[index], pending, bar, spec)? {
                states[index].active = Some(active);
                states[index].trades_today += 1;
                portfolio.trades_today += 1;
            }
        }
        for (index, bar) in bars.iter().copied() {
            if let Some(active) = states[index].active.as_mut() {
                active.holding_bars += 1;
                let completed = completed_bar(config, bar)?;
                let exit = exit_price(active, bar, completed.minute, config.flatten_minute);
                if let Some(raw_exit) = exit {
                    let active = states[index].active.take().expect("borrow ended above");
                    let trade = close_trade(
                        config,
                        &prepared[index],
                        active,
                        raw_exit,
                        completed.date,
                        spec.cost_multiple,
                    )?;
                    portfolio.realized_today += trade.net_pnl;
                    trades.push(trade);
                }
            }
        }

        for (index, bar) in bars {
            if states[index].pending.is_some_and(|pending| {
                pending
                    .ts_event
                    .checked_add(FIVE_MINUTE_NANOS)
                    .is_none_or(|expected| expected < bar.ts_event)
            }) {
                states[index].pending = None;
            }
            let completed = completed_bar(config, bar)?;
            let Some(features) = states[index].feature.on_bar(completed) else {
                continue;
            };
            let signals = states[index].detector.on_bar(features);
            if states[index].active.is_some()
                || states[index].pending.is_some()
                || portfolio.realized_today <= -config.daily_loss_limit
                || states[index].session_disabled
                || completed.minute + 5 < config.entry_start_minute
                || completed.minute + 5 > config.entry_end_minute
            {
                continue;
            }
            states[index].pending = signals
                .into_iter()
                .find(|signal| signal.variant == spec.variant);
        }
    }

    anyhow::ensure!(
        states.iter().all(|state| state.active.is_none()),
        "sample ended with an open position; flatten_time has no matching completed Bar",
    );
    let metrics = calculate_metrics(
        &trades,
        &trading_dates,
        config.starting_balance.as_decimal(),
    );
    Ok(SimulationResult {
        sample,
        spec,
        trades,
        metrics,
    })
}

fn completed_bar(config: &ResearchConfig, bar: Bar) -> anyhow::Result<CompletedBar> {
    let local = bar
        .ts_event
        .to_datetime_utc()
        .to_zoned(config.timezone.clone());
    Ok(CompletedBar {
        bar,
        date: local.date(),
        minute: u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?,
    })
}

fn open_positions(states: &[SymbolState]) -> usize {
    states.iter().filter(|state| state.active.is_some()).count()
}

fn prepare_entry(
    config: &ResearchConfig,
    input: &PreparedSymbol,
    signal: WyckoffSignal,
    bar: Bar,
    spec: SimulationSpec,
) -> anyhow::Result<Option<ActiveTrade>> {
    let increment = input.configured.price_increment.as_decimal();
    let raw_open = bar.open.as_decimal();
    let execution_bps =
        (config.spread_bps / Decimal::TWO + config.slippage_bps) * spec.cost_multiple;
    let entry = match signal.side {
        OrderSide::Buy => round_up(
            raw_open * (Decimal::ONE + execution_bps / Decimal::from(BASIS_POINTS)),
            increment,
        ),
        OrderSide::Sell => round_down(
            raw_open * (Decimal::ONE - execution_bps / Decimal::from(BASIS_POINTS)),
            increment,
        ),
        OrderSide::NoOrderSide => return Ok(None),
    };
    let atr = Decimal::from_f64_retain(signal.atr).context("ATR cannot convert to Decimal")?;
    let anchor =
        Decimal::from_f64_retain(signal.stop_anchor).context("stop cannot convert to Decimal")?;
    let buffer = atr
        * Decimal::from_f64_retain(config.stop_buffer_atr)
            .context("stop ATR buffer cannot convert to Decimal")?;
    let minimum_distance = atr * Decimal::new(25, 2);
    let stop = match signal.side {
        OrderSide::Buy => round_down((anchor - buffer).min(entry - minimum_distance), increment),
        OrderSide::Sell => round_up((anchor + buffer).max(entry + minimum_distance), increment),
        OrderSide::NoOrderSide => return Ok(None),
    };
    let per_share_risk = match signal.side {
        OrderSide::Buy => entry - stop,
        OrderSide::Sell => stop - entry,
        OrderSide::NoOrderSide => return Ok(None),
    };
    if entry <= Decimal::ZERO || stop <= Decimal::ZERO || per_share_risk <= Decimal::ZERO {
        return Ok(None);
    }
    let volatility_multiple = match signal.volatility {
        VolatilityRegime::High => Decimal::new(5, 1),
        VolatilityRegime::Normal => Decimal::ONE,
        VolatilityRegime::Low => Decimal::new(125, 2),
    };
    let risk_budget = config.risk_amount * volatility_multiple;
    let per_share_cost = config.round_trip_cost_per_share * spec.cost_multiple;
    let risk_quantity = (risk_budget / (per_share_risk + per_share_cost)).floor();
    let notional_quantity = (config.max_order_notional / entry).floor();
    let requested = risk_quantity
        .min(notional_quantity)
        .min(config.max_order_quantity.as_decimal());
    if requested < Decimal::ONE {
        return Ok(None);
    }
    let quantity = input
        .instrument
        .make_qty_from_decimal(requested, Some(true))
        .as_decimal();
    if quantity <= Decimal::ZERO {
        return Ok(None);
    }
    let target = match signal.side {
        OrderSide::Buy => round_down(entry + per_share_risk * spec.risk_reward, increment),
        OrderSide::Sell => round_up(entry - per_share_risk * spec.risk_reward, increment),
        OrderSide::NoOrderSide => return Ok(None),
    };
    if target <= Decimal::ZERO {
        return Ok(None);
    }
    Ok(Some(ActiveTrade {
        signal,
        entry_price: entry,
        stop,
        target,
        quantity,
        initial_risk: per_share_risk * quantity,
        holding_bars: 0,
    }))
}

fn exit_price(active: &ActiveTrade, bar: Bar, minute: u16, flatten_minute: u16) -> Option<Decimal> {
    let high = bar.high.as_decimal();
    let low = bar.low.as_decimal();
    let open = bar.open.as_decimal();
    let close = bar.close.as_decimal();
    let (stop_hit, target_hit) = match active.signal.side {
        OrderSide::Buy => (low <= active.stop, high >= active.target),
        OrderSide::Sell => (high >= active.stop, low <= active.target),
        OrderSide::NoOrderSide => return None,
    };
    if stop_hit {
        return Some(match active.signal.side {
            OrderSide::Buy => open.min(active.stop),
            OrderSide::Sell => open.max(active.stop),
            OrderSide::NoOrderSide => unreachable!(),
        });
    }
    if target_hit {
        return Some(active.target);
    }
    (minute + 5 >= flatten_minute).then_some(close)
}

fn close_trade(
    config: &ResearchConfig,
    input: &PreparedSymbol,
    active: ActiveTrade,
    raw_exit: Decimal,
    date: Date,
    cost_multiple: Decimal,
) -> anyhow::Result<TradeRecord> {
    let increment = input.configured.price_increment.as_decimal();
    let execution_bps = (config.spread_bps / Decimal::TWO + config.slippage_bps) * cost_multiple;
    let exit_price = match active.signal.side {
        OrderSide::Buy => round_down(
            raw_exit * (Decimal::ONE - execution_bps / Decimal::from(BASIS_POINTS)),
            increment,
        ),
        OrderSide::Sell => round_up(
            raw_exit * (Decimal::ONE + execution_bps / Decimal::from(BASIS_POINTS)),
            increment,
        ),
        OrderSide::NoOrderSide => anyhow::bail!("active trade side is unspecified"),
    };
    let signed_move = match active.signal.side {
        OrderSide::Buy => exit_price - active.entry_price,
        OrderSide::Sell => active.entry_price - exit_price,
        OrderSide::NoOrderSide => unreachable!(),
    };
    let gross_pnl = signed_move * active.quantity;
    let commission = config.round_trip_cost_per_share * cost_multiple * active.quantity;
    Ok(TradeRecord {
        symbol: input.configured.instrument_id.symbol.to_string(),
        side: active.signal.side,
        regime: active.signal.regime,
        entry_minute: active.signal.minute + 5,
        date,
        initial_risk: active.initial_risk,
        net_pnl: gross_pnl - commission,
        holding_bars: active.holding_bars,
    })
}

fn calculate_metrics(
    trades: &[TradeRecord],
    trading_dates: &BTreeSet<Date>,
    starting_balance: Decimal,
) -> Metrics {
    if trades.is_empty() {
        return Metrics::default();
    }
    let total_pnl: Decimal = trades.iter().map(|trade| trade.net_pnl).sum();
    let wins = trades
        .iter()
        .filter(|trade| trade.net_pnl > Decimal::ZERO)
        .collect::<Vec<_>>();
    let losses = trades
        .iter()
        .filter(|trade| trade.net_pnl < Decimal::ZERO)
        .collect::<Vec<_>>();
    let gross_profit: Decimal = wins.iter().map(|trade| trade.net_pnl).sum();
    let gross_loss: Decimal = losses.iter().map(|trade| -trade.net_pnl).sum();
    let total_r: f64 = trades
        .iter()
        .filter_map(|trade| (trade.net_pnl / trade.initial_risk).to_f64())
        .sum();
    let daily_pnl = trading_dates
        .iter()
        .map(|date| {
            trades
                .iter()
                .filter(|trade| &trade.date == date)
                .map(|trade| trade.net_pnl)
                .sum::<Decimal>()
        })
        .collect::<Vec<_>>();
    let risk = risk_metrics(&daily_pnl, starting_balance);
    Metrics {
        trades: trades.len(),
        win_rate_pct: Some(100.0 * wins.len() as f64 / trades.len() as f64),
        average_win: decimal_average(wins.iter().map(|trade| trade.net_pnl), wins.len()),
        average_loss: decimal_average(losses.iter().map(|trade| trade.net_pnl), losses.len()),
        profit_factor: ratio(gross_profit, gross_loss),
        expectancy: (total_pnl / Decimal::from(trades.len() as u64)).to_f64(),
        expectancy_r: Some(total_r / trades.len() as f64),
        sharpe: risk.sharpe,
        sortino: risk.sortino,
        max_drawdown_pct: risk.max_drawdown_pct,
        cagr_pct: risk.cagr_pct,
        total_pnl,
        average_holding_bars: Some(
            trades
                .iter()
                .map(|trade| trade.holding_bars as f64)
                .sum::<f64>()
                / trades.len() as f64,
        ),
        largest_win: wins
            .iter()
            .filter_map(|trade| trade.net_pnl.to_f64())
            .max_by(f64::total_cmp),
        largest_loss: losses
            .iter()
            .filter_map(|trade| trade.net_pnl.to_f64())
            .min_by(f64::total_cmp),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RiskMetrics {
    sharpe: Option<f64>,
    sortino: Option<f64>,
    max_drawdown_pct: Option<f64>,
    cagr_pct: Option<f64>,
}

fn risk_metrics(daily_pnl: &[Decimal], starting_balance: Decimal) -> RiskMetrics {
    if daily_pnl.is_empty() || starting_balance <= Decimal::ZERO {
        return RiskMetrics::default();
    }
    let Some(starting_balance_f64) = starting_balance.to_f64() else {
        return RiskMetrics::default();
    };
    let mut equity = starting_balance_f64;
    let mut peak = equity;
    let mut maximum_drawdown = 0.0_f64;
    let mut returns = Vec::with_capacity(daily_pnl.len());
    for pnl in daily_pnl {
        let Some(pnl) = pnl.to_f64() else {
            return RiskMetrics::default();
        };
        let previous = equity;
        equity += pnl;
        returns.push(if previous == 0.0 { 0.0 } else { pnl / previous });
        peak = peak.max(equity);
        if peak > 0.0 {
            maximum_drawdown = maximum_drawdown.min((equity - peak) / peak);
        }
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let stddev = sample_deviation(&returns, mean);
    let downside = returns
        .iter()
        .copied()
        .filter(|value| *value < 0.0)
        .collect::<Vec<_>>();
    let downside_deviation = if downside.is_empty() {
        None
    } else {
        Some(
            (downside.iter().map(|value| value * value).sum::<f64>() / downside.len() as f64)
                .sqrt(),
        )
    };
    let years = daily_pnl.len() as f64 / 252.0;
    RiskMetrics {
        sharpe: stddev
            .filter(|value| *value > 0.0)
            .map(|value| mean / value * 252.0_f64.sqrt()),
        sortino: downside_deviation
            .filter(|value| *value > 0.0)
            .map(|value| mean / value * 252.0_f64.sqrt()),
        max_drawdown_pct: Some(maximum_drawdown * 100.0),
        cagr_pct: (equity > 0.0 && years > 0.0)
            .then_some(((equity / starting_balance_f64).powf(1.0 / years) - 1.0) * 100.0),
    }
}

fn sample_deviation(values: &[f64], mean: f64) -> Option<f64> {
    (values.len() > 1).then(|| {
        (values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / (values.len() - 1) as f64)
            .sqrt()
    })
}

fn decimal_average(values: impl Iterator<Item = Decimal>, count: usize) -> Option<f64> {
    (count > 0)
        .then(|| values.sum::<Decimal>() / Decimal::from(count))
        .and_then(|value| value.to_f64())
}

fn ratio(numerator: Decimal, denominator: Decimal) -> Option<f64> {
    (denominator > Decimal::ZERO)
        .then(|| numerator / denominator)
        .and_then(|value| value.to_f64())
}

fn round_down(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).floor() * increment
}

fn round_up(value: Decimal, increment: Decimal) -> Decimal {
    (value / increment).ceil() * increment
}

fn optional(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| format!("{value:.4}"))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn simultaneous_stop_and_target_is_counted_as_stop() {
        let signal = WyckoffSignal {
            side: OrderSide::Buy,
            variant: EntryVariant::SpringImmediate,
            stop_anchor: 99.0,
            atr: 1.0,
            regime: TrendRegime::Ranging,
            volatility: VolatilityRegime::Normal,
            minute: 600,
            ts_event: UnixNanos::from(1),
            evidence_count: 3,
        };
        let active = ActiveTrade {
            signal,
            entry_price: Decimal::from(100),
            stop: Decimal::from(99),
            target: Decimal::from(102),
            quantity: Decimal::ONE,
            initial_risk: Decimal::ONE,
            holding_bars: 1,
        };
        let bar = Bar::new(
            "AAPL.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL".into(),
            "100".into(),
            "103".into(),
            "98".into(),
            "101".into(),
            "100".into(),
            UnixNanos::from(1),
            UnixNanos::from(2),
        );

        assert_eq!(exit_price(&active, bar, 600, 950), Some(Decimal::from(99)));
    }

    #[rstest]
    fn flat_daily_returns_remain_in_sharpe_sample() {
        let metrics = risk_metrics(
            &[Decimal::from(100), Decimal::ZERO, Decimal::from(-50)],
            Decimal::from(100_000),
        );

        assert!(metrics.sharpe.is_some());
        assert!(metrics.max_drawdown_pct.is_some_and(|value| value < 0.0));
    }
}
