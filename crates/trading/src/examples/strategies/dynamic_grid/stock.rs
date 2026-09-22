// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
// -------------------------------------------------------------------------------------------------

//! Causal US-stock session, gap, liquidity and breakout state.

use std::{collections::VecDeque, sync::LazyLock};

use jiff::tz::TimeZone;
use nautilus_core::{UnixNanos, datetime::get_timezone};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::config::GridConfig;

static NEW_YORK: LazyLock<TimeZone> =
    LazyLock::new(|| get_timezone("America/New_York").expect("bundled America/New_York timezone"));

/// Direction of a completed-close boundary break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum BreakoutDirection {
    Up,
    Down,
}

/// Reason stock-adapted entries are temporarily unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum StockGate {
    OffSession,
    GapRecovery,
    LiquidityWarmup,
    LowDollarVolume,
    WideSpread,
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
            Self::BelowMinimumPrice => "BELOW_MINIMUM_PRICE",
        }
    }
}

/// Persisted stock-market context; all values are derived from already completed data.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct StockMarketState {
    session_date: Option<String>,
    previous_close: Option<Decimal>,
    dollar_volumes: VecDeque<Decimal>,
    average_dollar_volume: Decimal,
    spread_bps: Option<Decimal>,
    pub(super) gap_pct: Decimal,
    pub(super) gap_atr: Decimal,
    gap_recovery_bars_remaining: u32,
    breakout_grid_id: u64,
    breakout_direction: Option<BreakoutDirection>,
    breakout_bars: u32,
    breakout_confirmed: bool,
}

impl StockMarketState {
    /// True only during the first US regular session. Exchange holidays are naturally data-free.
    #[must_use]
    pub(super) fn is_regular_session(ts_ns: u64) -> bool {
        let minute = Self::local_minute(ts_ns);
        (9 * 60 + 30..16 * 60).contains(&minute)
    }

    /// Completed one-minute bars commonly carry their close timestamp, including exactly 16:00.
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

    /// Observes one completed regular-session bar and detects the opening gap from prior close.
    pub(super) fn observe_bar(
        &mut self,
        config: &GridConfig,
        ts_ns: u64,
        open: Decimal,
        close: Decimal,
        volume: Decimal,
        prior_atr: Decimal,
    ) -> bool {
        if !Self::is_regular_session_bar(ts_ns) {
            return false;
        }
        let local = UnixNanos::from(ts_ns)
            .to_datetime_utc()
            .to_zoned(NEW_YORK.clone());
        let date = local.date().to_string();
        if self.session_date.as_deref() != Some(&date) {
            self.gap_pct = self
                .previous_close
                .map(|previous| (open / previous - Decimal::ONE).abs())
                .unwrap_or_default();
            self.gap_atr = self
                .previous_close
                .filter(|_| prior_atr > Decimal::ZERO)
                .map(|previous| (open - previous).abs() / prior_atr)
                .unwrap_or_default();
            if self.previous_close.is_some()
                && ((config.max_gap_pct > Decimal::ZERO && self.gap_pct > config.max_gap_pct)
                    || (config.max_gap_atr_multiple > Decimal::ZERO
                        && self.gap_atr > config.max_gap_atr_multiple))
            {
                self.gap_recovery_bars_remaining = config.gap_recovery_bars;
            }
            self.session_date = Some(date);
        }
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
        true
    }

    /// Advances the gap pause only after the current completed bar has been processed.
    pub(super) fn finish_bar(&mut self) {
        self.gap_recovery_bars_remaining = self.gap_recovery_bars_remaining.saturating_sub(1);
    }

    /// Updates an executable quote spread without inventing a spread from OHLC bars.
    pub(super) fn observe_quote(&mut self, bid: Decimal, ask: Decimal) {
        let midpoint = (bid + ask) / Decimal::from(2);
        self.spread_bps = (midpoint > Decimal::ZERO && ask >= bid)
            .then(|| (ask - bid) / midpoint * Decimal::from(10_000));
    }

    /// Returns the first deterministic stock-market entry gate which currently fails.
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

    /// Counts consecutive completed closes beyond the ATR-buffered boundary.
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
    use rust_decimal_macros::dec;

    use super::*;

    const OPEN_2025_01_02: u64 = 1_735_828_200_000_000_000;
    const DAY: u64 = 86_400_000_000_000;

    #[test]
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
        assert!(state.observe_bar(
            &config,
            OPEN_2025_01_02,
            dec!(100),
            dec!(100),
            dec!(1000),
            dec!(2),
        ));
        assert!(state.observe_bar(
            &config,
            OPEN_2025_01_02 + DAY,
            dec!(90),
            dec!(91),
            dec!(1000),
            dec!(2),
        ));
        assert_eq!(state.gap_pct, dec!(0.1));
        assert_eq!(state.gap_atr, dec!(5));
        assert_eq!(
            state.gate(&config, OPEN_2025_01_02 + DAY, dec!(91)),
            Some(StockGate::GapRecovery)
        );
        state.finish_bar();
        assert_eq!(
            state.gate(&config, OPEN_2025_01_02 + DAY, dec!(91)),
            Some(StockGate::GapRecovery)
        );
        state.finish_bar();
        assert_eq!(state.gate(&config, OPEN_2025_01_02 + DAY, dec!(91)), None);
    }

    #[test]
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
