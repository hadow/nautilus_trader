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

//! 回测、模拟盘和实盘共用的强校验策略配置。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 核心策略与执行链路共用的仓位语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyMode {
    /// 论文式原始 DGT 库存模型，仅保留作研究基准。
    LegacyDgt,
    /// 股票自适应目标仓位模型，将核心仓与网格仓分开管理。
    StockAdaptive,
}

/// 网格间距计算方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpacingMode {
    /// 相邻层级使用固定百分比。
    Percentage,
    /// 使用 ATR/当前价格，并限制在配置的最小与最大间距之间。
    Atr,
}

/// 趋势斜率与价格确认使用的移动平均类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegimeAverage {
    /// 使用原始滚动简单移动平均规则。
    Simple,
    /// 使用已完成收盘价的指数加权平均。
    Exponential,
}

/// 各层级之间的资金权重方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionSizing {
    /// 每层使用相同资金。
    Equal,
    /// 离中心越远，分配越大。
    Progressive,
    /// 越靠近中心，分配越大。
    Inverse,
    /// 层级预算相等，再按目标波动率与实际波动率之比缩减。
    VolatilityAdjusted,
}

/// 检测到方向性趋势时的网格行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendPolicy {
    /// 停止新增库存，但保留已有库存的卖出通道。
    Disable,
    /// 减少趋势方向上的有效买卖层数。
    ReduceGrid,
    /// 在下一次允许重置时扩大网格间距。
    WiderGrid,
    /// 上升趋势中允许积累多头库存，不执行普通网格止盈。
    LongOnly,
    /// 继续执行普通网格。
    Continue,
}

/// 触发硬性风险限制后的处置方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskPolicy {
    /// 撤销入场单，保留库存及其止盈卖单。
    Hold,
    /// 撤销全部订单，等待确认后再平掉已成交库存。
    Flatten,
}

/// 策略参数。所有比例均使用小数表示，例如 `0.01` 表示 1%。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridConfig {
    /// 选择研究基准行为或股票自适应生产行为。
    pub strategy_mode: StrategyMode,
    /// 中心价每一侧的网格层数。
    pub grid_levels: usize,
    /// 使用固定百分比或 ATR 计算间距。
    pub spacing_mode: SpacingMode,
    /// 固定百分比模式下的相邻间距。
    pub spacing_pct: Decimal,
    /// 应用于 ATR/价格的宽度倍数。
    pub atr_multiplier: Decimal,
    /// 相邻层级的最小间距比例。
    pub min_spacing_pct: Decimal,
    /// 相邻层级的最大间距比例。
    pub max_spacing_pct: Decimal,
    /// 策略初始总资金；重置网格不会隐式追加资金。
    pub capital: Decimal,
    /// 账户与暴露约束前，分配给该标的的资金比例。
    pub capital_allocation: Decimal,
    /// 用于预先建立上方可卖库存的网格资金比例。
    pub initial_inventory_fraction: Decimal,
    /// 标的分配资金中长期核心仓的目标比例。
    pub core_target_pct: Decimal,
    /// 标的分配资金中战术网格仓的最大比例。
    pub grid_max_pct: Decimal,
    /// 确认上升趋势后，核心仓目标的乘数。
    pub trend_up_core_multiplier: Decimal,
    /// 确认上升趋势后，网格仓目标的乘数。
    pub trend_up_grid_multiplier: Decimal,
    /// 确认下降趋势后，核心仓目标的乘数。
    pub trend_down_core_multiplier: Decimal,
    /// 确认下降趋势后，网格仓目标的乘数；设为零可禁止逆势加仓。
    pub trend_down_grid_multiplier: Decimal,
    /// 高波动状态下的目标仓位乘数；该状态仍禁止新增网格入场。
    pub high_volatility_position_multiplier: Decimal,
    /// 层级资金权重方式。
    pub position_sizing: PositionSizing,
    /// 波动率调整仓位的目标 ATR/价格；最终放大倍数不会超过 1。
    pub position_volatility_target: Decimal,
    /// 绝对最大数量，包含结果尚未明确的买单。
    pub max_position: Decimal,
    /// 按市值计价库存占策略权益的最大比例。
    pub max_position_pct: Decimal,
    /// 已持仓市值与待成交名义金额之和的上限。
    pub max_notional: Decimal,
    /// 当前网格及遗留库存最多可占用的资金。
    pub max_grid_exposure: Decimal,
    /// 单标的库存市值占券商账户权益的最大比例。
    pub max_asset_ratio: Decimal,
    /// 库存与待买名义金额占策略权益的最大比例。
    pub max_capital_utilization: Decimal,
    /// 相对策略权益历史高水位的最大回撤。
    pub max_drawdown: Decimal,
    /// 相对 UTC 日初权益的最大日内亏损。
    pub max_daily_loss: Decimal,
    /// 未实现亏损占初始资金的最大比例。
    pub max_unrealized_loss: Decimal,
    /// 未完成盈利周期时允许的最大连续重置次数。
    pub max_consecutive_resets: u32,
    /// 每个 UTC 日允许完成的最大重置次数，与盈利周期独立统计。
    pub maximum_resets_per_day: u32,
    /// 最大未终结订单数，包含结果未知的订单。
    pub max_orders: usize,
    /// 每侧允许配置的最大层数。
    pub max_grid_levels: usize,
    /// 买入不可占用的现金保留比例。
    pub reserve_capital: Decimal,
    /// 相对上一中心价的最小重置距离。
    pub minimum_reset_distance: Decimal,
    /// 突破边界后要求的 ATR 超出倍数；区间内重置则按离锚点距离计算。
    pub minimum_reset_atr_multiple: Decimal,
    /// 股票自适应模式重置前，要求连续收在网格外的已完成 K 线数。
    pub breakout_confirmation_bars: u32,
    /// 股票自适应模式恢复入场前，市场状态需连续稳定的已完成 K 线数。
    pub regime_confirmation_bars: u32,
    /// 两次网格重置之间的最短秒数。
    pub minimum_reset_interval_secs: u64,
    /// 触发波动率重置所需的相对间距变化。
    pub volatility_reset_ratio: Decimal,
    /// 突破边界后是否动态重置，而不是终止网格。
    pub enable_dynamic_reset: bool,
    /// 是否启用趋势策略。
    pub enable_trend_filter: bool,
    /// 是否启用波动率阈值。
    pub enable_volatility_filter: bool,
    /// 上升趋势策略。
    pub trend_up_policy: TrendPolicy,
    /// 下降趋势策略。
    pub trend_down_policy: TrendPolicy,
    /// `ReduceGrid` 模式保留的有效层级比例。
    pub trend_level_fraction: Decimal,
    /// `WiderGrid` 模式使用的间距倍数。
    pub trend_spacing_multiplier: Decimal,
    /// ATR 与方向运动指标的计算周期。
    pub atr_period: usize,
    /// 方向运动指标预热完成后的 ADX 平滑周期。
    pub adx_period: usize,
    /// 布林带与移动平均窗口。
    pub ma_period: usize,
    /// 市场方向分类所用的移动平均实现。
    pub regime_average: RegimeAverage,
    /// 是否要求价格位于移动平均的趋势方向一侧。
    pub require_price_ma_confirmation: bool,
    /// 启用价格确认后，价格偏离均线所需的最小比例。
    pub price_ma_confirmation_pct: f64,
    /// 计算均线斜率使用的已完成 K 线数。
    pub slope_period: usize,
    /// 已实现对数收益波动率窗口。
    pub volatility_period: usize,
    /// 布林带标准差倍数。
    pub bollinger_k: f64,
    /// 震荡市场允许的 ADX 上限。
    pub adx_range_max: f64,
    /// 趋势市场要求的 ADX 下限。
    pub adx_trend_min: f64,
    /// 单根 K 线归一化均线斜率的绝对阈值。
    pub ma_slope_threshold: f64,
    /// 允许创建网格所需的最小 ATR/价格。
    pub atr_pct_min: f64,
    /// 允许交易的最大 ATR/价格。
    pub atr_pct_max: f64,
    /// 布林带宽度/中轨的最大值。
    pub bollinger_width_max: f64,
    /// 单根 K 线对数收益波动率上限，不进行年化。
    pub realized_volatility_max: f64,
    /// 将股票自适应模式的入场限制在纽约时间 09:30–16:00 常规交易时段。
    pub regular_session_only: bool,
    /// 触发暂停新增仓位的隔夜跳空绝对比例。
    pub max_gap_pct: Decimal,
    /// 以前一根已完成 K 线 ATR 衡量、触发暂停入场的隔夜跳空倍数。
    pub max_gap_atr_multiple: Decimal,
    /// 大幅跳空后暂停入场的常规时段已完成 K 线数。
    pub gap_recovery_bars: u32,
    /// 成交额流动性门槛使用的已完成 K 线滚动窗口。
    pub liquidity_lookback_bars: usize,
    /// `收盘价 × 成交量` 的最小滚动均值；零表示关闭该门槛。
    pub minimum_average_dollar_volume: Decimal,
    /// 允许的最大买卖价差，单位为基点；零表示关闭该门槛。
    pub maximum_spread_bps: Decimal,
    /// 允许执行的最低股票价格；零表示关闭该门槛。
    pub minimum_price: Decimal,
    /// 计算间距和资金预留时使用的保守 maker 费率。
    pub maker_fee: Decimal,
    /// 计算间距和资金预留时使用的保守 taker 费率。
    pub taker_fee: Decimal,
    /// 在 maker/taker 费率之外额外计入的佣金比例。
    pub commission: Decimal,
    /// 提交订单前估计的单边滑点比例。
    pub slippage: Decimal,
    /// 单个周期预期净利润占入场名义金额的最低比例。
    pub minimum_profit_margin: Decimal,
    /// 触发硬性风险限制后的处置策略。
    pub risk_policy: RiskPolicy,
    /// 订单提交或撤销结果未知时允许等待的最长秒数。
    pub order_timeout_secs: u64,
    /// Tick 驱动交易可使用的已完成信号 K 线最大年龄。
    pub max_signal_age_secs: u64,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            strategy_mode: StrategyMode::LegacyDgt,
            grid_levels: 10,
            spacing_mode: SpacingMode::Atr,
            spacing_pct: Decimal::new(1, 2),
            atr_multiplier: Decimal::new(75, 2),
            min_spacing_pct: Decimal::new(5, 3),
            max_spacing_pct: Decimal::new(3, 2),
            capital: Decimal::from(100_000),
            capital_allocation: Decimal::new(2, 1),
            initial_inventory_fraction: Decimal::ZERO,
            core_target_pct: Decimal::new(4, 1),
            grid_max_pct: Decimal::new(6, 1),
            trend_up_core_multiplier: Decimal::new(125, 2),
            trend_up_grid_multiplier: Decimal::new(5, 1),
            trend_down_core_multiplier: Decimal::new(5, 1),
            trend_down_grid_multiplier: Decimal::ZERO,
            high_volatility_position_multiplier: Decimal::new(25, 2),
            position_sizing: PositionSizing::Equal,
            position_volatility_target: Decimal::new(1, 2),
            max_position: Decimal::from(1000),
            max_position_pct: Decimal::new(2, 1),
            max_notional: Decimal::from(20_000),
            max_grid_exposure: Decimal::from(20_000),
            max_asset_ratio: Decimal::new(2, 1),
            max_capital_utilization: Decimal::new(5, 1),
            max_drawdown: Decimal::new(1, 1),
            max_daily_loss: Decimal::new(3, 2),
            max_unrealized_loss: Decimal::new(8, 2),
            max_consecutive_resets: 5,
            maximum_resets_per_day: 100,
            max_orders: 40,
            max_grid_levels: 30,
            reserve_capital: Decimal::new(5, 1),
            minimum_reset_distance: Decimal::new(1, 2),
            minimum_reset_atr_multiple: Decimal::ZERO,
            breakout_confirmation_bars: 2,
            regime_confirmation_bars: 3,
            minimum_reset_interval_secs: 300,
            volatility_reset_ratio: Decimal::new(5, 1),
            enable_dynamic_reset: true,
            enable_trend_filter: true,
            enable_volatility_filter: true,
            trend_up_policy: TrendPolicy::ReduceGrid,
            trend_down_policy: TrendPolicy::Disable,
            trend_level_fraction: Decimal::new(5, 1),
            trend_spacing_multiplier: Decimal::from(2),
            atr_period: 14,
            adx_period: 14,
            ma_period: 20,
            regime_average: RegimeAverage::Simple,
            require_price_ma_confirmation: false,
            price_ma_confirmation_pct: 0.0,
            slope_period: 5,
            volatility_period: 20,
            bollinger_k: 2.0,
            adx_range_max: 20.0,
            adx_trend_min: 25.0,
            ma_slope_threshold: 0.001,
            atr_pct_min: 0.0001,
            atr_pct_max: 0.05,
            bollinger_width_max: 0.15,
            realized_volatility_max: 0.04,
            regular_session_only: true,
            max_gap_pct: Decimal::new(8, 2),
            max_gap_atr_multiple: Decimal::from(3),
            gap_recovery_bars: 5,
            liquidity_lookback_bars: 20,
            minimum_average_dollar_volume: Decimal::ZERO,
            maximum_spread_bps: Decimal::new(30, 0),
            minimum_price: Decimal::from(5),
            maker_fee: Decimal::new(8, 4),
            taker_fee: Decimal::new(10, 4),
            commission: Decimal::ZERO,
            slippage: Decimal::new(5, 4),
            minimum_profit_margin: Decimal::new(5, 4),
            risk_policy: RiskPolicy::Hold,
            order_timeout_secs: 30,
            max_signal_age_secs: 180,
        }
    }
}

impl GridConfig {
    /// 在创建指标、网格或订单前统一校验配置约束。
    ///
    /// # Errors
    ///
    /// 周期无效、信号参数非有限值，或风险/成本边界相互矛盾时返回错误。
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.grid_levels > 0
                && self.grid_levels <= self.max_grid_levels
                && self.max_grid_levels <= 1000,
            "Invalid grid level limit"
        );
        anyhow::ensure!(
            self.max_orders > 0
                && self.max_consecutive_resets > 0
                && self.maximum_resets_per_day > 0
                && self.breakout_confirmation_bars > 0
                && self.regime_confirmation_bars > 0
                && self.liquidity_lookback_bars > 0,
            "Order/reset limits must be positive"
        );
        for value in [
            self.capital,
            self.max_position,
            self.max_notional,
            self.max_grid_exposure,
            self.atr_multiplier,
        ] {
            anyhow::ensure!(
                value > Decimal::ZERO && value <= Decimal::from(1_000_000_000_000_u64),
                "Invalid capital, quantity or multiplier"
            );
        }
        for ratio in [
            self.capital_allocation,
            self.position_volatility_target,
            self.max_position_pct,
            self.max_asset_ratio,
            self.max_capital_utilization,
            self.max_drawdown,
            self.max_daily_loss,
            self.max_unrealized_loss,
            self.trend_level_fraction,
            self.core_target_pct,
            self.grid_max_pct,
        ] {
            anyhow::ensure!(
                ratio > Decimal::ZERO && ratio <= Decimal::ONE,
                "Risk ratios must be in (0, 1]"
            );
        }
        for ratio in [
            self.initial_inventory_fraction,
            self.reserve_capital,
            self.maker_fee,
            self.taker_fee,
            self.commission,
            self.slippage,
            self.minimum_profit_margin,
            self.minimum_reset_distance,
            self.max_gap_pct,
            self.trend_up_grid_multiplier,
            self.trend_down_core_multiplier,
            self.trend_down_grid_multiplier,
            self.high_volatility_position_multiplier,
        ] {
            anyhow::ensure!(
                ratio >= Decimal::ZERO && ratio < Decimal::ONE,
                "Fractions must be in [0, 1)"
            );
        }
        anyhow::ensure!(
            self.trend_up_core_multiplier > Decimal::ZERO
                && self.trend_up_core_multiplier <= Decimal::from(4)
                && self.core_target_pct + self.grid_max_pct <= Decimal::ONE,
            "Invalid stock target-position allocation"
        );
        anyhow::ensure!(
            self.min_spacing_pct > Decimal::ZERO
                && self.min_spacing_pct <= self.max_spacing_pct
                && self.max_spacing_pct < Decimal::ONE,
            "Invalid spacing bounds"
        );
        anyhow::ensure!(
            self.spacing_pct >= self.min_spacing_pct && self.spacing_pct <= self.max_spacing_pct,
            "Fixed spacing outside bounds"
        );
        anyhow::ensure!(
            self.minimum_reset_atr_multiple >= Decimal::ZERO
                && self.minimum_reset_atr_multiple <= Decimal::from(100)
                && self.max_gap_atr_multiple >= Decimal::ZERO
                && self.max_gap_atr_multiple <= Decimal::from(100)
                && self.minimum_average_dollar_volume >= Decimal::ZERO
                && self.maximum_spread_bps >= Decimal::ZERO
                && self.minimum_price >= Decimal::ZERO,
            "Reset ATR buffer must be in [0, 100]"
        );
        anyhow::ensure!(
            self.volatility_reset_ratio > Decimal::ZERO
                && self.trend_spacing_multiplier >= Decimal::ONE
                && self.trend_spacing_multiplier <= Decimal::from(100),
            "Invalid reset or trend multiplier"
        );
        anyhow::ensure!(
            self.order_timeout_secs > 0
                && self.max_signal_age_secs > 0
                && self.order_timeout_secs <= u64::MAX / 1_000_000_000
                && self.max_signal_age_secs <= u64::MAX / 1_000_000_000,
            "Timeouts must be positive and representable as nanoseconds"
        );
        for period in [
            self.atr_period,
            self.adx_period,
            self.ma_period,
            self.slope_period,
            self.volatility_period,
        ] {
            anyhow::ensure!((2..=1024).contains(&period), "Periods must be in [2, 1024]");
        }
        for value in [
            self.bollinger_k,
            self.adx_range_max,
            self.adx_trend_min,
            self.ma_slope_threshold,
            self.price_ma_confirmation_pct,
            self.atr_pct_min,
            self.atr_pct_max,
            self.bollinger_width_max,
            self.realized_volatility_max,
        ] {
            anyhow::ensure!(
                value.is_finite() && value >= 0.0,
                "Signal thresholds must be finite and nonnegative"
            );
        }
        anyhow::ensure!(
            self.bollinger_k > 0.0
                && self.adx_range_max < self.adx_trend_min
                && self.adx_trend_min <= 100.0
                && self.atr_pct_min < self.atr_pct_max
                && self.realized_volatility_max > 0.0,
            "Inconsistent regime thresholds"
        );
        anyhow::ensure!(
            self.price_ma_confirmation_pct < 1.0,
            "Price/MA confirmation must be below one"
        );
        anyhow::ensure!(
            self.cost_floor() <= self.max_spacing_pct,
            "Maximum grid spacing cannot cover costs"
        );
        Ok(())
    }

    /// 保守的双边交易成本与最低利润安全边际，包含出场名义金额对应的费用。
    #[must_use]
    pub fn cost_floor(&self) -> Decimal {
        let cost = self.maker_fee.max(self.taker_fee) + self.commission + self.slippage;
        if cost >= Decimal::ONE {
            return Decimal::MAX;
        }
        (Decimal::from(2) * cost + self.minimum_profit_margin) / (Decimal::ONE - cost)
    }
}
