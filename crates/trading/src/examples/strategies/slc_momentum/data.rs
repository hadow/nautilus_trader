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
//  See the License for the specific language governing permissions and limitations under the License.
// -------------------------------------------------------------------------------------------------

//! 分钟数据校验、周期聚合与单标的指标状态。
//!
//! 只接受已经完成且决策时可见的 bar；缺失一分钟会令所在聚合桶失效，跨标的分钟批次
//! 在更晚水位到达后才冻结，防止后来数据改写已经作出的决策。

use std::collections::{BTreeMap, VecDeque};

use nautilus_core::UnixNanos;
use nautilus_data::aggregation::BarBuilder;
use nautilus_indicators::{
    average::{MovingAverageType, ema::ExponentialMovingAverage, vwap::VolumeWeightedAveragePrice},
    indicator::{Indicator, MovingAverage},
    momentum::stochastics::{Stochastics, StochasticsDMethod},
    volatility::atr::AverageTrueRange,
};
use nautilus_model::{
    data::{Bar, BarSpecification, BarType},
    enums::{AggregationSource, BarAggregation, PriceType},
    identifiers::InstrumentId,
    types::Price,
};
use rust_decimal::Decimal;

use super::{
    Session, SlcConfig,
    structure::{DeliveryConfirmation, HTFStructureDetector},
};

pub(super) const MINUTE: u64 = 60_000_000_000;

/// Returns the canonical completed one-minute input type.
#[must_use]
pub fn minute_bar_type(id: InstrumentId) -> BarType {
    bar_type(id, 1, BarAggregation::Minute, AggregationSource::External)
}

pub(super) fn bar_type(
    id: InstrumentId,
    step: u64,
    aggregation: BarAggregation,
    source: AggregationSource,
) -> BarType {
    let (step, aggregation) = if aggregation == BarAggregation::Minute && step.is_multiple_of(60) {
        (step / 60, BarAggregation::Hour)
    } else {
        (step, aggregation)
    };
    BarType::new(
        id,
        BarSpecification::new(step as usize, aggregation, PriceType::Last),
        source,
    )
}

pub(super) fn validate_bar(bar: &Bar, now: UnixNanos) -> anyhow::Result<()> {
    anyhow::ensure!(
        bar.ts_event <= bar.ts_init && bar.ts_init <= now,
        "bar is not available at decision time"
    );
    anyhow::ensure!(
        bar.low > Price::from("0")
            && bar.low <= bar.open
            && bar.low <= bar.close
            && bar.high >= bar.open
            && bar.high >= bar.close,
        "invalid equity OHLC envelope"
    );
    Ok(())
}

/// Adds only calendar-boundary and completeness checks around the native OHLCV builder.
#[derive(Debug, Default)]
pub(super) struct SessionAggregator {
    builder: Option<BarBuilder>,
    bucket_start: u64,
    last: u64,
    count: u64,
}

impl SessionAggregator {
    pub(super) fn update(&mut self, bar: Bar, session: Session, minutes: u64) -> Option<Bar> {
        let ts = bar.ts_event.as_u64();
        let open = session.open.as_u64();
        let interval = minutes * MINUTE;
        let elapsed = ts.checked_sub(open)?;
        if elapsed == 0 || elapsed % MINUTE != 0 || ts > session.close.as_u64() {
            return None;
        }
        let start = open + ((elapsed - 1) / interval) * interval;
        if self.builder.is_none() || start != self.bucket_start {
            self.builder = Some(BarBuilder::new(
                bar_type(
                    bar.bar_type.instrument_id(),
                    minutes,
                    BarAggregation::Minute,
                    AggregationSource::Internal,
                ),
                bar.close.precision,
                bar.volume.precision,
            ));
            self.bucket_start = start;
            self.last = start;
            self.count = 0;
        }
        if ts != self.last + MINUTE {
            // A missing minute invalidates this entire bucket, including its final bar
            self.count = minutes + 1;
        }
        self.last = ts;
        self.count += 1;
        self.builder
            .as_mut()?
            .update_bar(bar, bar.volume, bar.ts_init);
        if ts == start + interval && self.count == minutes {
            return self
                .builder
                .as_mut()
                .map(|b| b.build(bar.ts_event, bar.ts_init));
        }
        None
    }
}

/// A frozen cross-symbol batch is emitted only when a later event watermark arrives.
#[derive(Debug, Default)]
pub(super) struct MinuteBatch {
    pub(super) timestamp: Option<UnixNanos>,
    bars: BTreeMap<InstrumentId, Bar>,
    last_emitted: Option<UnixNanos>,
}

impl MinuteBatch {
    pub(super) fn push(&mut self, bar: Bar) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.last_emitted.is_none_or(|ts| bar.ts_event > ts),
            "late bar would revise a frozen batch"
        );
        anyhow::ensure!(
            self.timestamp.is_none_or(|ts| ts == bar.ts_event),
            "flush the previous minute before inserting another"
        );
        if let Some(old) = self.bars.get(&bar.bar_type.instrument_id()) {
            anyhow::ensure!(*old == bar, "conflicting duplicate completed bar");
            return Ok(());
        }
        self.timestamp = Some(bar.ts_event);
        self.bars.insert(bar.bar_type.instrument_id(), bar);
        Ok(())
    }

    pub(super) fn flush(&mut self, watermark: UnixNanos) -> Vec<Bar> {
        if self.timestamp.is_none_or(|ts| ts >= watermark) {
            return vec![];
        }
        self.last_emitted = self.timestamp.take();
        std::mem::take(&mut self.bars).into_values().collect()
    }
}

#[derive(Debug)]
pub(super) struct SymbolFeatures {
    pub(super) session: Option<Session>,
    pub(super) last: Option<Bar>,
    pub(super) last_ltf: Option<Bar>,
    pub(super) last_htf: Option<UnixNanos>,
    pub(super) vwap: VolumeWeightedAveragePrice,
    pub(super) ema_fast: ExponentialMovingAverage,
    pub(super) ema_slow: ExponentialMovingAverage,
    pub(super) atr: AverageTrueRange,
    pub(super) stochastic: Stochastics,
    pub(super) previous_k: Option<f64>,
    pub(super) structure: HTFStructureDetector,
    pub(super) delivery: DeliveryConfirmation,
    pub(super) volume: Decimal,
    pub(super) observed_minutes: u64,
    pub(super) volume_profiles: VecDeque<Vec<Decimal>>,
    current_volume_profile: Vec<Decimal>,
    last_recorded_ltf: Option<UnixNanos>,
    pub(super) open: Option<Price>,
    pub(super) opening_high: Option<Price>,
    pub(super) opening_low: Option<Price>,
    pub(super) opening_ready: bool,
    pub(super) ltf_volume: VecDeque<f64>,
    pub(super) closes: VecDeque<f64>,
    htf: SessionAggregator,
    ltf: SessionAggregator,
}

impl SymbolFeatures {
    pub(super) fn new(c: &SlcConfig) -> Self {
        Self {
            session: None,
            last: None,
            last_ltf: None,
            last_htf: None,
            vwap: VolumeWeightedAveragePrice::new(),
            ema_fast: ExponentialMovingAverage::new(c.ema_fast, None),
            ema_slow: ExponentialMovingAverage::new(c.ema_slow, None),
            atr: AverageTrueRange::new(
                c.atr_period,
                Some(MovingAverageType::Wilder),
                Some(true),
                None,
            ),
            stochastic: Stochastics::new_with_params(
                c.stochastic_k,
                c.stochastic_d,
                c.stochastic_smoothing,
                MovingAverageType::Simple,
                StochasticsDMethod::MovingAverage,
            ),
            previous_k: None,
            structure: HTFStructureDetector::new(c),
            delivery: DeliveryConfirmation::default(),
            volume: Decimal::ZERO,
            observed_minutes: 0,
            volume_profiles: VecDeque::new(),
            current_volume_profile: Vec::new(),
            last_recorded_ltf: None,
            open: None,
            opening_high: None,
            opening_low: None,
            opening_ready: false,
            ltf_volume: VecDeque::new(),
            closes: VecDeque::new(),
            htf: SessionAggregator::default(),
            ltf: SessionAggregator::default(),
        }
    }

    pub(super) fn update(&mut self, bar: Bar, session: Session, c: &SlcConfig) -> Option<Bar> {
        if self.session.is_none_or(|s| s.open != session.open) {
            if self.session.is_some_and(|s| {
                self.observed_minutes == (s.close.as_u64() - s.open.as_u64()) / MINUTE
            }) {
                self.volume_profiles
                    .push_back(std::mem::take(&mut self.current_volume_profile));
                if self.volume_profiles.len() > 20 {
                    self.volume_profiles.pop_front();
                }
            }
            self.current_volume_profile.clear();
            self.vwap.reset();
            self.structure.reset_session();
            self.delivery = DeliveryConfirmation::default();
            self.stochastic.reset();
            self.previous_k = None;
            self.volume = Decimal::ZERO;
            self.observed_minutes = 0;
            self.open = Some(bar.open);
            self.opening_high = None;
            self.opening_low = None;
            self.opening_ready = false;
            self.closes.clear();
            self.session = Some(session);
        }
        let minute = (bar.ts_event.as_u64() - session.open.as_u64()) / MINUTE;
        self.observed_minutes += 1;
        self.volume += bar.volume.as_decimal();
        self.current_volume_profile.push(self.volume);
        self.vwap.handle_bar(&bar);
        self.ema_fast.handle_bar(&bar);
        self.ema_slow.handle_bar(&bar);
        self.closes.push_back(bar.close.as_f64());
        if self.closes.len() > c.regime_momentum_minutes + 1 {
            self.closes.pop_front();
        }
        if minute <= c.opening_range_minutes {
            self.opening_high = Some(self.opening_high.map_or(bar.high, |p| p.max(bar.high)));
            self.opening_low = Some(self.opening_low.map_or(bar.low, |p| p.min(bar.low)));
            self.opening_ready =
                minute == c.opening_range_minutes && self.observed_minutes == minute;
        }
        self.structure.invalidate_sweep(bar);
        if let Some(htf) = self.htf.update(bar, session, c.htf_minutes) {
            if self
                .last_htf
                .is_some_and(|at| htf.ts_event.as_u64() - at.as_u64() != c.htf_minutes * MINUTE)
            {
                self.structure.reset_session();
            }
            self.structure.update(htf);
            self.last_htf = Some(htf.ts_event);
        }
        self.last = Some(bar);
        let ltf = self.ltf.update(bar, session, c.ltf_minutes)?;
        if self
            .last_ltf
            .is_some_and(|b| ltf.ts_event.as_u64() - b.ts_event.as_u64() != c.ltf_minutes * MINUTE)
        {
            self.delivery = DeliveryConfirmation::default();
        }
        self.delivery.update(ltf);
        self.previous_k = self
            .stochastic
            .initialized
            .then_some(self.stochastic.value_k);
        self.stochastic.handle_bar(&ltf);
        self.atr.handle_bar(&ltf);
        self.last_ltf = Some(ltf);
        Some(ltf)
    }

    pub(super) fn relative_ltf_volume(&self, bar: Bar) -> Option<f64> {
        let count = self
            .ltf_volume
            .len()
            .saturating_sub(usize::from(self.last_recorded_ltf == Some(bar.ts_event)));
        if count < 5 {
            return None;
        }
        let average = self.ltf_volume.iter().take(count).sum::<f64>() / count as f64;
        (average > 0.0).then(|| bar.volume.as_f64() / average)
    }

    pub(super) fn record_ltf_volume(&mut self, bar: Bar) {
        self.last_recorded_ltf = Some(bar.ts_event);
        self.ltf_volume.push_back(bar.volume.as_f64());
        if self.ltf_volume.len() > 20 {
            self.ltf_volume.pop_front();
        }
    }

    pub(super) fn complete_at(&self, ts: UnixNanos) -> bool {
        self.session.is_some_and(|s| {
            s.open < ts
                && ts <= s.close
                && (ts.as_u64() - s.open.as_u64()).is_multiple_of(MINUTE)
                && self.observed_minutes == (ts.as_u64() - s.open.as_u64()) / MINUTE
                && self.last.is_some_and(|b| b.ts_event == ts)
        })
    }

    pub(super) fn regime_vote(&self, ts: UnixNanos, c: &SlcConfig) -> Option<i8> {
        let bar = self.last.filter(|b| b.ts_event == ts)?;
        if !self.opening_ready
            || !self.ema_slow.initialized()
            || self.closes.len() <= c.regime_momentum_minutes
        {
            return None;
        }
        let close = bar.close.as_f64();
        let momentum = close - self.closes.front()?;
        let middle = f64::midpoint(self.opening_high?.as_f64(), self.opening_low?.as_f64());
        let bullish = close > self.vwap.value
            && self.ema_fast.value() > self.ema_slow.value()
            && momentum > 0.0
            && close > middle;
        let bearish = close < self.vwap.value
            && self.ema_fast.value() < self.ema_slow.value()
            && momentum < 0.0
            && close < middle;
        Some(if bullish {
            1
        } else if bearish {
            -1
        } else {
            0
        })
    }
}
