// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
// -------------------------------------------------------------------------------------------------

//! 可恢复的慢周期分类与软状态确认；不改变分钟 ATR、价格估值或硬风控的时钟。

use nautilus_data::aggregation::BarBuilder;
use nautilus_model::{
    data::{Bar, BarSpecification, BarType},
    enums::{AggregationSource, BarAggregation, PriceType},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    config::{GridConfig, StrategyMode},
    grid_scale::{GridScaleMode, GridScaleRegime, GridScaleSnapshot},
    regime::{MarketRegime, Observation, RegimeDetector, RegimeSnapshot, is_fresh},
    stock::StockMarketState,
};

const MINUTE: u64 = 60_000_000_000;

/// 共用 15 分钟聚合；旧分类与网格尺度候选独立保存，分钟指标由原检测器维护。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegimeFilter {
    detector: RegimeDetector,
    // 保存最多一个未完成桶。重启后继续喂原生 BarBuilder，不复制其 OHLC 算法。
    pending: Vec<Bar>,
    candidate: MarketRegime,
    confirmed: MarketRegime,
    count: u32,
    observed_ns: u64,
    confirmed_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scale: Option<GridScaleRegime>,
}

pub(super) fn observation(bar: &Bar) -> Observation {
    Observation {
        ts_ns: bar.ts_event.as_u64(),
        high: bar.high.as_f64(),
        low: bar.low.as_f64(),
        close: bar.close.as_f64(),
    }
}

impl RegimeFilter {
    pub(super) fn new(config: &GridConfig) -> Self {
        Self {
            scale: config
                .grid_scale_regime
                .as_ref()
                .map(|_| GridScaleRegime::default()),
            ..Self::default()
        }
    }

    fn scale_active(config: &GridConfig) -> bool {
        config
            .grid_scale_regime
            .as_ref()
            .is_some_and(|c| c.mode != GridScaleMode::Shadow)
    }

    pub(super) fn scale_snapshot(&self) -> Option<&GridScaleSnapshot> {
        self.scale.as_ref().map(|scale| &scale.snapshot)
    }

    pub(super) fn enabled(config: &GridConfig) -> bool {
        config.strategy_mode == StrategyMode::StockAdaptive
    }

    pub(super) fn latest_input_ns(&self) -> u64 {
        self.pending
            .last()
            .map_or(self.observed_ns, |bar| bar.ts_event.as_u64())
    }

    pub(super) fn validate_bar_type(config: &GridConfig, bar_type: BarType) -> anyhow::Result<()> {
        anyhow::ensure!(
            !Self::enabled(config)
                || (bar_type.spec().aggregation == BarAggregation::Minute
                    && bar_type.spec().step.get() == 1
                    && bar_type.spec().price_type == PriceType::Last),
            "Regime filtering requires completed one-minute LAST input bars"
        );
        Ok(())
    }

    fn slow_config(config: &GridConfig) -> GridConfig {
        let mut slow = config.clone();
        // 慢层只判断方向。波动率门槛保留分钟尺度，避免把 15 分钟 ATR 当分钟 ATR 比较。
        slow.enable_volatility_filter = false;
        // 斜率阈值仍是每分钟比例；不是换成 15 分钟后暗中把阈值放宽 15 倍。
        slow.ma_slope_threshold *= config.regime_bar_minutes as f64;
        slow
    }

    pub(super) fn source(&self) -> &RegimeSnapshot {
        &self.detector.snapshot
    }

    pub(super) fn ready(&self, config: &GridConfig, fast: &RegimeSnapshot, now: u64) -> bool {
        let source = self.source();
        fast.initialized
            && if Self::scale_active(config) {
                self.scale_snapshot().is_some_and(|s| s.initialized)
            } else {
                source.initialized
            }
            && is_fresh(fast.ts_ns, now, config.max_signal_age_secs)
            && is_fresh(
                source.ts_ns,
                now,
                config
                    .max_signal_age_secs
                    .saturating_add(config.regime_bar_minutes as u64 * 60),
            )
    }

    pub(super) fn regime(&self, config: &GridConfig, fast: &RegimeSnapshot) -> MarketRegime {
        // 分钟高/低波动门槛不等待慢 K 线或软确认。资金/回撤硬风控在执行入口更早检查。
        if fast.initialized
            && matches!(
                fast.regime,
                MarketRegime::HighVolatility | MarketRegime::LowVolatility
            )
        {
            return fast.regime;
        }
        if !self.ready(config, fast, fast.ts_ns) {
            return MarketRegime::Disabled;
        }
        if Self::scale_active(config) {
            self.scale_snapshot()
                .map_or(MarketRegime::Disabled, |s| s.regime)
        } else {
            self.confirmed
        }
    }

    pub(super) fn entry_confirmed(&self, config: &GridConfig) -> bool {
        if Self::scale_active(config) {
            self.scale_snapshot().is_some_and(|s| s.initialized)
        } else {
            self.confirmed_ns > 0
        }
    }

    /// 只在分类时钟真正收盘时推进确认次数；重复 Tick 不能充当确认 Bar。
    pub(super) fn update(
        &mut self,
        config: &GridConfig,
        bar: &Bar,
        spacing: Option<Decimal>,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            bar.low <= bar.open && bar.open <= bar.high,
            "Invalid regime input open"
        );
        let Some(open) = StockMarketState::session_open_ns(bar.ts_event.as_u64()) else {
            self.pending.clear();
            return Ok(false);
        };
        let interval = config.regime_bar_minutes as u64 * MINUTE;
        let ts = bar.ts_event.as_u64();
        let start = open + (ts - open - 1) / interval * interval;
        if self
            .pending
            .last()
            .is_some_and(|last| last.ts_event.as_u64() + MINUTE != ts)
            || self
                .pending
                .first()
                .is_some_and(|first| first.ts_event.as_u64() != start + MINUTE)
        {
            self.pending.clear();
        }
        // 启动落在桶中间或丢失一分钟时，等待下一个完整桶；不拼接盘外数据或造价。
        if self.pending.is_empty() && ts != start + MINUTE {
            return Ok(false);
        }
        self.pending.push(*bar);
        if ts != start + interval {
            return Ok(false);
        }
        anyhow::ensure!(
            self.pending.len() == config.regime_bar_minutes,
            "Incomplete regime bar"
        );
        let kind = BarType::new(
            bar.bar_type.instrument_id(),
            BarSpecification::new(
                config.regime_bar_minutes,
                BarAggregation::Minute,
                PriceType::Last,
            ),
            AggregationSource::Internal,
        );
        let mut builder = BarBuilder::new(kind, bar.close.precision, bar.volume.precision);
        for input in &self.pending {
            builder.update_bar(*input, input.volume, input.ts_event);
        }
        let completed = builder.build(bar.ts_event, bar.ts_init);
        self.detector
            .update(&Self::slow_config(config), observation(&completed))?;
        if let (Some(scale), Some(scale_config)) = (&mut self.scale, &config.grid_scale_regime) {
            scale.update(
                scale_config,
                ts,
                completed.close.as_decimal(),
                spacing.ok_or_else(|| anyhow::anyhow!("Grid-scale spacing unavailable"))?,
            )?;
        }
        self.pending.clear();
        self.confirm(config)?;
        Ok(true)
    }

    fn confirm(&mut self, config: &GridConfig) -> anyhow::Result<()> {
        let source = self.source();
        let raw = source.regime;
        let ts = source.ts_ns;
        anyhow::ensure!(ts > self.observed_ns, "Regime observations must increase");
        // 隔夜或缺失整个桶后，不把不连续样本当作连续确认，也不带着旧趋势直接开仓。
        if self.observed_ns > 0
            && !is_fresh(
                self.observed_ns,
                ts,
                (config.regime_bar_minutes as u64 * 60).saturating_add(config.max_signal_age_secs),
            )
        {
            self.count = 0;
            self.confirmed = MarketRegime::Disabled;
            self.confirmed_ns = 0;
        }
        self.count = if raw == self.candidate {
            self.count
                .saturating_add(1)
                .min(config.regime_confirmation_bars)
        } else {
            1
        };
        self.candidate = raw;
        self.observed_ns = ts;
        if self.count >= config.regime_confirmation_bars || raw == MarketRegime::HighVolatility {
            self.confirmed = raw;
            self.confirmed_ns = ts;
        }
        Ok(())
    }

    pub(super) fn validate(
        &self,
        config: &GridConfig,
        bar_type: BarType,
        fast: &RegimeSnapshot,
    ) -> anyhow::Result<()> {
        Self::validate_bar_type(config, bar_type)?;
        self.detector.validate(&Self::slow_config(config))?;
        anyhow::ensure!(
            self.scale.is_some() == config.grid_scale_regime.is_some(),
            "Grid-scale recovery/configuration mismatch"
        );
        if let (Some(scale), Some(scale_config)) = (&self.scale, &config.grid_scale_regime) {
            scale.validate(scale_config, self.source().ts_ns)?;
        }
        let source = self.source();
        anyhow::ensure!(
            self.detector.snapshot.ts_ns <= fast.ts_ns
                && self.observed_ns == source.ts_ns
                && self.confirmed_ns <= self.observed_ns
                && self.candidate == source.regime
                && self.count <= config.regime_confirmation_bars
                && (self.count == 0) == (self.observed_ns == 0)
                && (self.confirmed_ns != 0 || self.confirmed == MarketRegime::Disabled)
                && (self.count < config.regime_confirmation_bars
                    || self.confirmed == self.candidate),
            "Invalid recovered regime confirmation"
        );
        anyhow::ensure!(
            self.pending.len() < config.regime_bar_minutes,
            "Oversized regime bucket"
        );
        let mut first = None;
        for (index, bar) in self.pending.iter().enumerate() {
            let ts = bar.ts_event.as_u64();
            let open = StockMarketState::session_open_ns(ts)
                .ok_or_else(|| anyhow::anyhow!("Recovered regime bar outside session"))?;
            let interval = config.regime_bar_minutes as u64 * MINUTE;
            let start = open + (ts - open - 1) / interval * interval;
            let first_ts = *first.get_or_insert(start + MINUTE);
            anyhow::ensure!(
                bar.bar_type == bar_type
                    && ts == first_ts + index as u64 * MINUTE
                    && first_ts == start + MINUTE
                    && ts <= fast.ts_ns
                    && ts > self.detector.snapshot.ts_ns
                    && bar.low.is_positive()
                    && bar.low <= bar.open
                    && bar.open <= bar.high
                    && bar.low <= bar.close
                    && bar.close <= bar.high,
                "Invalid recovered regime bucket"
            );
        }
        anyhow::ensure!(
            self.pending
                .last()
                .is_none_or(|bar| bar.ts_event.as_u64() == fast.ts_ns),
            "Regime bucket differs from latest input"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nautilus_model::{
        data::Bar,
        types::{Price, Quantity},
    };

    use super::*;

    const OPEN: u64 = 1_735_828_200_000_000_000;

    #[rstest::rstest]
    fn grid_scale_filter_uses_completed_buckets_and_keeps_cross_session_memory() {
        use super::super::grid_scale::GridScaleConfig;
        let config = GridConfig {
            strategy_mode: StrategyMode::StockAdaptive,
            grid_scale_regime: Some(GridScaleConfig {
                mode: GridScaleMode::Adaptive,
                lookback_sessions: 2,
                ..Default::default()
            }),
            atr_period: 2,
            adx_period: 2,
            ma_period: 2,
            slope_period: 2,
            volatility_period: 2,
            enable_volatility_filter: false,
            ..Default::default()
        };
        let mut filter = RegimeFilter::new(&config);
        let mut fast = RegimeDetector::default();
        for day in 0..2 {
            for minute in 1..=390 {
                let mut b = bar(minute);
                b.ts_event = (b.ts_event.as_u64() + day * 86_400_000_000_000).into();
                b.ts_init = b.ts_event;
                b.close = if (minute / 15) % 2 == 0 {
                    Price::from("100")
                } else {
                    Price::from("96")
                };
                b.low = Price::from("95");
                fast.update(&config, observation(&b)).unwrap();
                let changed = filter
                    .update(&config, &b, Some(Decimal::new(3, 2)))
                    .unwrap();
                assert_eq!(changed, minute % 15 == 0);
                if minute % 15 != 0 {
                    assert!(filter.scale_snapshot().unwrap().ts_ns < b.ts_event.as_u64());
                }
            }
        }
        assert!(filter.entry_confirmed(&config));
        assert_eq!(filter.regime(&config, &fast.snapshot), MarketRegime::Range);
        let mut restored: RegimeFilter =
            serde_json::from_value(serde_json::to_value(&filter).unwrap()).unwrap();
        restored
            .validate(&config, bar(1).bar_type, &fast.snapshot)
            .unwrap();
        let mut next = bar(15);
        next.ts_event = (OPEN + 4 * 86_400_000_000_000 + 15 * MINUTE).into();
        next.ts_init = next.ts_event;
        fast.snapshot.ts_ns = next.ts_event.as_u64();
        // 周末保留历史不等于允许旧行情交易：新完整桶到来前仍然过期。
        assert!(!restored.ready(&config, &fast.snapshot, fast.snapshot.ts_ns));
        for minute in 1..=15 {
            let mut b = next;
            b.ts_event = (OPEN + 4 * 86_400_000_000_000 + minute * MINUTE).into();
            b.ts_init = b.ts_event;
            restored
                .update(&config, &b, Some(Decimal::new(3, 2)))
                .unwrap();
        }
        assert!(restored.ready(&config, &fast.snapshot, fast.snapshot.ts_ns));
        assert!(restored.entry_confirmed(&config));
        fast.snapshot.regime = MarketRegime::HighVolatility;
        assert_eq!(
            restored.regime(&config, &fast.snapshot),
            MarketRegime::HighVolatility
        );
        let mut bad = serde_json::to_value(&restored).unwrap();
        bad["scale"]["snapshot"]["budget"] = serde_json::json!("1.1");
        let bad: RegimeFilter = serde_json::from_value(bad).unwrap();
        assert!(
            bad.validate(&config, next.bar_type, &fast.snapshot)
                .is_err()
        );
    }

    fn bar(minute: u64) -> Bar {
        Bar::new(
            "AAPL.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap(),
            Price::from("100.00"),
            Price::from("101.00"),
            Price::from("99.00"),
            Price::from("100.00"),
            Quantity::from(100),
            (OPEN + minute * MINUTE).into(),
            (OPEN + minute * MINUTE).into(),
        )
    }

    #[rstest::rstest]
    #[case(0.001, false)]
    #[case(0.001 / 15.0, true)]
    #[case(0.002 / 15.0, true)]
    fn fifteen_minute_slope_calibration_recovers_direction_without_changing_atr(
        #[case] threshold: f64,
        #[case] detects_trend: bool,
        #[values(-1, 1)] direction: i64,
    ) {
        use rust_decimal::Decimal;

        let config = GridConfig {
            strategy_mode: super::super::config::StrategyMode::StockAdaptive,
            regime_bar_minutes: 15,
            regime_confirmation_bars: 8,
            ma_slope_threshold: threshold,
            atr_period: 2,
            adx_period: 2,
            ma_period: 2,
            slope_period: 2,
            volatility_period: 2,
            ..Default::default()
        };
        config.validate().unwrap();
        let baseline = GridConfig {
            ma_slope_threshold: 0.001,
            ..config.clone()
        };
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        let mut original_fast = RegimeDetector::default();
        for minute in 1..=180 {
            let mut b = bar(minute);
            let price = Decimal::from(100) + Decimal::from(minute) * Decimal::new(direction * 4, 2);
            b.open = Price::from_decimal_dp(price, 2).unwrap();
            b.close = b.open;
            b.high = Price::from_decimal_dp(price + Decimal::new(5, 2), 2).unwrap();
            b.low = Price::from_decimal_dp(price - Decimal::new(5, 2), 2).unwrap();
            fast.update(&config, observation(&b)).unwrap();
            original_fast.update(&baseline, observation(&b)).unwrap();
            filter.update(&config, &b, None).unwrap();
            assert_eq!(fast.snapshot.atr, original_fast.snapshot.atr);
            assert_eq!(fast.snapshot.ma_slope, original_fast.snapshot.ma_slope);
            // 在确认尚未结束、桶尚未收盘时恢复；不能靠重启跳过趋势确认。
            if minute == 107 {
                filter = serde_json::from_slice(&serde_json::to_vec(&filter).unwrap()).unwrap();
                filter
                    .validate(&config, b.bar_type, &fast.snapshot)
                    .unwrap();
                assert_eq!(
                    filter.regime(&config, &fast.snapshot),
                    MarketRegime::Disabled
                );
            }
        }
        let expected = match (detects_trend, direction) {
            (true, 1) => MarketRegime::TrendUp,
            (true, _) => MarketRegime::TrendDown,
            (false, _) => MarketRegime::Disabled,
        };
        assert_eq!(filter.source().regime, expected);
        assert_eq!(filter.regime(&config, &fast.snapshot), expected);
        if expected == MarketRegime::TrendDown {
            assert!(!expected.permits_order(&config, true, -1));
            assert!(expected.permits_order(&config, false, -1));
        }
    }

    #[rstest::rstest]
    fn fifteen_minute_confirmation_counts_closes_not_execution_events() {
        let confirmations = 8;
        let config = GridConfig {
            strategy_mode: super::super::config::StrategyMode::StockAdaptive,
            regime_bar_minutes: 15,
            regime_confirmation_bars: confirmations,
            atr_period: 2,
            adx_period: 2,
            ma_period: 2,
            slope_period: 2,
            volatility_period: 2,
            ..Default::default()
        };
        config.validate().unwrap();
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        let mut first_range = None;
        let mut confirmed_range = None;
        for minute in 1..=180 {
            let b = bar(minute);
            fast.update(&config, observation(&b)).unwrap();
            let changed = filter.update(&config, &b, None).unwrap();
            assert_eq!(changed, minute % 15 == 0);
            if changed && filter.source().regime == MarketRegime::Range {
                first_range.get_or_insert(minute);
            }
            if filter.regime(&config, &fast.snapshot) == MarketRegime::Range {
                confirmed_range.get_or_insert(minute);
            }
            // 未收盘桶与确认次数一起恢复，不因重启多算一根，也不偷用正在形成的 Bar。
            filter = serde_json::from_slice(&serde_json::to_vec(&filter).unwrap()).unwrap();
            filter
                .validate(&config, b.bar_type, &fast.snapshot)
                .unwrap();
        }
        // 两根 MA 初始化，再累积两根斜率观测，第四根 15 分钟 Bar 才完成指标预热。
        assert_eq!(first_range, Some(60));
        assert_eq!(
            confirmed_range,
            Some(60 + u64::from(confirmations - 1) * 15)
        );
        assert!(filter.entry_confirmed(&config));

        // 增加软确认不能让分钟级高波动等待 45/75/120 分钟才生效。
        let before = filter.source().ts_ns;
        fast.snapshot.ts_ns += MINUTE;
        fast.snapshot.regime = MarketRegime::HighVolatility;
        assert_eq!(
            filter.regime(&config, &fast.snapshot),
            MarketRegime::HighVolatility
        );
        assert_eq!(filter.source().ts_ns, before);
        assert!(
            !filter
                .regime(&config, &fast.snapshot)
                .permits_order(&config, true, -1)
        );
        assert!(
            filter
                .regime(&config, &fast.snapshot)
                .permits_order(&config, false, -1)
        );
    }

    #[rstest::rstest]
    fn slow_regime_waits_for_close_and_restores_partial_bucket() {
        let config = GridConfig {
            regime_bar_minutes: 15,
            ..Default::default()
        };
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        for minute in 1..15 {
            let b = bar(minute);
            fast.update(&config, observation(&b)).unwrap();
            assert!(!filter.update(&config, &b, None).unwrap());
        }
        assert_eq!(filter.source().ts_ns, 0);
        let mut restored: RegimeFilter =
            serde_json::from_slice(&serde_json::to_vec(&filter).unwrap()).unwrap();
        restored
            .validate(&config, bar(1).bar_type, &fast.snapshot)
            .unwrap();
        let b = bar(15);
        fast.update(&config, observation(&b)).unwrap();
        assert!(filter.update(&config, &b, None).unwrap());
        assert!(restored.update(&config, &b, None).unwrap());
        assert_eq!(
            serde_json::to_value(&filter).unwrap(),
            serde_json::to_value(&restored).unwrap()
        );
        assert_eq!(filter.source().ts_ns, OPEN + 15 * MINUTE);
        assert_eq!(fast.snapshot.ts_ns, OPEN + 15 * MINUTE);
    }

    #[rstest::rstest]
    fn slow_regime_missing_minutes_never_create_synthetic_path() {
        let config = GridConfig {
            regime_bar_minutes: 15,
            ..Default::default()
        };
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        for minute in (1..=30).filter(|m| *m != 8) {
            let b = bar(minute);
            fast.update(&config, observation(&b)).unwrap();
            assert_eq!(filter.update(&config, &b, None).unwrap(), minute == 30);
            filter
                .validate(&config, b.bar_type, &fast.snapshot)
                .unwrap();
        }
        assert_eq!(filter.detector.snapshot.atr, 2.0);
        assert_eq!(filter.observed_ns, OPEN + 30 * MINUTE);
    }

    #[rstest::rstest]
    fn slow_regime_uses_real_high_low_and_close_only_at_bucket_end() {
        let config = GridConfig {
            regime_bar_minutes: 15,
            ..Default::default()
        };
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        for minute in 1..=15 {
            let mut b = bar(minute);
            if minute == 2 {
                b.high = Price::from("110.00");
            }
            if minute == 4 {
                b.low = Price::from("90.00");
            }
            fast.update(&config, observation(&b)).unwrap();
            filter.update(&config, &b, None).unwrap();
            assert_eq!(
                filter.detector.snapshot.atr,
                if minute == 15 { 20.0 } else { 0.0 }
            );
        }
    }

    #[rstest::rstest]
    fn slow_regime_clock_keeps_new_york_anchor_across_dst_and_overnight() {
        let config = GridConfig {
            regime_bar_minutes: 15,
            ..Default::default()
        };
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        for date in ["2025-03-07T14:30:00Z", "2025-03-10T13:30:00Z"] {
            let open = date.parse::<jiff::Timestamp>().unwrap().as_nanosecond() as u64;
            assert_eq!(StockMarketState::session_open_ns(open), None);
            for minute in 1..=20 {
                let mut b = bar(minute);
                b.ts_event = (open + minute * MINUTE).into();
                b.ts_init = b.ts_event;
                fast.update(&config, observation(&b)).unwrap();
                assert_eq!(filter.update(&config, &b, None).unwrap(), minute == 15);
                filter
                    .validate(&config, b.bar_type, &fast.snapshot)
                    .unwrap();
            }
            assert_eq!(filter.observed_ns, open + 15 * MINUTE);
            assert_eq!(filter.pending.len(), 5);
        }
    }

    #[rstest::rstest]
    fn slow_regime_freshness_does_not_extend_minute_quote_lifetime() {
        let config = GridConfig {
            regime_bar_minutes: 15,
            ..Default::default()
        };
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        for minute in 1..=390 {
            let b = bar(minute);
            fast.update(&config, observation(&b)).unwrap();
            filter.update(&config, &b, None).unwrap();
        }
        // 26 根尚不足默认 ADX 暖机，不允许偷用分钟方向状态代替。
        assert!(!filter.ready(&config, &fast.snapshot, fast.snapshot.ts_ns));
        for minute in 1..=59 {
            let mut b = bar(minute);
            b.ts_event = (b.ts_event.as_u64() + 86_400_000_000_000).into();
            b.ts_init = b.ts_event;
            fast.update(&config, observation(&b)).unwrap();
            filter.update(&config, &b, None).unwrap();
        }
        let now = fast.snapshot.ts_ns;
        assert!(filter.ready(&config, &fast.snapshot, now));
        assert!(!filter.ready(&config, &fast.snapshot, now + 181_000_000_000));
        // Tick 还在更新也不能让旧分钟/慢状态永久有效。
        let mut recent_fast = fast.snapshot.clone();
        recent_fast.ts_ns += 5 * MINUTE;
        assert!(!filter.ready(&config, &recent_fast, recent_fast.ts_ns));
        fast.snapshot.regime = MarketRegime::HighVolatility;
        assert_eq!(
            filter.regime(&config, &fast.snapshot),
            MarketRegime::HighVolatility
        );
    }

    #[rstest::rstest]
    fn soft_confirmation_debounces_all_policy_decisions_but_not_high_volatility() {
        let config = GridConfig::default();
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeSnapshot {
            initialized: true,
            ..Default::default()
        };
        let mut step = 0;
        for (raw, count, expected) in [
            (MarketRegime::Range, 8, MarketRegime::Range),
            (MarketRegime::Disabled, 7, MarketRegime::Range),
            (MarketRegime::Range, 1, MarketRegime::Range),
            (MarketRegime::TrendDown, 7, MarketRegime::Range),
            (MarketRegime::TrendDown, 1, MarketRegime::TrendDown),
        ] {
            for _ in 0..count {
                step += 1;
                fast.ts_ns = OPEN + step * 15 * MINUTE;
                filter.detector.snapshot = RegimeSnapshot {
                    ts_ns: fast.ts_ns,
                    initialized: true,
                    regime: raw,
                    ..Default::default()
                };
                filter.confirm(&config).unwrap();
                filter = serde_json::from_slice(&serde_json::to_vec(&filter).unwrap()).unwrap();
            }
            assert_eq!(filter.regime(&config, &fast), expected);
        }
        fast.regime = MarketRegime::HighVolatility;
        assert_eq!(filter.regime(&config, &fast), MarketRegime::HighVolatility);
        assert!(filter.entry_confirmed(&config));
    }

    #[rstest::rstest]
    fn slow_regime_rejects_unsupported_configuration_and_corrupt_buckets() {
        use super::super::config::StrategyMode;

        let mut config = GridConfig {
            regime_bar_minutes: 15,
            strategy_mode: StrategyMode::StockAdaptive,
            ..Default::default()
        };
        config.validate().unwrap();
        assert!(
            RegimeFilter::validate_bar_type(
                &config,
                "AAPL.SIM-5-MINUTE-LAST-EXTERNAL".parse().unwrap()
            )
            .is_err()
        );
        config.regime_bar_minutes = 0;
        assert!(config.validate().is_err());
        config.regime_bar_minutes = 15;
        let mut filter = RegimeFilter::default();
        let mut fast = RegimeDetector::default();
        let b = bar(1);
        fast.update(&config, observation(&b)).unwrap();
        filter.update(&config, &b, None).unwrap();
        filter.count = 9;
        assert!(
            filter
                .validate(&config, b.bar_type, &fast.snapshot)
                .is_err()
        );
        filter.count = 0;
        filter.pending[0].bar_type = "MSFT.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap();
        assert!(
            filter
                .validate(&config, b.bar_type, &fast.snapshot)
                .is_err()
        );
    }
}
