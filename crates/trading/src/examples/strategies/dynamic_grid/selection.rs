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

//! 网格候选研究：日线衡量往返路径，信号周期复用生产 ATR/间距，不生成订单或账户分配。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use nautilus_model::{
    data::{Bar, QuoteTick},
    enums::{BarAggregation, PriceType},
    identifiers::InstrumentId,
    types::{Price, Quantity},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use super::{
    config::{GridConfig, TrendPolicy},
    engine::{GridEngine, spacing},
    portfolio::{PortfolioConfig, PortfolioRiskManager},
    regime::{MarketRegime, Observation, RegimeDetector, RegimeSnapshot, rebound_opportunities},
    regime_filter::RegimeFilter,
    stock::StockMarketState,
};

const DAY_NS: u64 = 86_400_000_000_000;

/// 独立选股参数；不会覆盖任何实盘风控或标的配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridSelectionConfig {
    /// 日线收益与往返路径的交易日窗口，另需一根起始收盘价。
    pub lookback_sessions: usize,
    /// 最多输出的候选数量，不强制填满。
    pub max_candidates: usize,
    /// 同一配置行业最多保留的候选数。
    pub max_per_sector: usize,
    /// 入选分数下限，范围 0–100；只是研究假设。
    pub minimum_score: f64,
    /// 窗口内达到满分所需的收盘价确认反弹次数。
    pub target_rebounds: usize,
    /// 日均成交额代理值 `close * volume` 的下限，单位美元。
    pub minimum_average_dollar_volume: Decimal,
    /// 当前报价最大年龄；成交所需的新鲜度仍由执行层再次校验。
    pub max_quote_age_secs: u64,
    /// 非交易时段允许保留的历史最大日历年龄。
    pub max_history_age_days: u64,
    /// 每只标的建网格的预算探针，不是账户资金分配。
    pub grid_capital: Decimal,
    /// 每边最低佣金假设，需按账户实际收费配置。
    pub minimum_commission_per_order: Decimal,
    /// 候选间正相关上限；负相关不作为同向集中惩罚。
    pub maximum_correlation: f64,
    /// 相关性最少对齐收益区间数；不足时不假设独立。
    pub minimum_correlation_observations: usize,
}

impl Default for GridSelectionConfig {
    fn default() -> Self {
        Self {
            lookback_sessions: 60,
            max_candidates: 5,
            max_per_sector: 2,
            minimum_score: 55.0,
            target_rebounds: 6,
            minimum_average_dollar_volume: Decimal::from(20_000_000),
            max_quote_age_secs: 120,
            max_history_age_days: 7,
            grid_capital: Decimal::from(10_000),
            minimum_commission_per_order: Decimal::ZERO,
            maximum_correlation: 0.8,
            minimum_correlation_observations: 30,
        }
    }
}

impl GridSelectionConfig {
    /// 检查窗口、阈值及研究预算。
    ///
    /// # Errors
    /// 参数越界、非有限或预算非正时返回错误。
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (20..=252).contains(&self.lookback_sessions)
                && (1..=100).contains(&self.max_candidates)
                && (1..=self.max_candidates).contains(&self.max_per_sector)
                && self.target_rebounds > 0
                && self.minimum_score.is_finite()
                && (0.0..=100.0).contains(&self.minimum_score)
                && self.maximum_correlation.is_finite()
                && (0.0..=1.0).contains(&self.maximum_correlation)
                && (2..=self.lookback_sessions).contains(&self.minimum_correlation_observations)
                && (1..=3600).contains(&self.max_quote_age_secs)
                && (1..=30).contains(&self.max_history_age_days)
                && self.grid_capital > Decimal::ZERO
                && self.minimum_average_dollar_volume > Decimal::ZERO
                && self.minimum_commission_per_order >= Decimal::ZERO,
            "Invalid grid selection configuration"
        );
        Ok(())
    }
}

/// 一个标的的可回放输入。所有 Bar 的时间戳必须是完成时间，行情不得混入其他标的。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridSelectionInput {
    /// Nautilus 标的标识。
    pub instrument_id: InstrumentId,
    /// 显式配置的行业分组，不从股票名字猜测。
    pub sector: String,
    /// 生产网格参数的快照。
    pub grid: GridConfig,
    /// 最小报价单位。
    pub price_increment: Price,
    /// 最小交易单位。
    pub lot_size: Quantity,
    /// 已完成的未复权常规时段日线。
    pub daily_bars: Vec<Bar>,
    /// 与生产策略相同周期、未复权的已完成信号 Bar。
    pub signal_bars: Vec<Bar>,
    /// 已观测买卖报价；缺失时不得伪造零价差。
    pub quote: Option<QuoteTick>,
}

/// 排名的可解释诊断；不是回测收益或成交预测。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridSelectionMetrics {
    /// 实际用于研究的交易日收益区间数。
    pub sessions: usize,
    /// 日均 `close * volume` 代理成交额。
    pub average_dollar_volume: Decimal,
    /// 最近一个信号 Bar 的收盘价。
    pub last_price: Decimal,
    /// 网格核心计算出的当前实际间距。
    pub effective_spacing: Decimal,
    /// 当前报价的相对价差，单位基点。
    pub spread_bps: Decimal,
    /// 第一笔可负担的下方网格买单数量，仅用于成本探针。
    pub probe_quantity: Decimal,
    /// 该买卖对扣除双边费用、滑点、价差及安全边际的假设净价差。
    pub probe_cycle_edge: Decimal,
    /// 因整手取整为零的下方层数。
    pub zero_quantity_levels: usize,
    /// 收盘路径下降一个间距后，再上升一个间距的完成次数。
    pub confirmed_rebounds: usize,
    /// 下跌确认至反弹确认的平均交易日数；未完成反弹不计入此值。
    pub average_rebound_sessions: Option<f64>,
    /// 最后一次下跌确认后尚未反弹的交易日数，避免只看完成周期。
    pub unresolved_decline_sessions: usize,
    /// 净方向位移除以全部收盘绝对变化，越低越往返。
    pub directional_efficiency: f64,
    /// 日收盘峰谷回撤，包含未反弹的路径。
    pub maximum_drawdown: f64,
    /// 绝对开盘跳空占真实日波幅总和的比例，作为软惩罚而非禁买开关。
    pub gap_share: f64,
    /// 四项等权分数：往返次数、非方向性、成本余量、库存路径质量。
    pub score_components: [f64; 4],
    /// 分类周期快照：股票模式为 15 分钟，LegacyDgt 为原信号周期；间距仍使用分钟 ATR。
    pub regime: RegimeSnapshot,
}

/// 包含拒绝原因和观察提示的一条候选结果。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridCandidate {
    /// 标的标识。
    pub instrument_id: InstrumentId,
    /// 行业分组。
    pub sector: String,
    /// 数据与成本检查失败时没有分数。
    pub score: Option<f64>,
    /// 是否进入本次建议池；不表示自动允许下单。
    pub selected: bool,
    /// 未入选原因，可包含数据、分数、行业或相关性限制。
    pub reasons: Vec<String>,
    /// 不作为机械停买条件的研究风险提示。
    pub warnings: Vec<String>,
    /// 成功计算时的完整指标。
    pub metrics: Option<GridSelectionMetrics>,
}

/// 确定性候选报告，不含订单意图和资本分配。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridSelectionReport {
    /// 冻结评分口径的版本。
    pub method_version: String,
    /// 决策时刻；只使用不晚于此时刻的观测。
    pub as_of_ns: u64,
    /// 按得分从高到低的候选建议池。
    pub selected: Vec<InstrumentId>,
    /// 全部标的及其入选/未入选原因。
    pub ranked: Vec<GridCandidate>,
}

/// 根据已完成数据研究候选，并复用组合模块进行行业/正相关去重。
///
/// # Errors
/// 全局配置无效或标的重复时返回错误。单标的数据错误保留在报告中，不静默消失。
pub fn select_grid_candidates(
    config: &GridSelectionConfig,
    inputs: &[GridSelectionInput],
    as_of_ns: u64,
) -> anyhow::Result<GridSelectionReport> {
    config.validate()?;
    let mut ids = BTreeSet::new();
    let mut portfolio = PortfolioRiskManager::new(config.grid_capital);
    let correlation_config = PortfolioConfig {
        correlation_lookback_days: config.lookback_sessions * 3 + 14,
        correlation_min_observations: config.minimum_correlation_observations,
        ..Default::default()
    };
    let mut ranked = Vec::with_capacity(inputs.len());
    for input in inputs {
        anyhow::ensure!(
            ids.insert(input.instrument_id),
            "Duplicate selection instrument"
        );
        let mut candidate = GridCandidate {
            instrument_id: input.instrument_id,
            sector: input.sector.clone(),
            score: None,
            selected: false,
            reasons: Vec::new(),
            warnings: vec!["EARNINGS_NEWS_AND_CORPORATE_ACTIONS_REQUIRE_REVIEW".into()],
            metrics: None,
        };
        match assess(config, input, as_of_ns) {
            Ok(metrics) => {
                candidate.score = Some(25.0 * metrics.score_components.iter().sum::<f64>());
                if metrics.regime.regime != MarketRegime::Range {
                    candidate
                        .warnings
                        .push(format!("CURRENT_REGIME_{:?}", metrics.regime.regime));
                }
                if metrics.zero_quantity_levels > 0 {
                    candidate.warnings.push("SOME_LEVELS_ROUND_TO_ZERO".into());
                }
                if metrics.unresolved_decline_sessions > 0 {
                    candidate
                        .warnings
                        .push("UNRESOLVED_DECLINE_IN_RESEARCH_WINDOW".into());
                }
                for bar in completed_tail(&input.daily_bars, as_of_ns, config.lookback_sessions + 1)
                {
                    portfolio.close(
                        &correlation_config,
                        input.instrument_id,
                        bar.ts_event.as_u64(),
                        bar.close.as_decimal(),
                    );
                }
                candidate.metrics = Some(metrics);
            }
            Err(e) => candidate.reasons.push(format!("{e:#}")),
        }
        ranked.push(candidate);
    }
    ranked.sort_by(|a, b| {
        b.score
            .unwrap_or(-1.0)
            .total_cmp(&a.score.unwrap_or(-1.0))
            .then_with(|| {
                a.instrument_id
                    .to_string()
                    .cmp(&b.instrument_id.to_string())
            })
    });
    let mut selected = Vec::new();
    let mut sectors: BTreeMap<String, usize> = BTreeMap::new();
    for candidate in &mut ranked {
        let Some(score) = candidate.score else {
            continue;
        };
        if score < config.minimum_score {
            candidate.reasons.push("BELOW_MINIMUM_SCORE".into());
        } else if selected.len() >= config.max_candidates {
            candidate.reasons.push("CANDIDATE_CAPACITY".into());
        } else if sectors.get(&candidate.sector).copied().unwrap_or_default()
            >= config.max_per_sector
        {
            candidate.reasons.push("SECTOR_CONCENTRATION".into());
        } else {
            for other in &selected {
                match portfolio.correlation(
                    &correlation_config,
                    candidate.instrument_id,
                    *other,
                    as_of_ns,
                ) {
                    Some(value) if value <= config.maximum_correlation => {}
                    Some(value) => candidate
                        .reasons
                        .push(format!("CORRELATED_WITH_{other}:{value:.4}")),
                    None => candidate
                        .reasons
                        .push(format!("CORRELATION_UNAVAILABLE_WITH_{other}")),
                }
            }
            if candidate.reasons.is_empty() {
                candidate.selected = true;
                selected.push(candidate.instrument_id);
                *sectors.entry(candidate.sector.clone()).or_default() += 1;
            }
        }
    }
    Ok(GridSelectionReport {
        method_version: "grid-suitability-v1".into(),
        as_of_ns,
        selected,
        ranked,
    })
}

fn completed_tail(bars: &[Bar], as_of_ns: u64, count: usize) -> Vec<&Bar> {
    let mut completed: Vec<_> = bars
        .iter()
        .filter(|bar| bar.ts_event.as_u64() <= as_of_ns)
        .rev()
        .take(count)
        .collect();
    completed.reverse();
    completed
}

fn validate_bars(bars: &[Bar], id: InstrumentId, daily: bool, as_of_ns: u64) -> anyhow::Result<()> {
    // 尚未发生的数据连有效性判断也不参与，否则未来的坏记录仍会泄漏进当前排名。
    let bars = completed_tail(bars, as_of_ns, usize::MAX);
    anyhow::ensure!(!bars.is_empty(), "MISSING_BARS");
    let kind = bars[0].bar_type;
    anyhow::ensure!(
        kind.instrument_id() == id
            && kind.spec().price_type == PriceType::Last
            && if daily {
                kind.spec().aggregation == BarAggregation::Day && kind.spec().step.get() == 1
            } else {
                matches!(
                    kind.spec().aggregation,
                    BarAggregation::Minute | BarAggregation::Hour
                )
            },
        "WRONG_BAR_TYPE"
    );
    anyhow::ensure!(
        bars.windows(2).all(|p| p[0].ts_event < p[1].ts_event),
        "UNORDERED_OR_DUPLICATE_BARS"
    );
    for bar in bars {
        anyhow::ensure!(
            bar.bar_type == kind
                && bar.low > Price::zero(bar.low.precision)
                && bar.low <= bar.open
                && bar.open <= bar.high
                && bar.low <= bar.close
                && bar.close <= bar.high,
            "INVALID_OR_MIXED_BARS"
        );
    }
    Ok(())
}

fn assess(
    config: &GridSelectionConfig,
    input: &GridSelectionInput,
    as_of_ns: u64,
) -> anyhow::Result<GridSelectionMetrics> {
    input.grid.validate()?;
    anyhow::ensure!(!input.sector.trim().is_empty(), "MISSING_SECTOR");
    validate_bars(&input.daily_bars, input.instrument_id, true, as_of_ns)?;
    validate_bars(&input.signal_bars, input.instrument_id, false, as_of_ns)?;
    let daily = completed_tail(&input.daily_bars, as_of_ns, config.lookback_sessions + 1);
    anyhow::ensure!(
        daily.len() == config.lookback_sessions + 1,
        "INSUFFICIENT_DAILY_HISTORY"
    );
    let daily_last = daily.last().context("MISSING_DAILY_HISTORY")?;
    anyhow::ensure!(
        as_of_ns - daily_last.ts_event.as_u64() <= config.max_history_age_days * DAY_NS,
        "STALE_DAILY_HISTORY"
    );
    let signal_limit = input
        .grid
        .grid_scale_regime
        .as_ref()
        .filter(|c| c.mode != super::grid_scale::GridScaleMode::Shadow)
        .map_or(1000, |c| (c.lookback_sessions + 1) * 390);
    let signal = completed_tail(&input.signal_bars, as_of_ns, signal_limit);
    let last = signal.last().context("MISSING_COMPLETED_SIGNAL_BARS")?;
    anyhow::ensure!(
        as_of_ns - last.ts_event.as_u64() <= config.max_history_age_days * DAY_NS,
        "STALE_SIGNAL_HISTORY"
    );
    let quote = input.quote.context("MISSING_SPREAD_QUOTE")?;
    anyhow::ensure!(
        quote.instrument_id == input.instrument_id
            && quote.bid_price.is_positive()
            && quote.ask_price >= quote.bid_price
            && !quote.bid_size.is_zero()
            && !quote.ask_size.is_zero(),
        "INVALID_SPREAD_QUOTE"
    );
    anyhow::ensure!(
        quote.ts_init.as_u64() <= as_of_ns
            && as_of_ns
                .checked_sub(quote.ts_event.as_u64())
                .is_some_and(|age| age <= config.max_quote_age_secs * 1_000_000_000),
        "STALE_OR_FUTURE_QUOTE"
    );
    let spread = quote.ask_price.as_decimal() - quote.bid_price.as_decimal();
    let mid = (quote.ask_price.as_decimal() + quote.bid_price.as_decimal()) / Decimal::from(2);
    let spread_bps = spread / mid * Decimal::from(10000);
    anyhow::ensure!(
        spread_bps <= input.grid.maximum_spread_bps,
        "SPREAD_TOO_WIDE"
    );
    let price = last.close.as_decimal();
    anyhow::ensure!(price >= input.grid.minimum_price, "PRICE_BELOW_MINIMUM");
    let average_dollar_volume = daily
        .iter()
        .skip(1)
        .try_fold(Decimal::ZERO, |total, bar| {
            bar.close
                .as_decimal()
                .checked_mul(bar.volume.as_decimal())
                .and_then(|value| total.checked_add(value))
        })
        .context("DOLLAR_VOLUME_OVERFLOW")?
        / Decimal::from(config.lookback_sessions);
    anyhow::ensure!(
        average_dollar_volume >= config.minimum_average_dollar_volume,
        "INSUFFICIENT_DOLLAR_VOLUME"
    );
    let mut detector = RegimeDetector::default();
    let mut filter = RegimeFilter::enabled(&input.grid).then(|| RegimeFilter::new(&input.grid));
    if filter.is_some() {
        RegimeFilter::validate_bar_type(&input.grid, last.bar_type)?;
    }
    for bar in &signal {
        if filter.is_some() && !StockMarketState::is_regular_session_bar(bar.ts_event.as_u64()) {
            continue;
        }
        detector.update(
            &input.grid,
            Observation {
                ts_ns: bar.ts_event.as_u64(),
                high: bar.high.as_f64(),
                low: bar.low.as_f64(),
                close: bar.close.as_f64(),
            },
        )?;
        if let Some(filter) = &mut filter {
            let scale = if input.grid.grid_scale_regime.is_some() {
                Some(spacing(
                    &input.grid,
                    Decimal::from_f64_retain(detector.snapshot.atr).context("INVALID_ATR")?,
                    bar.close.as_decimal(),
                    Decimal::ONE,
                )?)
            } else {
                None
            };
            filter.update(&input.grid, bar, scale)?;
        }
    }
    anyhow::ensure!(detector.snapshot.initialized, "INDICATOR_WARMUP");
    let regime = if let Some(filter) = &filter {
        anyhow::ensure!(
            filter.ready(&input.grid, &detector.snapshot, last.ts_event.as_u64())
                && filter.entry_confirmed(&input.grid),
            "REGIME_CONFIRMATION_WARMUP"
        );
        let mut snapshot = filter.source().clone();
        snapshot.regime = filter.regime(&input.grid, &detector.snapshot);
        if snapshot.regime != filter.source().regime {
            snapshot.reason = Some("EFFECTIVE_REGIME_OVERRIDE".to_string());
        }
        snapshot
    } else {
        detector.snapshot.clone()
    };
    let multiplier = if regime.regime.policy(&input.grid) == TrendPolicy::WiderGrid {
        input.grid.trend_spacing_multiplier
    } else {
        Decimal::ONE
    };
    let atr = Decimal::from_f64_retain(detector.snapshot.atr).context("INVALID_ATR")?;
    let effective_spacing = spacing(&input.grid, atr, price, multiplier)?;
    let budget = config
        .grid_capital
        .min(input.grid.max_grid_exposure)
        .min(input.grid.max_notional);
    let grid = GridEngine::build(
        &input.grid,
        1,
        price,
        effective_spacing,
        budget,
        input.price_increment.as_decimal(),
        input.lot_size.as_decimal(),
        as_of_ns,
    )?;
    let zero_quantity_levels = grid
        .levels
        .iter()
        .filter(|l| l.level_index < 0 && l.quantity.is_zero())
        .count();
    let probe = grid
        .levels
        .iter()
        .find(|l| l.level_index < 0 && l.quantity > Decimal::ZERO)
        .context("ALL_BUY_LEVELS_ROUND_TO_ZERO")?;
    let entry_value = probe.price * probe.quantity;
    let exit_value = probe.exit_price * probe.quantity;
    let fee_rate = input.grid.maker_fee.max(input.grid.taker_fee) + input.grid.commission;
    let costs = (entry_value * fee_rate).max(config.minimum_commission_per_order)
        + (exit_value * fee_rate).max(config.minimum_commission_per_order)
        + (entry_value + exit_value) * input.grid.slippage
        + spread * probe.quantity
        + entry_value * input.grid.minimum_profit_margin;
    let gross = exit_value - entry_value;
    let edge = gross - costs;
    anyhow::ensure!(edge > Decimal::ZERO, "GRID_DISABLED_BY_COST");
    let closes: Vec<_> = daily.iter().map(|b| b.close.as_decimal()).collect();
    let (rebounds, average_duration, unresolved) =
        rebound_opportunities(&closes, effective_spacing);
    let path: Decimal = closes.windows(2).map(|p| (p[1] - p[0]).abs()).sum();
    let efficiency = if path.is_zero() {
        1.0
    } else {
        ((closes[closes.len() - 1] - closes[0]).abs() / path)
            .to_f64()
            .context("INVALID_PATH")?
    };
    let mut peak = closes[0];
    let mut drawdown = Decimal::ZERO;
    let mut gaps = Decimal::ZERO;
    let mut ranges = Decimal::ZERO;
    for pair in daily.windows(2) {
        let previous = pair[0].close.as_decimal();
        let bar = pair[1];
        let close = bar.close.as_decimal();
        peak = peak.max(close);
        drawdown = drawdown.max(Decimal::ONE - close / peak);
        gaps += (bar.open.as_decimal() - previous).abs();
        ranges += bar.high.as_decimal().max(previous) - bar.low.as_decimal().min(previous);
    }
    let gap_share = if ranges.is_zero() {
        0.0
    } else {
        (gaps / ranges).to_f64().context("INVALID_GAP_SHARE")?
    };
    let maximum_drawdown = drawdown.to_f64().context("INVALID_DRAWDOWN")?;
    let components = [
        (rebounds as f64 / config.target_rebounds as f64).min(1.0),
        1.0 - efficiency,
        (edge / gross).to_f64().context("INVALID_EDGE")?,
        (1.0 - maximum_drawdown)
            * (1.0 - gap_share)
            * (1.0 - unresolved as f64 / config.lookback_sessions as f64),
    ];
    Ok(GridSelectionMetrics {
        sessions: config.lookback_sessions,
        average_dollar_volume,
        last_price: price,
        effective_spacing,
        spread_bps,
        probe_quantity: probe.quantity,
        probe_cycle_edge: edge,
        zero_quantity_levels,
        confirmed_rebounds: rebounds,
        average_rebound_sessions: average_duration,
        unresolved_decline_sessions: unresolved,
        directional_efficiency: efficiency,
        maximum_drawdown,
        gap_share,
        score_components: components,
        regime,
    })
}

#[cfg(test)]
mod tests {
    use nautilus_model::{data::BarType, enums::BarAggregation};
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    fn input(symbol: &str) -> (GridSelectionInput, u64) {
        let id = InstrumentId::from(format!("{symbol}.US.LONGBRIDGE"));
        let daily_type: BarType = format!("{id}-1-DAY-LAST-EXTERNAL").parse().unwrap();
        let signal_type: BarType = format!("{id}-1-MINUTE-LAST-EXTERNAL").parse().unwrap();
        let as_of = 1000 * DAY_NS;
        let grid = GridConfig {
            min_spacing_pct: dec!(0.02),
            max_spacing_pct: dec!(0.05),
            spacing_pct: dec!(0.02),
            grid_levels: 5,
            capital: dec!(100000),
            max_grid_exposure: dec!(20000),
            max_notional: dec!(20000),
            ..Default::default()
        };
        let daily_bars = (0..61)
            .map(|i| {
                let close = [100, 97, 103, 99, 102][i % 5];
                bar(daily_type, close, (939 + i as u64) * DAY_NS)
            })
            .collect();
        let signal_bars = (0..100)
            .map(|i| bar(signal_type, 100, as_of - (101 - i) * 60_000_000_000))
            .collect();
        let quote = QuoteTick::new(
            id,
            Price::from("99.99"),
            Price::from("100.01"),
            Quantity::from(100),
            Quantity::from(100),
            as_of.into(),
            as_of.into(),
        );
        (
            GridSelectionInput {
                instrument_id: id,
                sector: "Test sector".into(),
                grid,
                price_increment: Price::from("0.01"),
                lot_size: Quantity::from(1),
                daily_bars,
                signal_bars,
                quote: Some(quote),
            },
            as_of,
        )
    }

    fn bar(kind: BarType, close: i64, ts: u64) -> Bar {
        let price = Decimal::from(close);
        let high = price
            + if kind.spec().aggregation == BarAggregation::Day {
                dec!(4)
            } else {
                dec!(0.1)
            };
        let low = price
            - if kind.spec().aggregation == BarAggregation::Day {
                dec!(4)
            } else {
                dec!(0.1)
            };
        Bar::new(
            kind,
            Price::from_decimal_dp(price, 2).unwrap(),
            Price::from_decimal_dp(high, 2).unwrap(),
            Price::from_decimal_dp(low, 2).unwrap(),
            Price::from_decimal_dp(price, 2).unwrap(),
            Quantity::from(1_000_000),
            ts.into(),
            ts.into(),
        )
    }

    #[rstest]
    fn rebound_count_does_not_invent_gap_fills() {
        let closes = [100, 90, 110, 105, 70, 75].map(Decimal::from);
        assert_eq!(
            rebound_opportunities(&closes, dec!(0.05)),
            (2, Some(1.0), 0)
        );
        assert_eq!(
            rebound_opportunities(&[100, 99, 90, 80].map(Decimal::from), dec!(0.05)),
            (0, None, 1)
        );
        assert_eq!(
            rebound_opportunities(&[100, 110, 120].map(Decimal::from), dec!(0.05)),
            (0, None, 0)
        );
    }

    #[rstest]
    fn oscillation_beats_directional_inventory_accumulation() {
        let (range, now) = input("RANGE");
        let (mut down, _) = input("DOWN");
        let kind = down.daily_bars[0].bar_type;
        down.daily_bars = (0..61)
            .map(|i| bar(kind, 160 - i, (939 + i as u64) * DAY_NS))
            .collect();
        let report =
            select_grid_candidates(&GridSelectionConfig::default(), &[down, range], now).unwrap();
        let best = &report.ranked[0];
        let worst = &report.ranked[1];
        assert_eq!(best.instrument_id.symbol.as_str(), "RANGE.US");
        assert!(best.score.unwrap() > worst.score.unwrap());
        assert!(best.metrics.as_ref().unwrap().confirmed_rebounds > 0);
        assert_eq!(worst.metrics.as_ref().unwrap().confirmed_rebounds, 0);
        assert!(worst.metrics.as_ref().unwrap().unresolved_decline_sessions > 0);
        assert!(worst.metrics.as_ref().unwrap().maximum_drawdown > 0.3);
    }

    #[rstest]
    fn signal_period_atr_and_costs_use_the_production_core() {
        let (input, now) = input("A");
        let report =
            select_grid_candidates(&GridSelectionConfig::default(), &[input.clone()], now).unwrap();
        let metrics = report.ranked[0].metrics.as_ref().unwrap();
        assert!(metrics.regime.atr < 1.0); // 分钟 ATR，而非日线的 8 美元波幅
        assert_eq!(metrics.effective_spacing, dec!(0.02));
        assert!(metrics.probe_quantity > Decimal::ZERO);
        assert!(metrics.probe_cycle_edge > Decimal::ZERO);
        assert_eq!(metrics.spread_bps, dec!(2));
        assert!(
            metrics
                .score_components
                .iter()
                .all(|x| (0.0..=1.0).contains(x))
        );
        let config = GridSelectionConfig {
            minimum_commission_per_order: dec!(1000),
            ..Default::default()
        };
        let rejected = select_grid_candidates(&config, &[input], now).unwrap();
        assert_eq!(rejected.ranked[0].reasons, ["GRID_DISABLED_BY_COST"]);
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    fn candidate_scoring_requires_completed_stock_regime_confirmation(#[case] ready: bool) {
        let (mut input, now) = input("A");
        input.grid.strategy_mode = super::super::config::StrategyMode::StockAdaptive;
        let open = 1_735_828_200_000_000_000_u64;
        let signal_type = input.signal_bars[0].bar_type;
        input.signal_bars = (1..=390)
            .map(|minute| bar(signal_type, 100, open + minute * 60_000_000_000))
            .collect();
        if ready {
            input.signal_bars.extend(
                (1..=240)
                    .map(|minute| bar(signal_type, 100, open + DAY_NS + minute * 60_000_000_000)),
            );
        }
        let shift = input.signal_bars.last().unwrap().ts_event.as_u64() - now;
        for bar in &mut input.daily_bars {
            bar.ts_event = (bar.ts_event.as_u64() + shift).into();
            bar.ts_init = bar.ts_event;
        }
        let now = now + shift;
        input.quote.as_mut().unwrap().ts_event = now.into();
        input.quote.as_mut().unwrap().ts_init = now.into();
        let report =
            select_grid_candidates(&GridSelectionConfig::default(), &[input], now).unwrap();
        if ready {
            let metrics = report.ranked[0].metrics.as_ref().unwrap();
            assert_eq!(metrics.regime.regime, MarketRegime::Range);
            assert_eq!(metrics.regime.ts_ns, now);
            assert_eq!(metrics.effective_spacing, dec!(0.02));
        } else {
            assert_eq!(report.ranked[0].reasons, ["REGIME_CONFIRMATION_WARMUP"]);
        }
    }

    #[rstest]
    fn future_prices_do_not_change_current_selection() {
        let (original, now) = input("A");
        let mut extended = original.clone();
        extended
            .daily_bars
            .push(bar(original.daily_bars[0].bar_type, 500, now + DAY_NS));
        extended.signal_bars.push(bar(
            original.signal_bars[0].bar_type,
            500,
            now + 60_000_000_000,
        ));
        extended.signal_bars.last_mut().unwrap().low = Price::from("999.00");
        let config = GridSelectionConfig::default();
        assert_eq!(
            serde_json::to_value(select_grid_candidates(&config, &[original], now).unwrap())
                .unwrap(),
            serde_json::to_value(select_grid_candidates(&config, &[extended], now).unwrap())
                .unwrap()
        );
    }

    #[rstest]
    fn correlated_five_symbol_pool_is_not_treated_as_diversified() {
        let config = GridSelectionConfig {
            max_per_sector: 5,
            ..Default::default()
        };
        let (first, now) = input("A");
        let mut inputs = vec![first];
        for name in ["B", "C", "D", "E"] {
            inputs.push(input(name).0);
        }
        let report = select_grid_candidates(&config, &inputs, now).unwrap();
        assert_eq!(report.selected, [inputs[0].instrument_id]);
        assert!(
            report.ranked[1..]
                .iter()
                .all(|row| row.reasons[0].starts_with("CORRELATED_WITH_"))
        );
        inputs.reverse();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(select_grid_candidates(&config, &inputs, now).unwrap()).unwrap()
        );
    }

    #[rstest]
    fn sector_limit_and_unknown_correlation_are_explicit() {
        let (a, now) = input("A");
        let b = input("B").0;
        let config = GridSelectionConfig {
            max_per_sector: 1,
            ..Default::default()
        };
        let report = select_grid_candidates(&config, &[a.clone(), b.clone()], now).unwrap();
        assert_eq!(report.ranked[1].reasons, ["SECTOR_CONCENTRATION"]);
        let mut missing_dates = b;
        for (index, bar) in missing_dates.daily_bars.iter_mut().enumerate() {
            // 相同终点、不同起点的两日收益不能冒充一日收益的相关样本。
            bar.ts_event = ((879 + index as u64 * 2) * DAY_NS).into();
        }
        let config = GridSelectionConfig::default();
        let report = select_grid_candidates(&config, &[a, missing_dates], now).unwrap();
        assert!(report.ranked[1].reasons[0].starts_with("CORRELATION_UNAVAILABLE_WITH_"));
    }

    #[rstest]
    #[case("mixed")]
    #[case("duplicate")]
    #[case("future_quote")]
    #[case("future_quote_receipt")]
    #[case("missing_quote")]
    #[case("wide_spread")]
    #[case("zero_quantity")]
    fn unsafe_inputs_are_reported_not_silently_dropped(#[case] failure: &str) {
        let (mut input, now) = input("A");
        let mut config = GridSelectionConfig::default();
        match failure {
            "mixed" => {
                input.signal_bars[0].bar_type =
                    "B.US.LONGBRIDGE-1-MINUTE-LAST-EXTERNAL".parse().unwrap()
            }
            "duplicate" => input.daily_bars[1] = input.daily_bars[0],
            "future_quote" => input.quote.as_mut().unwrap().ts_event = (now + 1).into(),
            "future_quote_receipt" => input.quote.as_mut().unwrap().ts_init = (now + 1).into(),
            "missing_quote" => input.quote = None,
            "wide_spread" => input.quote.as_mut().unwrap().ask_price = Price::from("110.00"),
            "zero_quantity" => config.grid_capital = Decimal::ONE,
            _ => unreachable!(),
        }
        let report = select_grid_candidates(&config, &[input], now).unwrap();
        assert!(report.selected.is_empty());
        assert_eq!(report.ranked.len(), 1);
        assert_eq!(report.ranked[0].reasons.len(), 1);
        assert!(report.ranked[0].score.is_none());
    }

    #[rstest]
    fn extreme_turnover_is_exact_or_explicitly_rejected() {
        let (mut input, now) = input("A");
        let config = GridSelectionConfig {
            lookback_sessions: 252,
            ..Default::default()
        };
        let price = Price::max(0);
        let volume = Quantity::new(nautilus_model::types::quantity::QUANTITY_MAX, 0);
        let kind = input.daily_bars[0].bar_type;
        input.daily_bars = (0..253)
            .map(|index| {
                let ts = ((747 + index) * DAY_NS).into();
                Bar::new(kind, price, price, price, price, volume, ts, ts)
            })
            .collect();
        let per_day = price.as_decimal().checked_mul(volume.as_decimal()).unwrap();
        let report = select_grid_candidates(&config, &[input], now).unwrap();
        // 使用模型实际允许的极值：标准精度下可表示，高精度下窗口累计可能溢出。
        if per_day.checked_mul(Decimal::from(252)).is_none() {
            assert_eq!(report.ranked[0].reasons, ["DOLLAR_VOLUME_OVERFLOW"]);
        } else {
            assert_eq!(
                report.ranked[0]
                    .metrics
                    .as_ref()
                    .unwrap()
                    .average_dollar_volume,
                per_day
            );
        }
    }
}
