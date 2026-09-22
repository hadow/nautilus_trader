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

//! Completed-bar structure and delivery confirmation, independent of order execution.

use std::{cmp::Ordering, collections::VecDeque};

use nautilus_core::UnixNanos;
use nautilus_indicators::{
    average::ema::ExponentialMovingAverage,
    indicator::{Indicator, MovingAverage},
};
use nautilus_model::{data::Bar, types::Price};

use super::{SlcConfig, Structure, StructureMode};

#[derive(Debug)]
pub(super) struct HTFStructureDetector {
    bars: VecDeque<Bar>,
    highs: VecDeque<Price>,
    lows: VecDeque<Price>,
    fast: ExponentialMovingAverage,
    slow: ExponentialMovingAverage,
    require_ema: bool,
    mode: StructureMode,
    previous: Option<Bar>,
    sweep_direction: Structure,
    sweep_age: usize,
    pub(super) confirmed_at: Option<UnixNanos>,
    pub(super) structure: Structure,
    pub(super) swing_low: Option<Price>,
    pub(super) swing_high: Option<Price>,
}

impl HTFStructureDetector {
    pub(super) fn new(c: &SlcConfig) -> Self {
        Self {
            bars: VecDeque::new(),
            highs: VecDeque::new(),
            lows: VecDeque::new(),
            fast: ExponentialMovingAverage::new(c.ema_fast, None),
            slow: ExponentialMovingAverage::new(c.ema_slow, None),
            require_ema: c.require_htf_ema,
            mode: c.structure_mode,
            previous: None,
            sweep_direction: Structure::Range,
            sweep_age: 0,
            confirmed_at: None,
            structure: Structure::Range,
            swing_low: None,
            swing_high: None,
        }
    }

    pub(super) fn update(&mut self, bar: Bar) {
        if self.previous.is_some_and(|b| bar.ts_event <= b.ts_event) {
            return;
        }
        self.fast.handle_bar(&bar);
        self.slow.handle_bar(&bar);
        if self.mode == StructureMode::SweepReclaim {
            self.update_sweep(bar);
            self.previous = Some(bar);
            return;
        }
        self.previous = Some(bar);
        let previous_structure = self.structure;
        self.bars.push_back(bar);
        if self.bars.len() < 3 {
            return;
        }
        let a = self.bars[0];
        let b = self.bars[1];
        let c = self.bars[2];
        if b.high > a.high && b.high > c.high {
            self.highs.push_back(b.high);
            self.swing_high = Some(b.high);
            if self.highs.len() > 2 {
                self.highs.pop_front();
            }
        }
        if b.low < a.low && b.low < c.low {
            self.lows.push_back(b.low);
            self.swing_low = Some(b.low);
            if self.lows.len() > 2 {
                self.lows.pop_front();
            }
        }
        self.bars.pop_front();
        self.structure = if self.highs.len() == 2 && self.lows.len() == 2 {
            let ema_ready = !self.require_ema || self.slow.initialized();
            if ema_ready
                && self.highs[1] > self.highs[0]
                && self.lows[1] > self.lows[0]
                && bar.close > self.lows[1]
                && (!self.require_ema || self.fast.value() > self.slow.value())
            {
                Structure::Bullish
            } else if ema_ready
                && self.highs[1] < self.highs[0]
                && self.lows[1] < self.lows[0]
                && bar.close < self.highs[1]
                && (!self.require_ema || self.fast.value() < self.slow.value())
            {
                Structure::Bearish
            } else {
                Structure::Range
            }
        } else {
            Structure::Range
        };
        if self.structure != previous_structure {
            self.confirmed_at = Some(bar.ts_event);
        }
    }

    fn update_sweep(&mut self, bar: Bar) {
        let Some(previous) = self.previous else {
            return;
        };
        let bullish = bar.low < previous.low && bar.close >= previous.low;
        let bearish = bar.high > previous.high && bar.close <= previous.high;
        self.sweep_age += 1;
        self.invalidate_sweep(bar);
        match (bullish, bearish) {
            (true, false) | (false, true) => {
                self.sweep_direction = if bullish {
                    Structure::Bullish
                } else {
                    Structure::Bearish
                };
                self.sweep_age = 0;
                self.confirmed_at = Some(bar.ts_event);
                self.swing_low = Some(bar.low);
                self.swing_high = Some(bar.high);
            }
            (true, true) => self.sweep_direction = Structure::Range,
            (false, false) => {}
        }
        // A C2 close permits C3 and C4 only; never erase an earlier emitted decision
        if self.sweep_age >= 2 {
            self.sweep_direction = Structure::Range;
        }
        let ema_aligned = !self.require_ema
            || (self.slow.initialized()
                && match self.sweep_direction {
                    Structure::Bullish => self.fast.value() > self.slow.value(),
                    Structure::Bearish => self.fast.value() < self.slow.value(),
                    Structure::Range => false,
                });
        self.structure = if ema_aligned {
            self.sweep_direction
        } else {
            Structure::Range
        };
    }

    /// Invalidates on an already observed minute extreme, without waiting for HTF close.
    pub(super) fn invalidate_sweep(&mut self, bar: Bar) {
        if self.mode != StructureMode::SweepReclaim {
            return;
        }
        if self
            .previous
            .is_some_and(|previous| match self.sweep_direction {
                Structure::Bullish => bar.low < previous.low,
                Structure::Bearish => bar.high > previous.high,
                Structure::Range => false,
            })
        {
            self.sweep_direction = Structure::Range;
            self.structure = Structure::Range;
        }
    }

    pub(super) fn reset_session(&mut self) {
        if self.mode == StructureMode::SweepReclaim {
            self.previous = None;
            self.sweep_direction = Structure::Range;
            self.structure = Structure::Range;
            self.confirmed_at = None;
            self.swing_high = None;
            self.swing_low = None;
        }
    }
}

/// A causal CISD research definition: reclaim the extreme open of an opposite body run.
/// Dojis preserve the run; a completed crossing is timestamped when it becomes observable.
#[derive(Debug, Default)]
pub(super) struct DeliveryConfirmation {
    previous: Option<Bar>,
    run: Structure,
    run_open: Option<Price>,
    bullish_level: Option<Price>,
    bearish_level: Option<Price>,
    pub(super) event: Option<(UnixNanos, Structure, Price)>,
}

impl DeliveryConfirmation {
    pub(super) fn update(&mut self, bar: Bar) {
        if self.previous.is_some_and(|b| bar.ts_event <= b.ts_event) {
            return;
        }
        let direction = match bar.close.cmp(&bar.open) {
            Ordering::Greater => Structure::Bullish,
            Ordering::Less => Structure::Bearish,
            Ordering::Equal => Structure::Range,
        };
        if let Some(previous) = self.previous {
            if let Some(level) = self.bullish_level
                && previous.close <= level
                && bar.close > level
            {
                self.event = Some((bar.ts_event, Structure::Bullish, level));
                self.bullish_level = None;
            } else if let Some(level) = self.bearish_level
                && previous.close >= level
                && bar.close < level
            {
                self.event = Some((bar.ts_event, Structure::Bearish, level));
                self.bearish_level = None;
            }
        }
        if direction != Structure::Range {
            let opening = if self.run == direction {
                self.run_open.map_or(bar.open, |p| match direction {
                    Structure::Bullish => p.min(bar.open),
                    _ => p.max(bar.open),
                })
            } else {
                bar.open
            };
            self.run = direction;
            self.run_open = Some(opening);
            match direction {
                Structure::Bullish => self.bearish_level = Some(opening),
                Structure::Bearish => self.bullish_level = Some(opening),
                Structure::Range => {}
            }
        }
        self.previous = Some(bar);
    }
}
