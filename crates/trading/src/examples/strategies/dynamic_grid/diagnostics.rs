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

//! 单标的被动研究观测；绝不参与订单或风险决策。

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    config::{GridConfig, TrendPolicy},
    engine::{GridEngine, spacing_components},
    grid_scale::GridScaleSnapshot,
    regime::{MarketRegime, RegimeDetector, RegimeSnapshot},
    regime_filter::RegimeFilter,
};

/// 随既有报告与检查点保存的单标的诊断数据，不构成第二套交易账本。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GridDiagnostics {
    /// 策略收到的已完成 K 线回调次数。
    pub bar_events: u64,
    /// 策略收到的有效 Quote Tick 回调次数。
    pub quote_events: u64,
    /// 原生订单生命周期回调次数，包含成交与拒单。
    pub order_events: u64,
    /// 组合 watchdog 回调次数；一次回调可能检查多个标的。
    pub timer_events: u64,
    /// 每根指标已预热的完成 K 线记录一个样本，即使当时禁止入场。
    pub spacing: Vec<SpacingObservation>,
    /// 指标已预热但无法计算诊断间距的 K 线数量。
    pub spacing_errors: u64,
    /// 网格创建时的不可变计划，包含价格及按 lot 取整后为零的数量。
    pub grids: Vec<GridEngine>,
    /// 仅记录通过撤单对账屏障后的已完成重置。
    pub resets: Vec<ResetObservation>,
    /// 已发送的撤单请求，不代表撤单确认，也不虚构成交。
    pub cancellations: Vec<CancelObservation>,
    /// 以稳定客户端订单标识为键的真实拒单/否决观测。
    pub rejections: BTreeMap<String, RejectionObservation>,
    /// 券商撤单失败次数，与订单提交拒绝分开统计。
    pub cancel_rejections: BTreeMap<String, RejectionObservation>,
    /// 各单标的硬停原因首次出现的时间戳；不虚构旧版本缺失历史。
    pub risk_stops: BTreeMap<String, u64>,
    /// 被首个适用入场过滤器阻止的已完成 K 线数，而非自然流逝时间。
    pub blocked_bars: BTreeMap<String, ObservationCount>,
    /// 被仓位计算或准入缩减为零的尝试次数，不等同于券商拒单或独立信号数。
    pub zero_admissions: BTreeMap<String, ObservationCount>,
    /// 通过上游门禁后，每根信号 Bar 首次 Sequential 入场决策；Tick 不重复计数。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sequential_entries: BTreeMap<String, ObservationCount>,
    /// 成本感知止盈实际改变的目标；不代表卖单已经成交。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exit_target_changes: Vec<ExitTargetObservation>,
}

/// 全额买入成交后、首次覆盖卖单前发生的目标调整。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExitTargetObservation {
    /// 调整时的本地时间戳。
    pub ts_ns: u64,
    /// 原始入场订单；同时确定标的、网格代次、层级与库存归属。
    pub entry_order_id: String,
    /// 调整前的止盈价。
    pub previous: Decimal,
    /// 调整后的原网格价格。
    pub target: Decimal,
}

/// 统计观测次数，不把连续 K 线错误当作相互独立的交易机会。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationCount {
    /// 观测次数。
    pub count: u64,
    /// 首次观测时间戳。
    pub first_ns: u64,
    /// 最近一次观测时间戳。
    pub last_ns: u64,
}

impl ObservationCount {
    pub(super) fn record(counts: &mut BTreeMap<String, Self>, reason: &str, now: u64) {
        let value = counts.entry(reason.to_string()).or_default();
        if value.count == 0 {
            value.first_ns = now;
        }
        value.count = value.count.saturating_add(1);
        value.last_ns = now;
    }
}

/// 有意将前瞻候选间距与当前网格冻结间距分开记录。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpacingObservation {
    /// 与本根间距观测同时可知的完整分类输入；旧报告缺失时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime: Option<RegimeSnapshot>,
    /// 实验启用后实际用于仓位/订单决策的状态，可能不同于原始分钟分类。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_regime: Option<MarketRegime>,
    /// 分类源刚收盘时才记录其原始指标，未收盘桶不产生观测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_source: Option<RegimeSnapshot>,
    /// 网格尺度候选的收盘观测；Shadow 也记录，但不改变订单决策。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_scale: Option<GridScaleSnapshot>,
    /// 已完成信号 K 线时间戳。
    pub ts_ns: u64,
    /// 该时刻已经可知的 ATR，不包含未来 K 线。
    pub atr: Decimal,
    /// 本次诊断计算使用的已完成 K 线收盘价。
    pub price: Decimal,
    /// 应用最小/最大/成本约束前的间距，已包含趋势倍数。
    pub raw: Decimal,
    /// 应用全部约束后的候选间距；该值不会移动现有网格。
    pub effective: Decimal,
    /// 处理本 K 线后有效网格的冻结间距；没有网格时为 None。
    pub active: Option<Decimal>,
}

/// 一次已对账重置及其原网格代次的真实买入历史。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetObservation {
    /// 重置完成时间戳，可能晚于原始重置信号。
    pub ts_ns: u64,
    /// 已退出的网格代次。
    pub grid_id: u64,
    /// 原始触发原因；优先级依次为向上突破、向下突破、状态变化、波动率变化。
    pub reason: String,
    /// 重置完成时的新鲜估值价格。
    pub price: Decimal,
    /// 至少有一次真实成交的独立买单数，包含部分成交与种子库存。
    pub entry_orders_with_fills: usize,
    /// 其中位于负层级的数量，不包含正层级种子库存买入。
    pub lower_entry_orders_with_fills: usize,
}

/// 一笔已发送撤单及其因果市场上下文；行情可能已经过期。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelObservation {
    /// 撤单发送时间戳。
    pub ts_ns: u64,
    /// 稳定的原生客户端订单标识。
    pub order_id: String,
    /// 订单所属网格代次，不一定是当前网格。
    pub grid_id: u64,
    /// 有符号层级索引。
    pub level: i32,
    /// 该订单是否用于新增库存。
    pub buy: bool,
    /// 调用方当时提供的撤单原因，不从后续状态倒推。
    pub reason: String,
    /// 请求撤单时已经成交的数量。
    pub filled: Decimal,
    /// 当时最近可用的市场估值价格。
    pub price: Option<Decimal>,
    /// 该估值时间戳；不能把隔夜 watchdog 使用的旧价格当作新鲜报价。
    pub mark_ns: u64,
    /// 匹配有效网格代次时，价格最高的负层级入场价。
    pub first_buy_level: Option<Decimal>,
    /// `(市价 - 首个买入层) / 首个买入层`；负值表示价格已经低于该层。
    pub first_buy_distance_pct: Option<Decimal>,
}

/// 每个订单标识观测到的第一条真实拒绝记录。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RejectionObservation {
    /// 本地观测时间戳。
    pub ts_ns: u64,
    /// 原生执行引擎或券商返回的原因，并保留 rejection/denial 类型前缀。
    pub reason: String,
}

impl GridDiagnostics {
    pub(super) fn observe_sequential(&mut self, reason: &str, bar_ns: u64) {
        if self
            .sequential_entries
            .values()
            .any(|v| v.last_ns >= bar_ns)
        {
            return;
        }
        ObservationCount::record(&mut self.sequential_entries, reason, bar_ns);
    }

    pub(super) fn observe_spacing(
        &mut self,
        config: &GridConfig,
        regime: &RegimeDetector,
        filter: Option<&RegimeFilter>,
        price: Decimal,
        grid: Option<&GridEngine>,
    ) {
        let signal = &regime.snapshot;
        if !signal.initialized {
            return;
        }
        let decision = filter.map(|filter| filter.regime(config, signal));
        let multiplier =
            if decision.unwrap_or(signal.regime).policy(config) == TrendPolicy::WiderGrid {
                config.trend_spacing_multiplier
            } else {
                Decimal::ONE
            };
        // 与执行层共用计算函数；这里只观察已完成 K 线，不改变冻结网格或传播诊断错误。
        if let Some(atr) = Decimal::from_f64_retain(signal.atr)
            && let Ok((raw, effective)) = spacing_components(config, atr, price, multiplier)
        {
            self.spacing.push(SpacingObservation {
                grid_scale: filter
                    .and_then(RegimeFilter::scale_snapshot)
                    .filter(|s| s.ts_ns == signal.ts_ns)
                    .cloned(),
                regime: Some(signal.clone()),
                decision_regime: decision,
                regime_source: filter
                    .map(RegimeFilter::source)
                    .filter(|source| source.ts_ns == signal.ts_ns)
                    .cloned(),
                ts_ns: signal.ts_ns,
                atr,
                price,
                raw,
                effective,
                active: grid.map(|g| g.spacing),
            });
        } else {
            self.spacing_errors = self.spacing_errors.saturating_add(1);
        }
    }

    pub(super) fn observe_stop(&mut self, reason: Option<&str>, now: u64) {
        if let Some(reason) = reason
            && !self.risk_stops.contains_key(reason)
        {
            self.risk_stops.insert(reason.to_string(), now);
        }
    }
}
