// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
// -------------------------------------------------------------------------------------------------

//! 美股常规交易时段、跳空、流动性与突破确认的因果状态。

use std::{collections::VecDeque, sync::LazyLock};

use jiff::tz::TimeZone;
use nautilus_core::{UnixNanos, datetime::get_timezone};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::config::GridConfig;

static NEW_YORK: LazyLock<TimeZone> =
    LazyLock::new(|| get_timezone("America/New_York").expect("bundled America/New_York timezone"));

/// 已完成收盘价突破网格边界的方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum BreakoutDirection {
    /// 向上突破上边界。
    Up,
    /// 向下突破下边界。
    Down,
}

/// 股票自适应模式暂时禁止新增仓位的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum StockGate {
    /// 当前不在美股常规交易时段。
    OffSession,
    /// 严重向下跳空后的观察期尚未结束。
    GapRecovery,
    /// 成交额滚动窗口尚未预热。
    LiquidityWarmup,
    /// 平均成交额低于配置下限。
    LowDollarVolume,
    /// 实时报价买卖价差过宽。
    WideSpread,
    /// Tick 执行模式缺少新鲜的可执行报价；成交打印不能替代盘口。
    StaleQuote,
    /// 当前价格低于允许交易的最低价格。
    BelowMinimumPrice,
}

impl StockGate {
    pub(super) const fn reason(self) -> &'static str {
        match self {
            Self::OffSession => "OUTSIDE_REGULAR_SESSION",
            Self::GapRecovery => "GAP_RECOVERY",
            Self::LiquidityWarmup => "LIQUIDITY_WARMUP",
            Self::LowDollarVolume => "LOW_DOLLAR_VOLUME",
            Self::WideSpread => "WIDE_SPREAD",
            Self::StaleQuote => "STALE_QUOTE",
            Self::BelowMinimumPrice => "BELOW_MINIMUM_PRICE",
        }
    }
}

/// 可持久化的股票市场上下文；所有字段均来自已经可获得的数据。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct StockMarketState {
    session_date: Option<String>,
    previous_close: Option<Decimal>,
    dollar_volumes: VecDeque<Decimal>,
    average_dollar_volume: Decimal,
    spread_bps: Option<Decimal>,
    #[serde(default)]
    quote_ns: Option<u64>,
    pub(super) gap_pct: Decimal,
    pub(super) gap_atr: Decimal,
    gap_recovery_bars_remaining: u32,
    breakout_grid_id: u64,
    breakout_direction: Option<BreakoutDirection>,
    breakout_bars: u32,
    breakout_confirmed: bool,
}

impl StockMarketState {
    /// 仅在美股常规交易时段内返回 true；交易所休市日自然不会产生行情数据。
    #[must_use]
    pub(super) fn is_regular_session(ts_ns: u64) -> bool {
        let minute = Self::local_minute(ts_ns);
        (9 * 60 + 30..16 * 60).contains(&minute)
    }

    /// 已完成的一分钟 K 线通常携带收盘时间，因此恰好 16:00 的 bar 仍属于常规时段。
    #[must_use]
    pub(super) fn is_regular_session_bar(ts_ns: u64) -> bool {
        let minute = Self::local_minute(ts_ns);
        (9 * 60 + 30..=16 * 60).contains(&minute)
    }

    fn local_minute(ts_ns: u64) -> i16 {
        let local = UnixNanos::from(ts_ns)
            .to_datetime_utc()
            .to_zoned(NEW_YORK.clone());
        i16::from(local.hour()) * 60 + i16::from(local.minute())
    }

    /// 对齐已完成整分钟 Bar 的纽约 09:30 起点；拒绝开盘瞬间与盘外数据。
    pub(super) fn session_open_ns(ts_ns: u64) -> Option<u64> {
        let minute = Self::local_minute(ts_ns);
        if !(9 * 60 + 31..=16 * 60).contains(&minute) || !ts_ns.is_multiple_of(60_000_000_000) {
            return None;
        }
        ts_ns.checked_sub((minute - (9 * 60 + 30)) as u64 * 60_000_000_000)
    }

    /// 处理一根已完成的常规时段 K 线，并用真实开盘价检测相对前收盘的跳空。
    pub(super) fn observe_bar(
        &mut self,
        config: &GridConfig,
        ts_ns: u64,
        open: Decimal,
        close: Decimal,
        volume: Decimal,
        prior_atr: Decimal,
    ) -> (bool, Option<Decimal>) {
        if !Self::is_regular_session_bar(ts_ns) {
            return (false, None);
        }
        let local = UnixNanos::from(ts_ns)
            .to_datetime_utc()
            .to_zoned(NEW_YORK.clone());
        let date = local.date().to_string();
        let opening_gap = if self.session_date.as_deref() == Some(&date) {
            None
        } else {
            let opening_gap = self.previous_close.map(|previous| open - previous);
            self.gap_pct = self
                .previous_close
                .map(|previous| (open / previous - Decimal::ONE).abs())
                .unwrap_or_default();
            self.gap_atr = self
                .previous_close
                .filter(|_| prior_atr > Decimal::ZERO)
                .map(|previous| (open - previous).abs() / prior_atr)
                .unwrap_or_default();
            // 分钟 ATR 仅保留诊断，不用日内波动尺度独立否决隔夜跳空后的买入。
            if opening_gap.is_some_and(|change| change < Decimal::ZERO)
                && config.max_gap_pct > Decimal::ZERO
                && self.gap_pct > config.max_gap_pct
            {
                self.gap_recovery_bars_remaining = config.gap_recovery_bars;
            }
            self.session_date = Some(date);
            opening_gap
        };
        self.previous_close = Some(close);
        self.dollar_volumes.push_back(close * volume);
        while self.dollar_volumes.len() > config.liquidity_lookback_bars {
            self.dollar_volumes.pop_front();
        }
        self.average_dollar_volume = if self.dollar_volumes.is_empty() {
            Decimal::ZERO
        } else {
            self.dollar_volumes.iter().sum::<Decimal>() / Decimal::from(self.dollar_volumes.len())
        };
        (true, opening_gap)
    }

    /// 当前已完成 K 线处理完毕后，才递减跳空恢复期，避免少暂停一根 bar。
    pub(super) fn finish_bar(&mut self) {
        self.gap_recovery_bars_remaining = self.gap_recovery_bars_remaining.saturating_sub(1);
    }

    /// 使用可执行买卖报价更新价差，不从 OHLC K 线虚构 spread。
    pub(super) fn observe_quote(&mut self, bid: Decimal, ask: Decimal, ts_ns: u64) {
        let midpoint = (bid + ask) / Decimal::from(2);
        self.spread_bps = (midpoint > Decimal::ZERO && ask >= bid)
            .then(|| (ask - bid) / midpoint * Decimal::from(10_000));
        self.quote_ns = self.spread_bps.map(|_| ts_ns);
    }

    pub(super) fn quote_gate(&self, config: &GridConfig, now: u64) -> Option<StockGate> {
        (config.maximum_spread_bps > Decimal::ZERO
            && self
                .quote_ns
                .is_none_or(|ts| !super::regime::is_fresh(ts, now, config.max_signal_age_secs)))
        .then_some(StockGate::StaleQuote)
    }

    /// 按固定优先级返回当前第一个未通过的股票市场入场门槛。
    #[must_use]
    pub(super) fn gate(
        &self,
        config: &GridConfig,
        ts_ns: u64,
        price: Decimal,
    ) -> Option<StockGate> {
        if config.regular_session_only && !Self::is_regular_session(ts_ns) {
            return Some(StockGate::OffSession);
        }
        if config.minimum_price > Decimal::ZERO && price < config.minimum_price {
            return Some(StockGate::BelowMinimumPrice);
        }
        if self.gap_recovery_bars_remaining > 0 {
            return Some(StockGate::GapRecovery);
        }
        if config.minimum_average_dollar_volume > Decimal::ZERO {
            if self.dollar_volumes.len() < config.liquidity_lookback_bars {
                return Some(StockGate::LiquidityWarmup);
            }
            if self.average_dollar_volume < config.minimum_average_dollar_volume {
                return Some(StockGate::LowDollarVolume);
            }
        }
        if config.maximum_spread_bps > Decimal::ZERO
            && self
                .spread_bps
                .is_some_and(|spread| spread > config.maximum_spread_bps)
        {
            return Some(StockGate::WideSpread);
        }
        None
    }

    /// 统计连续收在含 ATR 缓冲的网格边界之外的已完成 K 线数量。
    pub(super) fn observe_breakout(
        &mut self,
        config: &GridConfig,
        grid_id: u64,
        close: Decimal,
        lower: Decimal,
        upper: Decimal,
        atr: Decimal,
    ) {
        if self.breakout_grid_id != grid_id {
            self.reset_breakout(grid_id);
        }
        let buffer = atr * config.minimum_reset_atr_multiple;
        let direction = if close > upper && close - upper >= buffer {
            Some(BreakoutDirection::Up)
        } else if close < lower && lower - close >= buffer {
            Some(BreakoutDirection::Down)
        } else {
            None
        };
        match direction {
            Some(direction) if self.breakout_direction == Some(direction) => {
                self.breakout_bars = self.breakout_bars.saturating_add(1);
            }
            Some(direction) => {
                self.breakout_direction = Some(direction);
                self.breakout_bars = 1;
            }
            None => {
                self.breakout_direction = None;
                self.breakout_bars = 0;
                self.breakout_confirmed = false;
            }
        }
        self.breakout_confirmed = self.breakout_bars >= config.breakout_confirmation_bars;
    }

    #[must_use]
    pub(super) fn breakout(&self) -> Option<(BreakoutDirection, bool)> {
        self.breakout_direction
            .map(|direction| (direction, self.breakout_confirmed))
    }

    pub(super) fn reset_breakout(&mut self, grid_id: u64) {
        self.breakout_grid_id = grid_id;
        self.breakout_direction = None;
        self.breakout_bars = 0;
        self.breakout_confirmed = false;
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    const OPEN_2025_01_02: u64 = 1_735_828_200_000_000_000;
    const DAY: u64 = 86_400_000_000_000;

    #[rstest]
    fn executable_quote_expires_independently_of_trade_marks() {
        let config = GridConfig::default();
        let mut state = StockMarketState::default();
        let now = OPEN_2025_01_02;
        assert_eq!(state.quote_gate(&config, now), Some(StockGate::StaleQuote));
        state.observe_quote(dec!(100), dec!(100.01), now);
        let deadline = now + config.max_signal_age_secs * 1_000_000_000;
        assert_eq!(state.quote_gate(&config, deadline), None);
        assert_eq!(
            state.quote_gate(&config, deadline + 1),
            Some(StockGate::StaleQuote)
        );
        assert_eq!(
            state.quote_gate(&config, now - 1),
            Some(StockGate::StaleQuote)
        );
        let restored: StockMarketState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            restored.quote_gate(&config, deadline + 1),
            Some(StockGate::StaleQuote)
        );
    }

    #[rstest]
    #[case::minute_atr_spike(dec!(99), dec!(0.2), dec!(0.08), dec!(5), false)]
    #[case::tiny_atr(dec!(99), dec!(0.001), dec!(0.08), dec!(1000), false)]
    #[case::at_threshold(dec!(92), dec!(0.2), dec!(0.08), dec!(40), false)]
    #[case::above_threshold(dec!(91.99), dec!(10), dec!(0.08), dec!(0.801), true)]
    #[case::without_atr(dec!(90), dec!(0), dec!(0.08), dec!(0), true)]
    #[case::upward(dec!(110), dec!(0.2), dec!(0.08), dec!(50), false)]
    #[case::disabled_threshold(dec!(90), dec!(0.2), dec!(0), dec!(50), false)]
    fn gap_simplification_uses_only_downside_percentage(
        #[case] open: Decimal,
        #[case] prior_atr: Decimal,
        #[case] max_gap_pct: Decimal,
        #[case] expected_gap_atr: Decimal,
        #[case] paused: bool,
    ) {
        let config = GridConfig {
            max_gap_pct,
            // 旧配置中的有效值仍能加载，但不再影响入场。
            max_gap_atr_multiple: dec!(3),
            ..Default::default()
        };
        let mut state = StockMarketState::default();
        state.observe_bar(
            &config,
            OPEN_2025_01_02,
            dec!(100),
            dec!(100),
            dec!(1000),
            prior_atr,
        );
        assert_eq!(
            state.observe_bar(
                &config,
                OPEN_2025_01_02 + DAY,
                open,
                open,
                dec!(1000),
                prior_atr,
            ),
            (true, Some(open - dec!(100)))
        );
        assert_eq!(state.gap_atr, expected_gap_atr);
        assert_eq!(
            state.gate(&config, OPEN_2025_01_02 + DAY, open),
            paused.then_some(StockGate::GapRecovery)
        );
    }

    #[rstest]
    fn gap_pause_uses_real_open_and_expires_after_completed_bars() {
        let config = GridConfig {
            max_gap_pct: dec!(0.05),
            max_gap_atr_multiple: dec!(3),
            gap_recovery_bars: 2,
            minimum_price: Decimal::ZERO,
            maximum_spread_bps: Decimal::ZERO,
            ..Default::default()
        };
        let mut state = StockMarketState::default();
        assert_eq!(
            state.observe_bar(
                &config,
                OPEN_2025_01_02,
                dec!(100),
                dec!(100),
                dec!(1000),
                dec!(2),
            ),
            (true, None)
        );
        assert_eq!(
            state.observe_bar(
                &config,
                OPEN_2025_01_02 + DAY,
                dec!(90),
                dec!(91),
                dec!(1000),
                dec!(2),
            ),
            (true, Some(dec!(-10)))
        );
        assert_eq!(state.gap_pct, dec!(0.1));
        assert_eq!(state.gap_atr, dec!(5));
        assert_eq!(
            state.gate(&config, OPEN_2025_01_02 + DAY, dec!(91)),
            Some(StockGate::GapRecovery)
        );
        state.finish_bar();
        // 升级不擅自清除检查点中已启动的观察期；恢复后照常递减。
        let mut state: StockMarketState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            state.gate(&config, OPEN_2025_01_02 + DAY, dec!(91)),
            Some(StockGate::GapRecovery)
        );
        state.finish_bar();
        assert_eq!(state.gate(&config, OPEN_2025_01_02 + DAY, dec!(91)), None);
    }

    #[rstest]
    fn upward_and_ordinary_downward_gaps_do_not_pause_entries() {
        let config = GridConfig {
            max_gap_pct: dec!(0.05),
            max_gap_atr_multiple: dec!(3),
            gap_recovery_bars: 2,
            minimum_price: Decimal::ZERO,
            maximum_spread_bps: Decimal::ZERO,
            ..Default::default()
        };

        for (open, expected_change) in [(dec!(110), dec!(10)), (dec!(97), dec!(-3))] {
            let mut state = StockMarketState::default();
            state.observe_bar(
                &config,
                OPEN_2025_01_02,
                dec!(100),
                dec!(100),
                dec!(1000),
                dec!(2),
            );
            assert_eq!(
                state.observe_bar(
                    &config,
                    OPEN_2025_01_02 + DAY,
                    open,
                    open,
                    dec!(1000),
                    dec!(2),
                ),
                (true, Some(expected_change))
            );
            assert_eq!(state.gate(&config, OPEN_2025_01_02 + DAY, open), None);
        }
    }

    #[rstest]
    fn breakout_requires_persistent_atr_buffered_closes() {
        let config = GridConfig {
            breakout_confirmation_bars: 2,
            minimum_reset_atr_multiple: dec!(0.5),
            ..Default::default()
        };
        let mut state = StockMarketState::default();
        state.observe_breakout(&config, 7, dec!(111), dec!(90), dec!(110), dec!(2));
        assert_eq!(state.breakout(), Some((BreakoutDirection::Up, false)));
        state.observe_breakout(&config, 7, dec!(111.1), dec!(90), dec!(110), dec!(2));
        assert_eq!(state.breakout(), Some((BreakoutDirection::Up, true)));
        state.observe_breakout(&config, 7, dec!(109), dec!(90), dec!(110), dec!(2));
        assert_eq!(state.breakout(), None);
    }
}
