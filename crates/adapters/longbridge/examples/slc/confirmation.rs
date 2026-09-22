// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

use std::{fmt::Display, str::FromStr};

use nautilus_model::{data::Bar, enums::OrderSide, types::Price};

/// 选择用于验证 Level 产生真实价格响应的确认规则
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfirmationModel {
    StochasticReentry,
    LevelResponse,
}

impl Display for ConfirmationModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StochasticReentry => write!(f, "stochastic_reentry"),
            Self::LevelResponse => write!(f, "level_response"),
        }
    }
}

impl FromStr for ConfirmationModel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "stochastic_reentry" => Ok(Self::StochasticReentry),
            "level_response" => Ok(Self::LevelResponse),
            _ => Err("expected one of: stochastic_reentry, level_response".to_string()),
        }
    }
}

/// 当前 Bar 的 Stochastics 极值与阈值回穿事件
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StochasticConfirmation {
    pub extreme: bool,
    pub reentry: bool,
}

/// 确认完成时交给信号排序和诊断的不可变结果
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ConfirmationMatch {
    pub bars: u64,
    pub close_location: f64,
}

/// 从首次触达起保存确认窗口、触达极值和随机指标武装状态
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ConfirmationState {
    model: ConfirmationModel,
    touch_extreme: Price,
    stochastic_armed: bool,
    bars_left: usize,
    window_bars: usize,
    minimum_close_location: f64,
}

impl ConfirmationState {
    /// 以触达 K 线为锚点启动一个包含触达 Bar 和后续 N 根 Bar 的确认窗口
    pub(super) fn start(
        model: ConfirmationModel,
        touch: Bar,
        side: OrderSide,
        stochastic: StochasticConfirmation,
        window_bars: usize,
        minimum_close_location: f64,
    ) -> Self {
        Self {
            model,
            touch_extreme: match side {
                OrderSide::Buy => touch.high,
                OrderSide::Sell => touch.low,
                OrderSide::NoOrderSide => touch.close,
            },
            stochastic_armed: stochastic.extreme,
            bars_left: window_bars.saturating_add(1),
            window_bars,
            minimum_close_location,
        }
    }

    /// 只使用当前及触达时已知数据判断配置的确认模型是否成立
    pub(super) fn matched(
        &mut self,
        bar: Bar,
        side: OrderSide,
        zone_low: Price,
        zone_high: Price,
        stochastic: StochasticConfirmation,
    ) -> Option<ConfirmationMatch> {
        self.stochastic_armed |= stochastic.extreme;
        let close_location = directional_close_location(bar, side);
        let elapsed = self
            .window_bars
            .saturating_add(1)
            .saturating_sub(self.bars_left);
        let confirmed = match self.model {
            ConfirmationModel::StochasticReentry => self.stochastic_armed && stochastic.reentry,
            ConfirmationModel::LevelResponse => {
                let (outside_zone, breaks_touch_extreme) = match side {
                    OrderSide::Buy => (bar.close > zone_high, bar.close > self.touch_extreme),
                    OrderSide::Sell => (bar.close < zone_low, bar.close < self.touch_extreme),
                    OrderSide::NoOrderSide => (false, false),
                };
                outside_zone
                    && close_location >= self.minimum_close_location
                    && (elapsed == 0 || breaks_touch_extreme)
            }
        };
        confirmed.then_some(ConfirmationMatch {
            bars: u64::try_from(elapsed).unwrap_or(u64::MAX),
            close_location,
        })
    }

    /// 消耗当前已完成 Bar，并返回确认窗口是否已经用尽
    pub(super) fn advance(&mut self) -> bool {
        self.bars_left = self.bars_left.saturating_sub(1);
        self.bars_left == 0
    }
}

/// 返回收盘价靠近交易方向有利端的比例，强方向收盘越接近 1
pub(super) fn directional_close_location(bar: Bar, side: OrderSide) -> f64 {
    let range = bar.high.as_f64() - bar.low.as_f64();
    if range <= 0.0 {
        return 0.0;
    }
    match side {
        OrderSide::Buy => (bar.close.as_f64() - bar.low.as_f64()) / range,
        OrderSide::Sell => (bar.high.as_f64() - bar.close.as_f64()) / range,
        OrderSide::NoOrderSide => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use nautilus_core::UnixNanos;
    use nautilus_model::{data::BarType, types::Quantity};

    use super::*;

    fn bar(open: &str, high: &str, low: &str, close: &str, timestamp: u64) -> Bar {
        Bar::new(
            BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            Price::from(open),
            Price::from(high),
            Price::from(low),
            Price::from(close),
            Quantity::from(100),
            UnixNanos::from(timestamp),
            UnixNanos::from(timestamp),
        )
    }

    #[rstest::rstest]
    fn level_response_requires_break_of_the_touch_extreme() {
        let touch = bar("101.50", "102.00", "100.00", "100.50", 1);
        let mut confirmation = ConfirmationState::start(
            ConfirmationModel::LevelResponse,
            touch,
            OrderSide::Buy,
            StochasticConfirmation::default(),
            3,
            0.65,
        );

        assert!(
            confirmation
                .matched(
                    touch,
                    OrderSide::Buy,
                    Price::from("99.00"),
                    Price::from("101.00"),
                    StochasticConfirmation::default(),
                )
                .is_none(),
        );
        assert!(!confirmation.advance());
        assert!(
            confirmation
                .matched(
                    bar("100.50", "101.80", "100.40", "101.50", 2),
                    OrderSide::Buy,
                    Price::from("99.00"),
                    Price::from("101.00"),
                    StochasticConfirmation::default(),
                )
                .is_none(),
        );
        assert!(!confirmation.advance());
        assert_eq!(
            confirmation.matched(
                bar("101.50", "104.00", "101.40", "102.10", 3),
                OrderSide::Buy,
                Price::from("99.00"),
                Price::from("101.00"),
                StochasticConfirmation::default(),
            ),
            None,
        );
        assert!(!confirmation.advance());
        assert_eq!(
            confirmation
                .matched(
                    bar("101.50", "102.50", "101.40", "102.20", 4),
                    OrderSide::Buy,
                    Price::from("99.00"),
                    Price::from("101.00"),
                    StochasticConfirmation::default(),
                )
                .map(|matched| matched.bars),
            Some(3),
        );
    }

    #[rstest::rstest]
    #[case(
        OrderSide::Buy,
        bar("100.00", "102.00", "99.00", "101.50", 1),
        Price::from("99.00"),
        Price::from("101.00")
    )]
    #[case(
        OrderSide::Sell,
        bar("101.00", "102.00", "99.00", "99.50", 1),
        Price::from("100.00"),
        Price::from("102.00")
    )]
    fn level_response_accepts_immediate_strong_rejection(
        #[case] side: OrderSide,
        #[case] touch: Bar,
        #[case] zone_low: Price,
        #[case] zone_high: Price,
    ) {
        let mut confirmation = ConfirmationState::start(
            ConfirmationModel::LevelResponse,
            touch,
            side,
            StochasticConfirmation::default(),
            2,
            0.65,
        );

        assert_eq!(
            confirmation
                .matched(
                    touch,
                    side,
                    zone_low,
                    zone_high,
                    StochasticConfirmation::default(),
                )
                .map(|matched| matched.bars),
            Some(0),
        );
    }
}
