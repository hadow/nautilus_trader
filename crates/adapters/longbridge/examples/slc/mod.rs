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

//! 使用 Longbridge 股票行情和交易接口运行 Structure-Level-Confirmation（SLC）实盘及回测策略。
//!
//! # 风险提示
//!
//! 本示例会真实提交订单。默认配置连接 Longbridge 模拟盘；把配置项 `papertrading` 改为
//! `false` 后会把订单路由到真实保证金账户，同时必须把 `live_order_ack` 设置为
//! `I_UNDERSTAND_LIVE_ORDERS`。该确认只防止误启动，不代表策略已经具备稳定盈利能力。
//! 首次运行必须使用隔离的模拟账户，并在每次关闭节点后人工核对券商侧订单与持仓。
//!
//! # 策略底层逻辑
//!
//! SLC 将一次入场拆成三个必须依次成立的层次：
//!
//! 1. **Structure（结构）**：只使用已经完成并经过右侧 K 线确认的 4 小时 pivot；连续更高的
//!    pivot high 与 pivot low 定义上涨结构，连续更低的二者定义下跌结构，其余状态均为中性。
//! 2. **Level（位置）**：只在当前 4 小时方向同侧寻找 5 分钟强位移。近期底部的 Demand 与
//!    近期顶部的 Supply 保持最小离开时间后可首次回测；过早回访直接失效。其他强位移区域必须
//!    先完成一次破位收复。区域取位移前最后一根反向 K 线完整高低区间，趋势改变时丢弃旧 level。
//! 3. **Confirmation（确认）**：价格进入有效区域后开启有限确认窗口，要求 Stochastics %K
//!    曾进入 20/80 极值区并重新穿越阈值。上涨结构只允许 demand 做多，下跌结构只允许
//!    supply 做空；确认 Bar 收盘后立即产生入场信号。
//!
//! 这种分层设计把“方向、位置、触发”分开，避免仅因指标超买超卖就逆势入场。所有判断只消费
//! 已完成 Bar；实时订阅收到同一时间戳的多次更新时，必须等下一根 Bar 出现才确认上一根，防止
//! 使用尚未收盘的数据。代价是信号必然比视觉回看更晚，尤其对称 4 小时 pivot 需要等待右侧
//! Bar，不能把这种确认延迟误认为数据故障。
//!
//! # 订单与风险模型
//!
//! 每个 symbol 创建独立策略实例和信号状态，多个实例只共享账户风险账本。信号出现后提交仅保留
//! 一根 5 分钟 Bar 的可成交限价单；数量按照“最坏允许入场价到区域止损”的每股风险计算，并受
//! 整手、最大数量、单仓名义金额、账户总名义金额、最大持仓数和开放风险共同约束。
//!
//! 实盘每一次部分成交都会立即创建券商托管的 Longbridge Market-If-Touched 止损。2R 目标按
//! 实际平均成交价重新计算，由可立即成交的一档 bid/ask 触发撤单后市价平仓；已完成 5 分钟 Bar
//! 仅作遗漏报价时的补偿。回测为每次成交提交 OUO 保护组合。
//! 实盘目标依赖本地进程和行情连接，因此当前实现不能等同于券商原子 bracket order。
//!
//! # 回测解释边界
//!
//! 回测和实盘共用信号、仓位和风险代码，但 5 分钟 OHLC 无法重建盘口价差、队列顺序、网络延迟
//! 及真实止损滑点。保守统计额外扣除每股往返成本，假设入场成交在允许的最差限价，并把同一根
//! Bar 同时触及止损与目标的未知路径按亏损处理。Sharpe、年化收益和 Calmar 仍然只是给定样本
//! 的估计值，不能证明未来收益。
//!
//! # 运行方式
//!
//! `cargo run -p nautilus-longbridge --features examples --example longbridge-slc-trader`
//!
//! 策略、风控、回测和 Longbridge OAuth 公共客户端 ID 均在 `examples/slc_symbols.toml`
//! 配置；OAuth token 仍由官方 SDK 的本地安全存储管理。若要使用其他配置文件，在命令后
//! 添加文件路径，例如 `cargo run ... -- /path/to/slc.toml`。

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    env,
    fmt::{Debug, Display},
    fs,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use jiff::{Timestamp, civil::Time as CivilTime, tz::TimeZone};
use longbridge::{
    Market,
    quote::{AdjustType, Candlestick, Period, QuoteContext, TradeSession, TradeSessions},
};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::{actor::DataActor, enums::Environment, live::get_runtime};
use nautilus_core::{
    UUID4, UnixNanos,
    datetime::get_timezone,
    serialization::{deserialize_decimal_from_str, serialize_decimal_as_str},
};
use nautilus_indicators::{
    average::MovingAverageType,
    indicator::Indicator,
    momentum::stochastics::{Stochastics, StochasticsDMethod},
    volatility::atr::AverageTrueRange,
};
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{
        parse::{parse_bar_with_price_precision, parse_instrument},
        rate_limit::{MAX_QUOTE_SUBSCRIPTION_SYMBOLS, quote_api_call_with_retry},
    },
};
use nautilus_model::{
    data::{Bar, BarType, Data, QuoteTick},
    enums::{
        AccountType, AggregationSource, BarAggregation, BookType, ContingencyType, OmsType,
        OrderSide, OrderType, PriceType, TimeInForce, TriggerType,
    },
    events::{
        OrderCancelRejected, OrderCanceled, OrderDenied, OrderExpired, OrderFilled, OrderRejected,
        PositionClosed,
    },
    identifiers::{AccountId, ClientOrderId, InstrumentId, OrderListId, StrategyId, TraderId},
    instruments::{Instrument, InstrumentAny},
    orders::{LimitOrder, Order, OrderAny, StopMarketOrder},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use ustr::Ustr;

const TRADER_ID: &str = "SLC-TRADER-001";
const ACCOUNT_ID: &str = "LONGBRIDGE-001";
const NODE_NAME: &str = "LONGBRIDGE-SLC-001";
const STRATEGY_ID: &str = "SLC-001";
const US_TIMEZONE: &str = "America/New_York";
const RTH_OPEN_MINUTE: u16 = 9 * 60 + 30;
const RTH_CLOSE_MINUTE: u16 = 16 * 60;
const FIVE_MINUTES: u16 = 5;
const FIVE_MINUTE_NANOS: u64 = 5 * 60 * 1_000_000_000;
const FOUR_HOUR_NANOS: u64 = 4 * 60 * 60 * 1_000_000_000;
const HISTORY_CHUNK_DAYS: i64 = 7;
const TRADING_DAYS_CHUNK_DAYS: i64 = 28;
const MAX_WARMUP_AGE_NANOS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;
const MAX_WARMUP_BARS: usize = 1_000;
const LIVE_ACK: &str = "I_UNDERSTAND_LIVE_ORDERS";
const DEFAULT_CONFIG_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/slc_symbols.toml");

/// 美股常规时段内允许入场、禁止新单和强制平仓的时间约束
#[derive(Clone, Copy, Debug)]
struct SessionRules {
    entry_start_minute: u16,
    entry_end_minute: u16,
    flatten_before_close_minutes: u16,
    max_trades_per_day: usize,
}

impl SessionRules {
    /// 校验入场时间窗和收盘前平仓规则均位于美股常规交易时段内
    fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.entry_start_minute >= RTH_OPEN_MINUTE,
            "entry window must start no earlier than 09:30 New York time",
        );
        anyhow::ensure!(
            self.entry_start_minute < self.entry_end_minute
                && self.entry_end_minute <= RTH_CLOSE_MINUTE,
            "entry window must be non-empty and end by 16:00 New York time",
        );
        anyhow::ensure!(
            self.flatten_before_close_minutes > 0,
            "flatten-before-close minutes must be positive",
        );
        anyhow::ensure!(
            self.max_trades_per_day > 0,
            "max trades per day must be positive",
        );
        Ok(())
    }
}

/// 全局方向控制变量，用同一参数组隔离评估多头与空头期望值
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TradeDirection {
    #[default]
    Both,
    Long,
    Short,
}

impl TradeDirection {
    /// 判断当前方向开关是否允许指定买卖方向产生入场信号
    fn allows(self, side: OrderSide) -> bool {
        matches!(
            (self, side),
            (Self::Both | Self::Long, OrderSide::Buy) | (Self::Both | Self::Short, OrderSide::Sell)
        )
    }
}

impl Display for TradeDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Both => write!(f, "both"),
            Self::Long => write!(f, "long"),
            Self::Short => write!(f, "short"),
        }
    }
}

impl FromStr for TradeDirection {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "both" => Ok(Self::Both),
            "long" => Ok(Self::Long),
            "short" => Ok(Self::Short),
            _ => Err("expected one of: both, long, short".to_string()),
        }
    }
}

/// 从统一 TOML 加载并完成交叉校验的应用级策略配置
#[derive(Clone, Debug)]
struct AppConfig {
    instruments: Vec<SlcInstrument>,
    oauth_client_id: String,
    oauth_callback_port: u16,
    papertrading: bool,
    trade_direction: TradeDirection,
    risk_amount: Decimal,
    daily_loss_limit: Decimal,
    max_open_risk: Decimal,
    max_account_notional: Decimal,
    max_open_positions: usize,
    max_order_quantity: Quantity,
    max_order_notional: Decimal,
    minimum_risk_utilization: Decimal,
    max_entry_slippage_ticks: u64,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
    atr_period: usize,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    displacement_max_bars: usize,
    level_extreme_lookback_bars: usize,
    pivot_span: usize,
    zone_ttl_bars: usize,
    minimum_fresh_level_age_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    stochastic_k_period: usize,
    stochastic_k_smoothing: usize,
    stochastic_d_period: usize,
    oversold: f64,
    overbought: f64,
    five_minute_warmup: usize,
    four_hour_warmup: usize,
    minimum_target_time_minutes: u16,
    risk_state_path: PathBuf,
    timezone: TimeZone,
    session: SessionRules,
}

/// 配置文件中的标的身份及交易所规定的精确最小价格变动单位
#[derive(Clone, Copy, Debug)]
struct SlcInstrument {
    instrument_id: InstrumentId,
    price_increment: Price,
}

/// TOML 根结构；未知字段直接报错，避免拼写错误静默使用其他值
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlcFileConfig {
    longbridge: LongbridgeSettings,
    risk: RiskSettings,
    signal: SignalSettings,
    warmup: WarmupSettings,
    session: SessionSettings,
    backtest: BacktestSettings,
    symbols: Vec<SymbolConfigEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongbridgeSettings {
    oauth_client_id: String,
    oauth_callback_port: u16,
    papertrading: bool,
    live_order_ack: String,
    paper_risk_state_path: PathBuf,
    live_risk_state_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskSettings {
    risk_amount: String,
    daily_loss_limit: String,
    max_open_risk: String,
    max_account_notional: String,
    max_open_positions: usize,
    max_order_quantity: String,
    max_order_notional: String,
    minimum_risk_utilization: String,
    max_entry_slippage_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalSettings {
    trade_direction: String,
    risk_reward: String,
    stop_buffer_ticks: u64,
    atr_period: usize,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    displacement_max_bars: usize,
    level_extreme_lookback_bars: usize,
    pivot_span: usize,
    zone_ttl_bars: usize,
    minimum_fresh_level_age_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    stochastic_k_period: usize,
    stochastic_k_smoothing: usize,
    stochastic_d_period: usize,
    oversold: f64,
    overbought: f64,
    minimum_target_time_minutes: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WarmupSettings {
    five_minute_bars: usize,
    four_hour_bars: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionSettings {
    entry_start: String,
    entry_end: String,
    flatten_before_close_minutes: u16,
    max_trades_per_day: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BacktestSettings {
    start: String,
    end: String,
    risk_rewards: Vec<String>,
    starting_balance: String,
    timeout_secs: u64,
    log_bars: bool,
    round_trip_cost_per_share: String,
    walk_forward: Option<WalkForwardSettings>,
}

/// 滚动 walk-forward 的交易日窗口和最低 OOS 验收门槛
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WalkForwardSettings {
    train_days: usize,
    test_days: usize,
    step_days: usize,
    minimum_folds: usize,
    minimum_is_trades: u64,
    minimum_oos_trades: u64,
    minimum_oos_sharpe: f64,
    maximum_oos_drawdown_pct: f64,
    minimum_pass_rate: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolConfigEntry {
    symbol: String,
    price_increment: String,
}

impl AppConfig {
    /// 加载统一 TOML 配置，并拒绝不安全、越界或彼此矛盾的参数组合
    fn load(path: &Path, live: bool) -> anyhow::Result<Self> {
        let config = load_config_file(path)?;
        Self::from_file_config(&config, path, live)
    }

    /// 把已反序列化配置转换成交易热路径使用的精确领域类型
    fn from_file_config(config: &SlcFileConfig, path: &Path, live: bool) -> anyhow::Result<Self> {
        let longbridge = &config.longbridge;
        let oauth_client_id = longbridge.oauth_client_id.trim().to_string();
        anyhow::ensure!(
            !oauth_client_id.is_empty(),
            "longbridge.oauth_client_id must not be empty",
        );
        anyhow::ensure!(
            longbridge.oauth_callback_port > 0,
            "longbridge.oauth_callback_port must be positive",
        );
        let papertrading = longbridge.papertrading;
        if live {
            validate_live_guard(papertrading, Some(longbridge.live_order_ack.trim()))?;
        }

        let instruments = parse_instruments(&config.symbols)?;
        let signal = &config.signal;
        let risk = &config.risk;
        let trade_direction =
            parse_config_value("signal.trade_direction", &signal.trade_direction)?;
        let risk_amount = parse_config_value("risk.risk_amount", &risk.risk_amount)?;
        let daily_loss_limit = parse_config_value("risk.daily_loss_limit", &risk.daily_loss_limit)?;
        let max_open_risk = parse_config_value("risk.max_open_risk", &risk.max_open_risk)?;
        let max_account_notional =
            parse_config_value("risk.max_account_notional", &risk.max_account_notional)?;
        let max_open_positions = risk.max_open_positions;
        let max_order_quantity =
            parse_config_value("risk.max_order_quantity", &risk.max_order_quantity)?;
        let max_order_notional =
            parse_config_value("risk.max_order_notional", &risk.max_order_notional)?;
        let minimum_risk_utilization = parse_config_value(
            "risk.minimum_risk_utilization",
            &risk.minimum_risk_utilization,
        )?;
        let max_entry_slippage_ticks = risk.max_entry_slippage_ticks;
        let risk_reward = parse_config_value("signal.risk_reward", &signal.risk_reward)?;
        anyhow::ensure!(risk_amount > Decimal::ZERO, "risk amount must be positive");
        anyhow::ensure!(
            daily_loss_limit > Decimal::ZERO,
            "daily loss limit must be positive",
        );
        anyhow::ensure!(
            max_open_risk > Decimal::ZERO,
            "maximum open risk must be positive"
        );
        anyhow::ensure!(
            risk_amount <= max_open_risk,
            "risk amount must not exceed maximum open risk",
        );
        anyhow::ensure!(
            max_account_notional > Decimal::ZERO,
            "maximum account notional must be positive",
        );
        anyhow::ensure!(
            max_open_positions > 0,
            "maximum open positions must be positive",
        );
        anyhow::ensure!(
            Quantity::is_positive(&max_order_quantity),
            "maximum order quantity must be positive",
        );
        anyhow::ensure!(
            max_order_notional > Decimal::ZERO,
            "maximum order notional must be positive",
        );
        anyhow::ensure!(
            max_order_notional <= max_account_notional,
            "maximum order notional must not exceed maximum account notional",
        );
        anyhow::ensure!(
            (Decimal::ZERO..=Decimal::ONE).contains(&minimum_risk_utilization),
            "minimum risk utilization must be between 0 and 1",
        );
        anyhow::ensure!(
            risk_reward >= Decimal::from(2),
            "SLC risk reward must be at least 2R",
        );

        let atr_period = signal.atr_period;
        let displacement_atr_multiple = signal.displacement_atr_multiple;
        let displacement_close_fraction = signal.displacement_close_fraction;
        let displacement_max_bars = signal.displacement_max_bars;
        let level_extreme_lookback_bars = signal.level_extreme_lookback_bars;
        let pivot_span = signal.pivot_span;
        let zone_ttl_bars = signal.zone_ttl_bars;
        let minimum_fresh_level_age_bars = signal.minimum_fresh_level_age_bars;
        let max_zones_per_side = signal.max_zones_per_side;
        let confirmation_window_bars = signal.confirmation_window_bars;
        let stochastic_k_period = signal.stochastic_k_period;
        let stochastic_k_smoothing = signal.stochastic_k_smoothing;
        let stochastic_d_period = signal.stochastic_d_period;
        let oversold = signal.oversold;
        let overbought = signal.overbought;
        anyhow::ensure!(atr_period > 0, "ATR period must be positive");
        anyhow::ensure!(
            displacement_atr_multiple > 0.0,
            "displacement ATR multiple must be positive",
        );
        anyhow::ensure!(
            (0.0..=0.5).contains(&displacement_close_fraction),
            "displacement close fraction must be between 0 and 0.5",
        );
        anyhow::ensure!(
            (1..=12).contains(&displacement_max_bars),
            "displacement maximum bars must be between 1 and 12",
        );
        anyhow::ensure!(
            (1..=24).contains(&level_extreme_lookback_bars),
            "level extreme lookback bars must be between 1 and 24",
        );
        anyhow::ensure!(
            pivot_span > 0 && pivot_span <= (MAX_WARMUP_BARS - 1) / 2,
            "pivot span must fit inside the maximum warmup window",
        );
        anyhow::ensure!(zone_ttl_bars > 0, "zone TTL must be positive");
        anyhow::ensure!(
            minimum_fresh_level_age_bars > 0 && minimum_fresh_level_age_bars < zone_ttl_bars,
            "minimum fresh level age must be positive and below zone TTL",
        );
        anyhow::ensure!(
            (1..=32).contains(&max_zones_per_side),
            "maximum zones per side must be between 1 and 32",
        );
        anyhow::ensure!(
            confirmation_window_bars > 0 && confirmation_window_bars <= zone_ttl_bars,
            "confirmation window must be positive and not exceed zone TTL",
        );
        anyhow::ensure!(
            stochastic_k_period > 0 && stochastic_k_smoothing > 0 && stochastic_d_period > 0,
            "stochastic periods must be positive",
        );
        anyhow::ensure!(
            0.0 < oversold && oversold < overbought && overbought < 100.0,
            "stochastic thresholds must satisfy 0 < oversold < overbought < 100",
        );

        let five_minute_warmup = config.warmup.five_minute_bars;
        let four_hour_warmup = config.warmup.four_hour_bars;
        let minimum_five_minute_warmup = atr_period.max(
            stochastic_k_period
                .saturating_add(stochastic_k_smoothing)
                .saturating_add(stochastic_d_period),
        );
        anyhow::ensure!(
            five_minute_warmup > minimum_five_minute_warmup && five_minute_warmup < MAX_WARMUP_BARS,
            "5-minute warmup must initialize ATR and stochastic periods and be less than {MAX_WARMUP_BARS}",
        );
        anyhow::ensure!(
            four_hour_warmup > pivot_span * 2 + 1 && four_hour_warmup < MAX_WARMUP_BARS,
            "4-hour warmup must exceed the pivot window and be less than {MAX_WARMUP_BARS}",
        );

        let session = SessionRules {
            entry_start_minute: parse_clock("session.entry_start", &config.session.entry_start)?,
            entry_end_minute: parse_clock("session.entry_end", &config.session.entry_end)?,
            flatten_before_close_minutes: config.session.flatten_before_close_minutes,
            max_trades_per_day: config.session.max_trades_per_day,
        };
        session.validate()?;
        let minimum_target_time_minutes = signal.minimum_target_time_minutes;
        anyhow::ensure!(
            minimum_target_time_minutes > 0,
            "minimum target time minutes must be positive",
        );
        let configured_risk_state_path = if papertrading {
            &longbridge.paper_risk_state_path
        } else {
            &longbridge.live_risk_state_path
        };
        let risk_state_path = resolve_config_path(path, configured_risk_state_path);

        Ok(Self {
            instruments,
            oauth_client_id,
            oauth_callback_port: longbridge.oauth_callback_port,
            papertrading,
            trade_direction,
            risk_amount,
            daily_loss_limit,
            max_open_risk,
            max_account_notional,
            max_open_positions,
            max_order_quantity,
            max_order_notional,
            minimum_risk_utilization,
            max_entry_slippage_ticks,
            risk_reward,
            stop_buffer_ticks: signal.stop_buffer_ticks,
            atr_period,
            displacement_atr_multiple,
            displacement_close_fraction,
            displacement_max_bars,
            level_extreme_lookback_bars,
            pivot_span,
            zone_ttl_bars,
            minimum_fresh_level_age_bars,
            max_zones_per_side,
            confirmation_window_bars,
            stochastic_k_period,
            stochastic_k_smoothing,
            stochastic_d_period,
            oversold,
            overbought,
            five_minute_warmup,
            four_hour_warmup,
            minimum_target_time_minutes,
            risk_state_path,
            timezone: get_timezone(US_TIMEZONE)?,
            session,
        })
    }

    /// 构造用于信号计算及实盘目标兜底检测的外部 5 分钟 LAST BarType
    fn five_minute_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{instrument_id}-5-MINUTE-LAST-EXTERNAL").as_str())
    }

    /// 构造用于高周期市场结构识别的外部 4 小时 LAST BarType
    fn four_hour_bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::from(format!("{instrument_id}-4-HOUR-LAST-EXTERNAL").as_str())
    }

    /// 为每个可用持仓槽位均分账户名义额度，并返回单次入场可使用的上限
    fn per_position_notional_limit(&self) -> Decimal {
        per_position_notional_limit(
            self.max_account_notional,
            self.max_open_positions.min(self.instruments.len()),
            self.max_order_notional,
        )
    }

    /// 为全部配置标的构造 Longbridge 数据客户端参数，并保留精确最小价格变动单位
    fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            oauth_client_id: Some(self.oauth_client_id.clone()),
            oauth_callback_port: self.oauth_callback_port,
            instrument_price_increments: self
                .instruments
                .iter()
                .map(|instrument| {
                    (
                        instrument.instrument_id.to_string(),
                        instrument.price_increment.to_string(),
                    )
                })
                .collect(),
            ..Default::default()
        }
    }
}

/// 在实盘共享配置之上增加历史区间、资金、成本与样本切分规则
#[derive(Clone, Debug)]
struct SlcBacktestConfig {
    strategy: AppConfig,
    start: Timestamp,
    end: Timestamp,
    risk_rewards: Vec<Decimal>,
    starting_balance: Money,
    timeout_secs: u64,
    log_bars: bool,
    round_trip_cost_per_share: Decimal,
    walk_forward: Option<WalkForwardSettings>,
}

impl SlcBacktestConfig {
    /// 从统一 TOML 加载共享策略和回测区间、资金、成本及 walk-forward 配置
    fn load(path: &Path) -> anyhow::Result<Self> {
        let config = load_config_file(path)?;
        let mut strategy = AppConfig::from_file_config(&config, path, false)?;
        let backtest = &config.backtest;
        let start = parse_config_value("backtest.start", &backtest.start)?;
        let end = parse_config_value("backtest.end", &backtest.end)?;
        anyhow::ensure!(start < end, "backtest.start must be before backtest.end",);
        if let Some(walk_forward) = backtest.walk_forward {
            validate_walk_forward_settings(walk_forward)?;
        }
        let risk_rewards = parse_decimal_grid(&backtest.risk_rewards)?;
        anyhow::ensure!(
            risk_rewards
                .iter()
                .all(|risk_reward| *risk_reward >= Decimal::from(2)),
            "every SLC backtest risk reward must be at least 2R",
        );
        let starting_balance =
            parse_config_value("backtest.starting_balance", &backtest.starting_balance)?;
        anyhow::ensure!(
            Money::is_positive(&starting_balance),
            "backtest.starting_balance must be positive",
        );
        let timeout_secs = backtest.timeout_secs;
        anyhow::ensure!(timeout_secs > 0, "backtest.timeout_secs must be positive");
        let round_trip_cost_per_share = parse_config_value(
            "backtest.round_trip_cost_per_share",
            &backtest.round_trip_cost_per_share,
        )?;
        anyhow::ensure!(
            round_trip_cost_per_share >= Decimal::ZERO,
            "backtest.round_trip_cost_per_share must be non-negative",
        );
        strategy.risk_state_path = env::temp_dir().join(format!(
            "nautilus-slc-backtest-risk-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()),
        ));
        Ok(Self {
            strategy,
            start,
            end,
            risk_rewards,
            starting_balance,
            timeout_secs,
            log_bars: backtest.log_bars,
            round_trip_cost_per_share,
            walk_forward: backtest.walk_forward,
        })
    }
}

/// 拒绝重叠 OOS、样本不足或没有统计意义的滚动窗口设置
fn validate_walk_forward_settings(settings: WalkForwardSettings) -> anyhow::Result<()> {
    anyhow::ensure!(
        settings.train_days > 0,
        "walk_forward.train_days must be positive"
    );
    anyhow::ensure!(
        settings.test_days > 0,
        "walk_forward.test_days must be positive"
    );
    anyhow::ensure!(
        settings.step_days == settings.test_days,
        "walk_forward.step_days must equal test_days so OOS windows do not overlap or leave gaps",
    );
    anyhow::ensure!(
        settings.minimum_folds > 0,
        "walk_forward.minimum_folds must be positive",
    );
    anyhow::ensure!(
        settings.minimum_is_trades > 0 && settings.minimum_oos_trades > 0,
        "walk-forward minimum trade counts must be positive",
    );
    anyhow::ensure!(
        settings.minimum_oos_sharpe.is_finite(),
        "walk_forward.minimum_oos_sharpe must be finite",
    );
    anyhow::ensure!(
        settings.maximum_oos_drawdown_pct.is_finite() && settings.maximum_oos_drawdown_pct > 0.0,
        "walk_forward.maximum_oos_drawdown_pct must be positive",
    );
    anyhow::ensure!(
        settings.minimum_pass_rate.is_finite() && (0.0..=1.0).contains(&settings.minimum_pass_rate),
        "walk_forward.minimum_pass_rate must be finite and between 0 and 1",
    );
    Ok(())
}

/// 读取并反序列化统一 TOML 配置，错误信息始终包含实际文件路径
fn load_config_file(path: &Path) -> anyhow::Result<SlcFileConfig> {
    let value = fs::read_to_string(path)
        .with_context(|| format!("failed to read SLC config {}", path.display()))?;
    toml::from_str(&value).with_context(|| format!("invalid SLC config {}", path.display()))
}

/// 把字符串配置解析成精确领域类型，并在错误中保留 TOML 字段名和原始值
fn parse_config_value<T>(name: &str, value: &str) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    value
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid {name}={value:?}: {e}"))
}

/// 解析用于退出参数比较的正数 Decimal 网格，并排序去重以保证每次搜索顺序一致
fn parse_decimal_grid(configured: &[String]) -> anyhow::Result<Vec<Decimal>> {
    let mut values = configured
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<Decimal>()
                .map_err(|error| anyhow::anyhow!("invalid risk reward {value:?}: {error}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(!values.is_empty(), "risk reward grid must not be empty");
    anyhow::ensure!(
        values.iter().all(|value| *value > Decimal::ZERO),
        "risk reward grid values must be positive",
    );
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

/// 把配置项转换成唯一的 Longbridge 美股 InstrumentId，并校验精确且为正的价格步长
fn parse_instruments(configured: &[SymbolConfigEntry]) -> anyhow::Result<Vec<SlcInstrument>> {
    anyhow::ensure!(
        !configured.is_empty(),
        "symbols must contain at least one instrument",
    );
    anyhow::ensure!(
        configured.len() <= MAX_QUOTE_SUBSCRIPTION_SYMBOLS,
        "symbol config supports at most {MAX_QUOTE_SUBSCRIPTION_SYMBOLS} instruments",
    );

    let mut instruments = Vec::with_capacity(configured.len());
    for entry in configured {
        let symbol = entry.symbol.trim();
        anyhow::ensure!(
            symbol.ends_with(".US"),
            "SLC live example currently requires a Longbridge US symbol such as QQQ.US: {symbol}",
        );
        let instrument_id: InstrumentId = format!("{symbol}.LONGBRIDGE")
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid Longbridge symbol {symbol:?}: {e}"))?;
        anyhow::ensure!(
            instrument_id.venue.as_str() == "LONGBRIDGE"
                && instrument_id.symbol.as_str().ends_with(".US"),
            "SLC live example currently requires US equities on venue LONGBRIDGE: {instrument_id}",
        );
        anyhow::ensure!(
            !instruments
                .iter()
                .any(|instrument: &SlcInstrument| instrument.instrument_id == instrument_id),
            "duplicate Longbridge instrument configured: {instrument_id}",
        );
        let price_increment = entry.price_increment.parse::<Price>().map_err(|e| {
            anyhow::anyhow!(
                "invalid price_increment {:?} for {symbol}: {e}",
                entry.price_increment,
            )
        })?;
        anyhow::ensure!(
            Price::is_positive(&price_increment),
            "price_increment for {symbol} must be positive",
        );
        instruments.push(SlcInstrument {
            instrument_id,
            price_increment,
        });
    }
    Ok(instruments)
}

/// 将 TOML 中的 `HH:MM` 时钟转换为当地午夜后的分钟数，并拒绝无效值
fn parse_clock(name: &str, value: &str) -> anyhow::Result<u16> {
    let (hour, minute) = value
        .split_once(':')
        .with_context(|| format!("invalid {name}={value:?}, expected HH:MM"))?;
    let hour: u16 = hour
        .parse()
        .with_context(|| format!("invalid hour in {name}={value:?}"))?;
    let minute: u16 = minute
        .parse()
        .with_context(|| format!("invalid minute in {name}={value:?}"))?;
    anyhow::ensure!(
        hour < 24 && minute < 60,
        "invalid {name}={value:?}, expected HH:MM",
    );
    Ok(hour * 60 + minute)
}

/// 相对路径以配置文件所在目录为基准，绝对路径保持不变
fn resolve_config_path(config_path: &Path, configured_path: &Path) -> PathBuf {
    if configured_path.is_absolute() {
        configured_path.to_path_buf()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(configured_path)
    }
}

/// 在允许示例路由真实订单前要求第二道完全匹配的人工确认字符串
fn validate_live_guard(papertrading: bool, live_ack: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        papertrading || live_ack == Some(LIVE_ACK),
        concat!(
            "live trading requires longbridge.live_order_ack=",
            "I_UNDERSTAND_LIVE_ORDERS",
        ),
    );
    Ok(())
}

/// 4 小时确认 pivot 推导出的方向许可，不直接代表 5 分钟入场信号
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Trend {
    Up,
    Down,
    #[default]
    Neutral,
}

/// 保存确认 pivot 所需的最小滑动窗口，以及最近两个高点和低点
///
/// pivot 只有在右侧 `span` 根 Bar 全部完成后才被确认，因此该结构天然避免使用未来数据，
/// 代价是趋势切换会比价格拐点晚 `span` 根 4 小时 Bar。
#[derive(Debug)]
struct PivotStructure {
    span: usize,
    window: VecDeque<Bar>,
    highs: VecDeque<Price>,
    lows: VecDeque<Price>,
}

impl PivotStructure {
    /// 创建对称 pivot 检测器，候选点左右两侧都必须拥有指定数量的已完成 Bar
    fn new(span: usize) -> Self {
        Self {
            span,
            window: VecDeque::with_capacity(span * 2 + 1),
            highs: VecDeque::with_capacity(2),
            lows: VecDeque::with_capacity(2),
        }
    }

    /// 加入一根已完成 4 小时 Bar，仅在完整窗口形成后确认正中央的严格 pivot high/low
    fn update(&mut self, bar: Bar) {
        let window_size = self.span * 2 + 1;
        if self.window.len() == window_size {
            self.window.pop_front();
        }
        self.window.push_back(bar);
        if self.window.len() != window_size {
            return;
        }

        let center = self.window[self.span];
        let pivot_high = self
            .window
            .iter()
            .enumerate()
            .all(|(index, candidate)| index == self.span || center.high > candidate.high);
        let pivot_low = self
            .window
            .iter()
            .enumerate()
            .all(|(index, candidate)| index == self.span || center.low < candidate.low);
        if pivot_high {
            push_last_two(&mut self.highs, center.high);
        }
        if pivot_low {
            push_last_two(&mut self.lows, center.low);
        }
    }

    /// 判断是否已经各有两个确认的 pivot high 和 pivot low，可用于结构分类
    fn initialized(&self) -> bool {
        self.highs.len() == 2 && self.lows.len() == 2
    }

    /// 用最近两个确认高点和低点分类结构；二者必须同向变化，否则保持 Neutral
    fn trend(&self) -> Trend {
        if !self.initialized() {
            return Trend::Neutral;
        }
        let high_rising = self.highs[1] > self.highs[0];
        let high_falling = self.highs[1] < self.highs[0];
        let low_rising = self.lows[1] > self.lows[0];
        let low_falling = self.lows[1] < self.lows[0];

        if high_rising && low_rising {
            Trend::Up
        } else if high_falling && low_falling {
            Trend::Down
        } else {
            Trend::Neutral
        }
    }
}

/// 仅保留结构分类所需的最近两个确认 pivot，避免历史状态无界增长
fn push_last_two(values: &mut VecDeque<Price>, value: Price) {
    if values.len() == 2 {
        values.pop_front();
    }
    values.push_back(value);
}

/// 由向上或向下 displacement 生成的需求区、供给区方向
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneKind {
    Demand,
    Supply,
}

/// Supply/Demand level 的生命周期状态
///
/// `Fresh` 首次等待回测，`AwaitingConfirmation` 已触达并处于确认窗口，`BrokenOnce` 表示
/// 首次有效破位，`Reclaimed` 表示从另一侧收复后重新取得交易资格。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneState {
    Fresh,
    AwaitingConfirmation,
    BrokenOnce,
    Reclaimed,
}

/// 一个有界生命周期的 supply/demand 价格区域及其确认状态
///
/// 区域边界取 displacement 源 K 线的完整高低点；只有近期极值允许 fresh 入场，其他强位移
/// 区域必须先完成一次破位收复。ATR 和位移强度固定记录在创建时刻，不被后续波动率重新解释。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Zone {
    kind: ZoneKind,
    low: Price,
    high: Price,
    fresh_entry_eligible: bool,
    age: usize,
    state: ZoneState,
    break_count: u8,
    confirmation_armed: bool,
    confirmation_bars_left: usize,
    atr_at_creation: f64,
    displacement_strength_atr: f64,
}

impl Zone {
    /// 用 displacement 前源 K 线的完整高低区间创建 fresh zone
    fn from_bar(kind: ZoneKind, bar: Bar) -> Self {
        Self {
            kind,
            low: bar.low,
            high: bar.high,
            fresh_entry_eligible: true,
            age: 0,
            state: ZoneState::Fresh,
            break_count: 0,
            confirmation_armed: false,
            confirmation_bars_left: 0,
            atr_at_creation: (bar.high.as_f64() - bar.low.as_f64()).max(f64::EPSILON),
            displacement_strength_atr: 0.0,
        }
    }

    /// 创建 zone，并固定记录是否允许首次回测、生成时 ATR 及位移强度
    fn from_displacement(
        kind: ZoneKind,
        bar: Bar,
        atr: f64,
        strength_atr: f64,
        fresh_entry_eligible: bool,
    ) -> Self {
        Self {
            atr_at_creation: atr,
            displacement_strength_atr: strength_atr,
            fresh_entry_eligible,
            ..Self::from_bar(kind, bar)
        }
    }

    /// 判断已完成 Bar 的高低区间是否与 zone 有任何重叠，即价格是否触达该区域
    fn intersects(self, bar: Bar) -> bool {
        bar.low <= self.high && bar.high >= self.low
    }

    /// 判断收盘价是否穿过 zone 远端；只看收盘可过滤盘中短暂刺穿
    fn broken(self, bar: Bar) -> bool {
        match self.kind {
            ZoneKind::Demand => bar.close < self.low,
            ZoneKind::Supply => bar.close > self.high,
        }
    }

    /// 判断一次破位后的 zone 是否从另一侧被收盘价完整收复
    fn reclaimed(self, bar: Bar) -> bool {
        match self.kind {
            ZoneKind::Demand => bar.close > self.high,
            ZoneKind::Supply => bar.close < self.low,
        }
    }

    /// 在有效回测区域后启动有界随机指标确认窗口；极值必须在触达 Level 时或之后出现
    fn begin_confirmation(&mut self, confirmation: Confirmation, window_bars: usize) {
        self.state = ZoneState::AwaitingConfirmation;
        self.confirmation_armed = confirmation.extreme;
        self.confirmation_bars_left = window_bars + 1;
    }

    /// 用一根已完成 Bar 推进 fresh、待确认、一次破位和收复四态 level 状态机
    ///
    /// 状态转换遵循以下约束：首次有效破位只把区域标记为 `BrokenOnce`，待价格从反方向完整
    /// 收复后才允许作为 `Reclaimed` 再次回测；非近期极值只跳过 Fresh 入场，仍可等待一次破位
    /// 收复。可 Fresh 入场的 Level 若过早回访则直接失效，表示价格没有真正离开。收复后的再次
    /// 破位、确认超时或超过 TTL 都会删除区域。只有趋势、方向和随机指标回穿成立才返回信号。
    fn observe(
        &mut self,
        bar: Bar,
        confirmation: Confirmation,
        allow_entry: bool,
        side: OrderSide,
        rules: SignalRules,
    ) -> ZoneObservation {
        self.age += 1;
        if self.age > rules.zone_ttl_bars {
            return ZoneObservation::Remove;
        }

        if self.broken(bar) {
            match self.state {
                ZoneState::BrokenOnce => return ZoneObservation::Keep,
                ZoneState::Fresh | ZoneState::AwaitingConfirmation if self.break_count == 0 => {
                    self.break_count = 1;
                    self.state = ZoneState::BrokenOnce;
                    self.confirmation_armed = false;
                    self.confirmation_bars_left = 0;
                    return ZoneObservation::Keep;
                }
                ZoneState::Fresh | ZoneState::AwaitingConfirmation | ZoneState::Reclaimed => {
                    return ZoneObservation::Remove;
                }
            }
        }

        if self.state == ZoneState::Fresh
            && self.fresh_entry_eligible
            && self.age < rules.minimum_fresh_level_age_bars
            && self.intersects(bar)
        {
            return ZoneObservation::Remove;
        }

        match self.state {
            ZoneState::Fresh if self.fresh_entry_eligible && self.intersects(bar) => {
                self.begin_confirmation(confirmation, rules.confirmation_window_bars);
            }
            ZoneState::Reclaimed if self.intersects(bar) => {
                self.begin_confirmation(confirmation, rules.confirmation_window_bars);
            }
            ZoneState::BrokenOnce if self.reclaimed(bar) => {
                self.state = ZoneState::Reclaimed;
                return ZoneObservation::Keep;
            }
            ZoneState::BrokenOnce | ZoneState::Fresh | ZoneState::Reclaimed => {
                return ZoneObservation::Keep;
            }
            ZoneState::AwaitingConfirmation => {}
        }

        self.confirmation_armed |= confirmation.extreme;
        let distance_atr = zone_distance_atr(*self, bar.close, self.atr_at_creation);
        let close_location = directional_close_location(bar, side);
        if allow_entry && self.confirmation_armed && confirmation.reentry {
            return ZoneObservation::Signal(Signal {
                side,
                level: if self.break_count == 0 {
                    SignalLevel::Fresh
                } else {
                    SignalLevel::Reclaimed
                },
                entry: bar.close,
                zone_low: self.low,
                zone_high: self.high,
                level_age_bars: u64::try_from(self.age).unwrap_or(u64::MAX),
                confirmation_bars: u64::try_from(
                    rules.confirmation_window_bars + 1 - self.confirmation_bars_left,
                )
                .unwrap_or(u64::MAX),
                confirmation_close_location: close_location,
                distance_atr,
                zone_width_atr: (self.high.as_f64() - self.low.as_f64()) / self.atr_at_creation,
                displacement_strength_atr: self.displacement_strength_atr,
                ts_event: bar.ts_event,
            });
        }

        self.confirmation_bars_left -= 1;
        if self.confirmation_bars_left == 0 {
            ZoneObservation::Remove
        } else {
            ZoneObservation::Keep
        }
    }
}

/// level 完成 SLC 三步检查后交给订单层的不可变信号快照
///
/// 除方向、入场参考价和区域边界外，同时携带只读诊断特征；这些特征不参与准入或排序。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Signal {
    side: OrderSide,
    level: SignalLevel,
    entry: Price,
    zone_low: Price,
    zone_high: Price,
    level_age_bars: u64,
    confirmation_bars: u64,
    confirmation_close_location: f64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    ts_event: UnixNanos,
}

/// 区分首次回测与破位收复后的 setup，便于独立衡量两类 level 的期望值
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SignalLevel {
    Fresh,
    Reclaimed,
}

impl Display for SignalLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh => write!(f, "fresh"),
            Self::Reclaimed => write!(f, "reclaimed"),
        }
    }
}

/// 当前随机指标是否到达过极值，以及本 Bar 是否完成阈值回穿
#[derive(Clone, Copy, Debug)]
struct Confirmation {
    extreme: bool,
    reentry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ZoneObservation {
    Keep,
    Remove,
    Signal(Signal),
}

/// 将 Longbridge 同一时间戳的多次实时更新折叠成一根最终完成 Bar
///
/// 缓冲器不依赖供应商的 confirmed 推送；看到更晚的时间戳时，才把上一时间戳数据交给策略。
#[derive(Debug, Default)]
struct FinalBarBuffer {
    pending: Option<Bar>,
}

impl FinalBarBuffer {
    /// 缓存同一时间戳的最新 Bar 更新，只在下一根 Bar 开始时释放上一根作为最终完成数据
    fn update(&mut self, bar: Bar) -> Option<Bar> {
        let Some(pending) = self.pending else {
            self.pending = Some(bar);
            return None;
        };
        if bar.ts_event < pending.ts_event {
            return None;
        }
        if bar.ts_event == pending.ts_event {
            self.pending = Some(bar);
            return None;
        }
        self.pending = Some(bar);
        Some(pending)
    }

    /// 在有限历史 warmup 结束后取出最后一根已确认 Bar，避免回测边界丢失有效数据
    fn take(&mut self) -> Option<Bar> {
        self.pending.take()
    }
}

/// 从应用配置提取的纯信号规则，便于同一状态机在回测和实盘中复用
#[derive(Clone, Copy, Debug)]
struct SignalRules {
    trade_direction: TradeDirection,
    zone_ttl_bars: usize,
    minimum_fresh_level_age_bars: usize,
    max_zones_per_side: usize,
    confirmation_window_bars: usize,
    displacement_atr_multiple: f64,
    displacement_close_fraction: f64,
    displacement_max_bars: usize,
    level_extreme_lookback_bars: usize,
    oversold: f64,
    overbought: f64,
}

/// 记录信号从方向、level、触达、确认到最终产生的逐层样本数
///
/// 该漏斗是诊断“没有交易”的首要依据，不参与任何交易决策。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SignalFunnel {
    five_minute_bars: u64,
    directional_bars: u64,
    zones_created: u64,
    level_touches: u64,
    stochastic_extremes: u64,
    stochastic_reentries: u64,
    signals: u64,
}

impl SignalFunnel {
    /// 每个方向、每根 Bar 只统计一次触达和确认事件，避免重叠 level 重复放大漏斗计数
    fn record_confirmation(
        &mut self,
        zones: &VecDeque<Zone>,
        bar: Bar,
        confirmation: Confirmation,
    ) {
        let touched = zones.iter().any(|zone| {
            (zone.state == ZoneState::Reclaimed
                || (zone.state == ZoneState::Fresh && zone.fresh_entry_eligible))
                && !zone.broken(bar)
                && zone.intersects(bar)
        });
        let active = touched
            || zones
                .iter()
                .any(|zone| zone.state == ZoneState::AwaitingConfirmation && !zone.broken(bar));
        self.level_touches += u64::from(touched);
        self.stochastic_extremes += u64::from(active && confirmation.extreme);
        self.stochastic_reentries += u64::from(active && confirmation.reentry);
    }
}

/// 单个标的的 SLC 信号引擎，封装 Bar 完成、指标、结构、level 与确认状态
///
/// 它只产生 `Signal`，不读取账户余额、不计算数量也不提交订单，使信号规则能在回测和实盘
/// 共享同一条执行路径，并与跨标的账户风险控制解耦。
struct SlcSignalState {
    five_minute_bars: FinalBarBuffer,
    four_hour_bars: FinalBarBuffer,
    structure: PivotStructure,
    atr: AverageTrueRange,
    stochastics: Stochastics,
    recent_five_minute_bars: VecDeque<Bar>,
    level_trend: Trend,
    last_demand_source: Option<UnixNanos>,
    last_supply_source: Option<UnixNanos>,
    previous_k: Option<f64>,
    demand: VecDeque<Zone>,
    supply: VecDeque<Zone>,
    funnel: SignalFunnel,
    rules: SignalRules,
}

impl SlcSignalState {
    /// 根据已校验配置创建单标的指标、Bar 完成缓冲器及有界 supply/demand zone 集合
    fn new(config: &AppConfig) -> Self {
        Self {
            five_minute_bars: FinalBarBuffer::default(),
            four_hour_bars: FinalBarBuffer::default(),
            structure: PivotStructure::new(config.pivot_span),
            atr: AverageTrueRange::new(
                config.atr_period,
                Some(MovingAverageType::Wilder),
                Some(true),
                None,
            ),
            stochastics: Stochastics::new_with_params(
                config.stochastic_k_period,
                config.stochastic_k_smoothing,
                config.stochastic_d_period,
                MovingAverageType::Simple,
                StochasticsDMethod::MovingAverage,
            ),
            recent_five_minute_bars: VecDeque::with_capacity(
                config.displacement_max_bars + config.level_extreme_lookback_bars + 1,
            ),
            level_trend: Trend::Neutral,
            last_demand_source: None,
            last_supply_source: None,
            previous_k: None,
            demand: VecDeque::with_capacity(config.max_zones_per_side),
            supply: VecDeque::with_capacity(config.max_zones_per_side),
            funnel: SignalFunnel::default(),
            rules: SignalRules {
                trade_direction: config.trade_direction,
                zone_ttl_bars: config.zone_ttl_bars,
                minimum_fresh_level_age_bars: config.minimum_fresh_level_age_bars,
                max_zones_per_side: config.max_zones_per_side,
                confirmation_window_bars: config.confirmation_window_bars,
                displacement_atr_multiple: config.displacement_atr_multiple,
                displacement_close_fraction: config.displacement_close_fraction,
                displacement_max_bars: config.displacement_max_bars,
                level_extreme_lookback_bars: config.level_extreme_lookback_bars,
                oversold: config.oversold,
                overbought: config.overbought,
            },
        }
    }

    /// 回放已完成历史 Bar 初始化指标、结构和 level，但禁止 warmup 期间产生可交易信号
    fn warm_up(
        &mut self,
        five_minute_bars: Vec<Bar>,
        four_hour_bars: Vec<Bar>,
        finalize_last: bool,
    ) {
        for bar in four_hour_bars {
            if let Some(finalized) = self.four_hour_bars.update(bar) {
                self.structure.update(finalized);
            }
        }
        for bar in five_minute_bars {
            if let Some(finalized) = self.five_minute_bars.update(bar) {
                let _ = self.process_five_minute(finalized, false);
            }
        }
        if finalize_last {
            if let Some(finalized) = self.four_hour_bars.take() {
                self.structure.update(finalized);
            }
            if let Some(finalized) = self.five_minute_bars.take() {
                let _ = self.process_five_minute(finalized, false);
            }
        }
    }

    /// 判断 ATR 与 Stochastics 是否都已积累足够样本，可以参与 5 分钟信号判断
    fn indicators_initialized(&self) -> bool {
        self.atr.initialized() && self.stochastics.initialized()
    }

    /// 接收 4 小时 Bar 更新，仅在出现更新的时间戳后返回上一根最终 Bar
    fn finalize_four_hour(&mut self, bar: Bar) -> Option<Bar> {
        self.four_hour_bars.update(bar)
    }

    /// 将一根已完成 4 小时 Bar 写入确认 pivot 结构，不接触任何订单状态
    fn process_four_hour(&mut self, bar: Bar) {
        self.structure.update(bar);
    }

    /// 接收 5 分钟 Bar 更新，仅在出现更新的时间戳后返回上一根最终 Bar
    fn finalize_five_minute(&mut self, bar: Bar) -> Option<Bar> {
        self.five_minute_bars.update(bar)
    }

    /// 推进指标与全部 level，并至多返回一个和 4 小时结构一致的确认信号
    ///
    /// 处理顺序刻意固定：先用当前 Bar 更新随机指标并观察已有区域，再用更新前 ATR 检测当前
    /// displacement 并创建新区域，最后才更新 ATR。这样新区域不能在创建它的同一根 Bar 上被
    /// 当作历史 level 使用，也避免把当前大幅波动提前计入用于判定自身的 ATR 基准。
    fn process_five_minute(&mut self, bar: Bar, allow_signal: bool) -> Option<Signal> {
        self.funnel.five_minute_bars += 1;
        let atr_before = self.atr.value;
        let atr_initialized = self.atr.initialized();
        self.stochastics.handle_bar(&bar);
        let current_k = self.stochastics.value_k;
        let stochastic_initialized = self.stochastics.initialized();
        let long_cross = stochastic_initialized
            && self
                .previous_k
                .is_some_and(|previous| previous <= self.rules.oversold)
            && current_k > self.rules.oversold;
        let short_cross = stochastic_initialized
            && self
                .previous_k
                .is_some_and(|previous| previous >= self.rules.overbought)
            && current_k < self.rules.overbought;

        let trend = self.structure.trend();
        self.align_levels_with_trend(trend);
        self.funnel.directional_bars += u64::from(trend != Trend::Neutral);
        let long_confirmation = Confirmation {
            extreme: stochastic_initialized && current_k <= self.rules.oversold,
            reentry: long_cross,
        };
        let short_confirmation = Confirmation {
            extreme: stochastic_initialized && current_k >= self.rules.overbought,
            reentry: short_cross,
        };
        self.funnel
            .record_confirmation(&self.demand, bar, long_confirmation);
        self.funnel
            .record_confirmation(&self.supply, bar, short_confirmation);
        let long_signal = observe_zones(
            &mut self.demand,
            bar,
            long_confirmation,
            trend == Trend::Up && allow_signal && self.rules.trade_direction.allows(OrderSide::Buy),
            OrderSide::Buy,
            self.rules,
        );
        let short_signal = observe_zones(
            &mut self.supply,
            bar,
            short_confirmation,
            trend == Trend::Down
                && allow_signal
                && self.rules.trade_direction.allows(OrderSide::Sell),
            OrderSide::Sell,
            self.rules,
        );

        if self
            .recent_five_minute_bars
            .back()
            .is_some_and(|previous| has_five_minute_gap(previous.ts_event, bar.ts_event))
        {
            self.recent_five_minute_bars.clear();
        }
        self.recent_five_minute_bars.push_back(bar);
        while self.recent_five_minute_bars.len()
            > self.rules.displacement_max_bars + self.rules.level_extreme_lookback_bars + 1
        {
            self.recent_five_minute_bars.pop_front();
        }
        if atr_initialized
            && let Some((kind, source, displacement_strength_atr, fresh_entry_eligible)) =
                displacement_zone(&self.recent_five_minute_bars, atr_before, self.rules)
            && matches!(
                (trend, kind),
                (Trend::Up, ZoneKind::Demand) | (Trend::Down, ZoneKind::Supply)
            )
        {
            let last_source = match kind {
                ZoneKind::Demand => &mut self.last_demand_source,
                ZoneKind::Supply => &mut self.last_supply_source,
            };
            if *last_source != Some(source.ts_event) {
                let zones = match kind {
                    ZoneKind::Demand => &mut self.demand,
                    ZoneKind::Supply => &mut self.supply,
                };
                push_zone(
                    zones,
                    Zone::from_displacement(
                        kind,
                        source,
                        atr_before,
                        displacement_strength_atr,
                        fresh_entry_eligible,
                    ),
                    self.rules.max_zones_per_side,
                );
                *last_source = Some(source.ts_event);
                self.funnel.zones_created += 1;
            }
        }

        self.atr.handle_bar(&bar);
        self.previous_k = Some(current_k);
        let signal = long_signal.or(short_signal);
        self.funnel.signals += u64::from(signal.is_some());
        signal
    }

    /// 4 小时方向变化时丢弃旧结构下生成的 level，后续只接受新方向同侧的 level
    fn align_levels_with_trend(&mut self, trend: Trend) {
        if trend == self.level_trend {
            return;
        }
        self.demand.clear();
        self.supply.clear();
        self.last_demand_source = None;
        self.last_supply_source = None;
        self.level_trend = trend;
    }
}

/// 从新到旧推进全部有效区域，同一根 Bar 最多选择最近的一个确认 setup
fn observe_zones(
    zones: &mut VecDeque<Zone>,
    bar: Bar,
    confirmation: Confirmation,
    allow_entry: bool,
    side: OrderSide,
    rules: SignalRules,
) -> Option<Signal> {
    let mut signal = None;
    for index in (0..zones.len()).rev() {
        let observation = zones[index].observe(
            bar,
            confirmation,
            allow_entry && signal.is_none(),
            side,
            rules,
        );
        match observation {
            ZoneObservation::Keep => {}
            ZoneObservation::Remove => {
                zones.remove(index);
            }
            ZoneObservation::Signal(candidate) => {
                zones.remove(index);
                signal = Some(candidate);
            }
        }
    }
    signal
}

/// 加入最新 level；集合达到容量时仅淘汰最旧项，保证内存和每根 Bar 的扫描成本有界
fn push_zone(zones: &mut VecDeque<Zone>, zone: Zone, max_zones: usize) {
    if zones.len() == max_zones {
        zones.pop_front();
    }
    zones.push_back(zone);
}

/// 在近期顶部/底部寻找最后一根反向源 K 线，并验证随后 1 至 N 根 Bar 达到 ATR 位移阈值
///
/// Demand 要求源 K 线收跌、当前收盘突破源高点且靠近位移区间上沿；Supply 条件完全镜像。
/// 返回值同时标记源 K 是否处于近期极值：极值区可首次回测，非极值区只能等待破位收复；强度
/// 使用生成前 ATR 归一化，便于在价格和波动率不同的标的之间比较。
fn displacement_zone(
    bars: &VecDeque<Bar>,
    atr: f64,
    rules: SignalRules,
) -> Option<(ZoneKind, Bar, f64, bool)> {
    if atr <= 0.0 {
        return None;
    }
    let current = *bars.back()?;
    let first_source = bars.len().saturating_sub(rules.displacement_max_bars + 1);
    for source_index in (first_source..bars.len().saturating_sub(1)).rev() {
        let source = bars[source_index];
        let context_start = source_index.saturating_sub(rules.level_extreme_lookback_bars);
        let at_recent_bottom = bars
            .iter()
            .skip(context_start)
            .all(|candidate| source.low <= candidate.low);
        let at_recent_top = bars
            .iter()
            .skip(context_start)
            .all(|candidate| source.high >= candidate.high);
        let mut impulse_high = f64::NEG_INFINITY;
        let mut impulse_low = f64::INFINITY;
        for impulse in bars.iter().skip(source_index + 1) {
            impulse_high = impulse_high.max(impulse.high.as_f64());
            impulse_low = impulse_low.min(impulse.low.as_f64());
        }
        let impulse_range = impulse_high - impulse_low;
        if impulse_range <= 0.0 {
            continue;
        }
        let required_move = atr * rules.displacement_atr_multiple;
        let upward_move = current.close.as_f64() - source.close.as_f64();
        let downward_move = source.close.as_f64() - current.close.as_f64();
        if source.close < source.open
            && current.close > source.high
            && upward_move >= required_move
            && (impulse_high - current.close.as_f64()) / impulse_range
                <= rules.displacement_close_fraction
        {
            return Some((
                ZoneKind::Demand,
                source,
                upward_move / atr,
                at_recent_bottom,
            ));
        }
        if source.close > source.open
            && current.close < source.low
            && downward_move >= required_move
            && (current.close.as_f64() - impulse_low) / impulse_range
                <= rules.displacement_close_fraction
        {
            return Some((ZoneKind::Supply, source, downward_move / atr, at_recent_top));
        }
    }
    None
}

/// 所有标的共同竞争的账户级风险上限
#[derive(Clone, Copy, Debug)]
struct AccountRiskLimits {
    daily_loss: Decimal,
    open_risk: Decimal,
    account_notional: Decimal,
    open_positions: usize,
}

/// 一笔未完成入场预先占用的最大初始风险与名义金额
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct RiskReservation {
    #[serde(
        serialize_with = "serialize_decimal_as_str",
        deserialize_with = "deserialize_decimal_from_str"
    )]
    risk: Decimal,
    #[serde(
        serialize_with = "serialize_decimal_as_str",
        deserialize_with = "deserialize_decimal_from_str"
    )]
    notional: Decimal,
}

/// 可持久化的跨策略账户风险账本
///
/// 多标的采用独立策略实例，但通过此状态共享当日盈亏、停机状态、交易次数及未释放预留，
/// 防止并发信号分别通过单标的检查后合计突破账户上限。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
struct AccountRiskState {
    version: u8,
    date: Option<String>,
    entries_by_symbol: HashMap<String, usize>,
    #[serde(
        serialize_with = "serialize_decimal_as_str",
        deserialize_with = "deserialize_decimal_from_str"
    )]
    realized_pnl: Decimal,
    halted: bool,
    reservations: HashMap<String, RiskReservation>,
}

impl Default for AccountRiskState {
    fn default() -> Self {
        Self {
            version: 1,
            date: None,
            entries_by_symbol: HashMap::new(),
            realized_pnl: Decimal::ZERO,
            halted: false,
            reservations: HashMap::new(),
        }
    }
}

/// 入场在数量、单标的或账户层被拒绝的稳定分类，用于日志与统计聚合
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum RiskRejectionReason {
    ZeroQuantity,
    RiskUnderutilized,
    SymbolTradeLimit,
    AccountHalted,
    DailyLoss,
    OpenPositions,
    OpenRisk,
    AccountNotional,
    ExistingSymbolReservation,
}

impl Display for RiskRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQuantity => write!(f, "zero_quantity"),
            Self::RiskUnderutilized => write!(f, "risk_underutilized"),
            Self::SymbolTradeLimit => write!(f, "symbol_trade_limit"),
            Self::AccountHalted => write!(f, "account_halted"),
            Self::DailyLoss => write!(f, "daily_loss_limit"),
            Self::OpenPositions => write!(f, "open_positions_limit"),
            Self::OpenRisk => write!(f, "open_risk_limit"),
            Self::AccountNotional => write!(f, "account_notional_limit"),
            Self::ExistingSymbolReservation => write!(f, "existing_symbol_reservation"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservationOutcome {
    Reserved,
    Rejected(RiskRejectionReason),
}

/// 某次风险决策时账户账本的只读快照，保证日志可以解释拒绝原因
#[derive(Clone, Copy, Debug)]
struct AccountRiskSnapshot {
    realized_pnl: Decimal,
    halted: bool,
    open_risk: Decimal,
    account_notional: Decimal,
    open_positions: usize,
    entries_for_symbol: usize,
}

/// 以互斥锁和原子文件替换协调多标的策略的共享风险账本
///
/// 所有检查、预留和释放均在锁内完成；持久化失败会恢复内存旧状态，避免内存与磁盘账本分叉。
#[derive(Debug)]
struct AccountRisk {
    path: PathBuf,
    state: Mutex<AccountRiskState>,
}

impl AccountRisk {
    /// 从磁盘加载共享账户风险状态；首次运行创建空状态，损坏或不安全内容按 fail-closed 处理
    fn load(path: PathBuf) -> anyhow::Result<Self> {
        let state = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read SLC risk state {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse SLC risk state {}", path.display()))?
        } else {
            AccountRiskState::default()
        };
        validate_account_risk_state(&state)?;
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// 在入场命令到达执行引擎前原子预留账户风险、名义金额、持仓槽位和当日交易次数
    ///
    /// 先在互斥锁内滚动交易日并检查全部账户门槛，通过后才持久化 reservation。任何失败都不会
    /// 让订单先于风险状态到达执行引擎，从而避免多个 symbol 同时看到同一份剩余额度。
    fn reserve_entry(
        &self,
        symbol: &str,
        date: jiff::civil::Date,
        reservation: RiskReservation,
        max_trades_per_symbol: usize,
        limits: AccountRiskLimits,
    ) -> anyhow::Result<(ReservationOutcome, AccountRiskSnapshot)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        roll_account_risk_date(&mut state, date);
        let snapshot = account_risk_snapshot(&state, symbol);
        let rejection = if snapshot.entries_for_symbol >= max_trades_per_symbol {
            Some(RiskRejectionReason::SymbolTradeLimit)
        } else if snapshot.halted {
            Some(RiskRejectionReason::AccountHalted)
        } else if snapshot.realized_pnl <= -limits.daily_loss {
            Some(RiskRejectionReason::DailyLoss)
        } else if snapshot.open_positions >= limits.open_positions {
            Some(RiskRejectionReason::OpenPositions)
        } else if snapshot.open_risk + reservation.risk > limits.open_risk {
            Some(RiskRejectionReason::OpenRisk)
        } else if snapshot.account_notional + reservation.notional > limits.account_notional {
            Some(RiskRejectionReason::AccountNotional)
        } else if state.reservations.contains_key(symbol) {
            Some(RiskRejectionReason::ExistingSymbolReservation)
        } else {
            None
        };
        if let Some(reason) = rejection {
            if *state != before {
                self.persist_or_restore(&mut state, before)?;
            }
            return Ok((ReservationOutcome::Rejected(reason), snapshot));
        }

        *state
            .entries_by_symbol
            .entry(symbol.to_string())
            .or_default() += 1;
        state.reservations.insert(symbol.to_string(), reservation);
        self.persist_or_restore(&mut state, before)?;
        Ok((
            ReservationOutcome::Reserved,
            account_risk_snapshot(&state, symbol),
        ))
    }

    /// 未发生任何成交时释放入场 reservation，并回退该 symbol 的当日交易次数
    fn release_unfilled(&self, symbol: &str) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        let remove_entry_count = state.reservations.remove(symbol).is_some()
            && state
                .entries_by_symbol
                .get_mut(symbol)
                .is_some_and(|entries| {
                    *entries = entries.saturating_sub(1);
                    *entries == 0
                });
        if remove_entry_count {
            state.entries_by_symbol.remove(symbol);
        }
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// 已成交头寸结束后释放开放风险，但保留当日已执行交易次数
    fn release_reservation(&self, symbol: &str) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        state.reservations.remove(symbol);
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// 记录已实现 PnL；仅在没有剩余入场数量可能再次成交时释放 reservation
    fn record_close(
        &self,
        symbol: &str,
        date: jiff::civil::Date,
        realized_pnl: Option<Decimal>,
        release_reservation: bool,
    ) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        roll_account_risk_date(&mut state, date);
        if release_reservation {
            state.reservations.remove(symbol);
        }
        if let Some(pnl) = realized_pnl {
            state.realized_pnl += pnl;
        } else {
            state.halted = true;
        }
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// 对账单个 symbol；券商存在敞口但本地无风险 reservation 时立即停止账户新入场
    fn reconcile_symbol(
        &self,
        symbol: &str,
        date: jiff::civil::Date,
        has_exposure: bool,
    ) -> anyhow::Result<AccountRiskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC account risk state mutex was poisoned"))?;
        let before = state.clone();
        roll_account_risk_date(&mut state, date);
        if has_exposure && !state.reservations.contains_key(symbol) {
            state.halted = true;
        } else if !has_exposure {
            state.reservations.remove(symbol);
        }
        self.persist_or_restore(&mut state, before)?;
        Ok(account_risk_snapshot(&state, symbol))
    }

    /// 返回前持久化风险状态；写盘失败时恢复修改前内存快照，禁止仅内存生效的不一致状态
    fn persist_or_restore(
        &self,
        state: &mut AccountRiskState,
        before: AccountRiskState,
    ) -> anyhow::Result<()> {
        if *state == before {
            return Ok(());
        }
        if let Err(e) = self.persist(state) {
            *state = before;
            return Err(e);
        }
        Ok(())
    }

    /// 覆盖写入完整的小型风险状态文件并执行磁盘同步，成功后订单流程才可继续
    fn persist(&self, state: &AccountRiskState) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create SLC risk state directory {}",
                    parent.display()
                )
            })?;
        }
        let encoded = toml::to_string(state).context("failed to serialize SLC risk state")?;
        let mut file = fs::File::create(&self.path)
            .with_context(|| format!("failed to create SLC risk state {}", self.path.display()))?;
        file.write_all(encoded.as_bytes())
            .with_context(|| format!("failed to write SLC risk state {}", self.path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync SLC risk state {}", self.path.display()))
    }
}

/// 在持久化状态参与实盘风控前拒绝版本不兼容、非正风险或非正名义金额
fn validate_account_risk_state(state: &AccountRiskState) -> anyhow::Result<()> {
    anyhow::ensure!(state.version == 1, "unsupported SLC risk state version");
    anyhow::ensure!(
        state
            .reservations
            .values()
            .all(|reservation| reservation.risk > Decimal::ZERO
                && reservation.notional > Decimal::ZERO),
        "SLC risk state contains a non-positive reservation",
    );
    Ok(())
}

/// 切换到新的美股交易日，清零日内计数与 PnL，但保留意外隔夜敞口对应的风险预留
fn roll_account_risk_date(state: &mut AccountRiskState, date: jiff::civil::Date) {
    let date = date.to_string();
    if state.date.as_deref() == Some(date.as_str()) {
        return;
    }
    state.date = Some(date);
    state.entries_by_symbol.clear();
    state.realized_pnl = Decimal::ZERO;
    state.halted = false;
}

/// 从 reservation 账本精确汇总账户 PnL、开放风险、名义金额、持仓数及单标的次数
fn account_risk_snapshot(state: &AccountRiskState, symbol: &str) -> AccountRiskSnapshot {
    AccountRiskSnapshot {
        realized_pnl: state.realized_pnl,
        halted: state.halted,
        open_risk: state
            .reservations
            .values()
            .map(|reservation| reservation.risk)
            .sum(),
        account_notional: state
            .reservations
            .values()
            .map(|reservation| reservation.notional)
            .sum(),
        open_positions: state.reservations.len(),
        entries_for_symbol: state
            .entries_by_symbol
            .get(symbol)
            .copied()
            .unwrap_or_default(),
    }
}

/// 判断相邻常规时段 Bar 的开始时间是否并非严格相差 5 分钟
fn has_five_minute_gap(previous: UnixNanos, current: UnixNanos) -> bool {
    current.as_u64().saturating_sub(previous.as_u64()) != FIVE_MINUTE_NANOS
}

/// 仅在仍有敞口且尚未发起退出时触发一次收盘前退出
fn should_request_preclose_exit(
    close_minute: u16,
    flatten_minute: u16,
    has_exposure: bool,
    exit_pending: bool,
) -> bool {
    close_minute >= flatten_minute && has_exposure && !exit_pending
}

/// 计算确认收盘价到 zone 最近边界的距离，并使用 zone 创建时 ATR 归一化
fn zone_distance_atr(zone: Zone, close: Price, atr: f64) -> f64 {
    if atr <= 0.0 {
        return f64::INFINITY;
    }
    let distance = if close < zone.low {
        zone.low.as_f64() - close.as_f64()
    } else if close > zone.high {
        close.as_f64() - zone.high.as_f64()
    } else {
        0.0
    };
    distance / atr
}

/// 返回确认 K 线收盘价靠近交易方向有利端的比例，实体方向越强该值越接近 1
fn directional_close_location(bar: Bar, side: OrderSide) -> f64 {
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

/// 按持仓槽位均分账户名义上限，防止首个订单耗尽其他标的的全部容量
fn per_position_notional_limit(
    account_notional: Decimal,
    open_positions: usize,
    order_notional: Decimal,
) -> Decimal {
    let open_positions = u64::try_from(open_positions).expect("validated position limit fits u64");
    order_notional.min(account_notional / Decimal::from(open_positions))
}

/// 在回测与 Longbridge 实盘之间选择语义等价的止损类型：StopMarket 或 MIT
fn protective_stop_order_type(is_backtest: bool) -> OrderType {
    if is_backtest {
        OrderType::StopMarket
    } else {
        OrderType::MarketIfTouched
    }
}

/// 构造仅减仓的回测 OUO 止损/目标组合，使任一成交都会缩量或取消另一保护单
fn backtest_protective_orders(
    event: &OrderFilled,
    exit_side: OrderSide,
    stop: Price,
    target: Price,
    order_list_id: OrderListId,
    stop_order_id: ClientOrderId,
    target_order_id: ClientOrderId,
) -> anyhow::Result<Vec<OrderAny>> {
    let stop_order = StopMarketOrder::new_checked(
        event.trader_id,
        event.strategy_id,
        event.instrument_id,
        stop_order_id,
        exit_side,
        event.last_qty,
        stop,
        TriggerType::Default,
        TimeInForce::Day,
        None,
        true,
        false,
        None,
        None,
        None,
        Some(ContingencyType::Ouo),
        Some(order_list_id),
        Some(vec![target_order_id]),
        None,
        None,
        None,
        None,
        Some(vec![Ustr::from("SLC_STOP")]),
        UUID4::new(),
        event.ts_init,
    )?;
    let target_order = LimitOrder::new_checked(
        event.trader_id,
        event.strategy_id,
        event.instrument_id,
        target_order_id,
        exit_side,
        event.last_qty,
        target,
        TimeInForce::Day,
        None,
        false,
        true,
        false,
        None,
        None,
        None,
        Some(ContingencyType::Ouo),
        Some(order_list_id),
        Some(vec![stop_order_id]),
        None,
        None,
        None,
        None,
        Some(vec![Ustr::from("SLC_TARGET")]),
        UUID4::new(),
        event.ts_init,
    )?;
    Ok(vec![
        OrderAny::StopMarket(stop_order),
        OrderAny::Limit(target_order),
    ])
}

/// 已平仓交易的退出归因；支持部分成交经过多条退出路径时标记为 `Mixed`
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TradeExitReason {
    Target,
    Stop,
    PreClose,
    RiskExit,
    Mixed,
    Unknown,
}

impl TradeExitReason {
    /// 合并多次部分退出原因；若同一交易经不同路径成交则保留为 Mixed，避免误归因
    fn combine(current: Option<Self>, next: Self) -> Self {
        match current {
            None => next,
            Some(current) if current == next => current,
            Some(_) => Self::Mixed,
        }
    }
}

impl Display for TradeExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Target => write!(f, "target"),
            Self::Stop => write!(f, "stop"),
            Self::PreClose => write!(f, "pre_close"),
            Self::RiskExit => write!(f, "risk_exit"),
            Self::Mixed => write!(f, "mixed"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// 一笔已关闭交易的信号质量、风险、路径和成本统计快照
///
/// 使用 `Option<Decimal>` 保留券商或引擎未提供 realized PnL 的事实，不以零值掩盖缺失数据。
#[derive(Clone, Copy, Debug)]
struct ClosedTradeStatistics {
    side: OrderSide,
    level: SignalLevel,
    level_age_bars: u64,
    confirmation_bars: u64,
    confirmation_close_location: f64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    entry_minute: u16,
    holding_bars: u64,
    mfe_r: Decimal,
    mae_r: Decimal,
    exit_reason: TradeExitReason,
    realized_pnl: Option<Decimal>,
    estimated_cost: Decimal,
    entry_slippage_stress: Decimal,
    initial_risk: Decimal,
    risk_utilization: Decimal,
    r_multiple: Option<Decimal>,
    close_ts: UnixNanos,
    ambiguous_exit_bar: bool,
}

impl ClosedTradeStatistics {
    /// 从已实现 PnL 中扣除配置的每股往返成本，缺失券商 PnL 时不伪造结果
    fn cost_adjusted_pnl(self) -> Option<Decimal> {
        self.realized_pnl.map(|pnl| pnl - self.estimated_cost)
    }

    /// 再扣除最坏允许入场滑点，并把 OHLC 路径不明的目标成交重估为完整止损亏损
    fn conservative_pnl(self) -> Option<Decimal> {
        self.cost_adjusted_pnl().map(|pnl| {
            if self.ambiguous_exit_bar && self.exit_reason == TradeExitReason::Target {
                -self.initial_risk - self.estimated_cost - self.entry_slippage_stress
            } else {
                pnl - self.entry_slippage_stress
            }
        })
    }
}

/// 单个标的的信号漏斗、风控拒绝原因和逐笔交易结果
#[derive(Debug, Default)]
struct SymbolRunStatistics {
    funnel: SignalFunnel,
    entries_submitted: u64,
    risk_rejections: BTreeMap<RiskRejectionReason, u64>,
    trades: Vec<ClosedTradeStatistics>,
}

/// 全部策略实例共享的诊断容器，只影响观测性，不参与信号或下单
#[derive(Debug, Default)]
struct RunStatistics {
    symbols: HashMap<InstrumentId, SymbolRunStatistics>,
}

impl RunStatistics {
    /// 生成顺序稳定、便于 grep 和跨轮比较的运行汇总、分标的及 cohort 诊断行
    fn lines(&self) -> Vec<String> {
        let all_trades = self
            .symbols
            .values()
            .flat_map(|symbol| symbol.trades.iter());
        let total = TradeAggregate::from_trades(all_trades.clone());
        let entries_submitted = self
            .symbols
            .values()
            .map(|symbol| symbol.entries_submitted)
            .sum::<u64>();
        let risk_rejections = self
            .symbols
            .values()
            .flat_map(|symbol| symbol.risk_rejections.values())
            .sum::<u64>();
        let mut lines = vec![format!(
            "SLC diagnostics total: entries_submitted={entries_submitted}, risk_rejections={risk_rejections}, {}",
            total.summary(),
        )];
        let mut rejections: BTreeMap<RiskRejectionReason, u64> = BTreeMap::new();
        for symbol in self.symbols.values() {
            for (reason, count) in &symbol.risk_rejections {
                *rejections.entry(*reason).or_default() += count;
            }
        }
        for (reason, count) in rejections {
            lines.push(format!(
                "SLC risk rejection: reason={reason}, count={count}",
            ));
        }

        let mut exits: BTreeMap<TradeExitReason, Vec<&ClosedTradeStatistics>> = BTreeMap::new();
        for trade in all_trades {
            exits.entry(trade.exit_reason).or_default().push(trade);
        }
        for (reason, trades) in exits {
            lines.push(format!(
                "SLC exit cohort: exit_reason={reason}, {}",
                TradeAggregate::from_trades(trades.into_iter()).summary(),
            ));
        }

        let mut side_cohorts: BTreeMap<String, Vec<&ClosedTradeStatistics>> = BTreeMap::new();
        let mut level_cohorts: BTreeMap<SignalLevel, Vec<&ClosedTradeStatistics>> = BTreeMap::new();
        for trade in self
            .symbols
            .values()
            .flat_map(|symbol| symbol.trades.iter())
        {
            side_cohorts
                .entry(trade.side.to_string())
                .or_default()
                .push(trade);
            level_cohorts.entry(trade.level).or_default().push(trade);
        }
        for (side, trades) in side_cohorts {
            lines.push(format!(
                "SLC side cohort: side={side}, {}",
                TradeAggregate::from_trades(trades.into_iter()).summary(),
            ));
        }
        for (level, trades) in level_cohorts {
            lines.push(format!(
                "SLC level cohort: level={level}, {}",
                TradeAggregate::from_trades(trades.into_iter()).summary(),
            ));
        }

        let mut time_cohorts: BTreeMap<u16, Vec<&ClosedTradeStatistics>> = BTreeMap::new();
        for trade in self
            .symbols
            .values()
            .flat_map(|symbol| symbol.trades.iter())
        {
            time_cohorts
                .entry((trade.entry_minute / 30) * 30)
                .or_default()
                .push(trade);
        }
        for (minute, trades) in time_cohorts {
            lines.push(format!(
                "SLC entry-time cohort: bucket={:02}:{:02}, {}",
                minute / 60,
                minute % 60,
                TradeAggregate::from_trades(trades.into_iter()).summary(),
            ));
        }

        let mut symbols = self
            .symbols
            .iter()
            .map(|(instrument_id, statistics)| (instrument_id.to_string(), statistics))
            .collect::<Vec<_>>();
        symbols.sort_by(|left, right| left.0.cmp(&right.0));
        for (instrument_id, statistics) in symbols {
            let rejected = statistics.risk_rejections.values().sum::<u64>();
            lines.push(format!(
                "[{instrument_id}] SLC diagnostics: 5m_bars={}, directional_4h_bars={}, zones_created={}, level_touches={}, stochastic_extremes={}, stochastic_reentries={}, signals={}, entries_submitted={}, risk_rejections={}, {}",
                statistics.funnel.five_minute_bars,
                statistics.funnel.directional_bars,
                statistics.funnel.zones_created,
                statistics.funnel.level_touches,
                statistics.funnel.stochastic_extremes,
                statistics.funnel.stochastic_reentries,
                statistics.funnel.signals,
                statistics.entries_submitted,
                rejected,
                TradeAggregate::from_trades(statistics.trades.iter()).summary(),
            ));
            let mut cohorts: BTreeMap<(String, SignalLevel), Vec<&ClosedTradeStatistics>> =
                BTreeMap::new();
            for trade in &statistics.trades {
                cohorts
                    .entry((trade.side.to_string(), trade.level))
                    .or_default()
                    .push(trade);
            }
            for ((side, level), trades) in cohorts {
                lines.push(format!(
                    "[{instrument_id}] SLC trade cohort: side={side}, level={level}, confirmation=stochastic_reentry, {}",
                    TradeAggregate::from_trades(trades.into_iter()).summary(),
                ));
            }
        }
        lines
    }
}

/// 从保守逐日组合 PnL 计算的账户级回测风险指标
#[derive(Clone, Copy, Debug, Default)]
struct BacktestRiskMetrics {
    sharpe: Option<f64>,
    max_drawdown_pct: Option<f64>,
    annualized_return_pct: Option<f64>,
    calmar: Option<f64>,
    positive_days: u64,
    negative_days: u64,
    flat_days: u64,
}

/// 用逐日精确 PnL 和账户权益计算 Sharpe、回撤、年化及 Calmar，并保留零交易日
fn risk_metrics_from_daily_pnl(
    daily_pnl: &[Decimal],
    starting_balance: Decimal,
) -> Option<BacktestRiskMetrics> {
    if daily_pnl.is_empty() || starting_balance <= Decimal::ZERO {
        return None;
    }
    let mut equity = starting_balance;
    let mut peak = starting_balance;
    let mut max_drawdown = 0.0_f64;
    let mut returns = Vec::with_capacity(daily_pnl.len());
    let mut metrics = BacktestRiskMetrics::default();
    for pnl in daily_pnl {
        if equity <= Decimal::ZERO {
            return None;
        }
        returns.push((*pnl / equity).to_f64()?);
        match pnl.cmp(&Decimal::ZERO) {
            std::cmp::Ordering::Greater => metrics.positive_days += 1,
            std::cmp::Ordering::Less => metrics.negative_days += 1,
            std::cmp::Ordering::Equal => metrics.flat_days += 1,
        }
        equity += *pnl;
        if equity <= Decimal::ZERO {
            return None;
        }
        peak = peak.max(equity);
        max_drawdown = max_drawdown.min((equity / peak).to_f64()? - 1.0);
    }
    let day_count = u32::try_from(daily_pnl.len()).ok()?;
    let annualized_return = (equity / starting_balance)
        .to_f64()?
        .powf(252.0 / f64::from(day_count))
        - 1.0;
    metrics.sharpe = annualized_sharpe(&returns);
    metrics.max_drawdown_pct = Some(max_drawdown * 100.0);
    metrics.annualized_return_pct = Some(annualized_return * 100.0);
    metrics.calmar =
        (max_drawdown < -f64::EPSILON).then_some(annualized_return / max_drawdown.abs());
    Some(metrics)
}

/// 汇总扣费、最坏入场滑点及模糊路径压力后的逐日组合 PnL，并保留零交易日
fn conservative_daily_pnl(
    statistics: &RunStatistics,
    trading_days: &BTreeSet<String>,
    timezone: &TimeZone,
) -> Option<Vec<Decimal>> {
    let mut pnl_by_day: BTreeMap<String, Decimal> = BTreeMap::new();
    for trade in statistics
        .symbols
        .values()
        .flat_map(|symbol| symbol.trades.iter())
    {
        let pnl = trade.conservative_pnl()?;
        let date = trade
            .close_ts
            .to_datetime_utc()
            .to_zoned(timezone.clone())
            .date()
            .to_string();
        *pnl_by_day.entry(date).or_default() += pnl;
    }

    Some(
        trading_days
            .iter()
            .map(|date| pnl_by_day.get(date).copied().unwrap_or_default())
            .collect(),
    )
}

/// 将逐笔交易汇总成总计或 cohort 共用的绩效字段
#[derive(Debug, Default)]
struct TradeAggregate {
    trades: u64,
    wins: u64,
    cost_adjusted_wins: u64,
    realized_pnl: Decimal,
    estimated_cost: Decimal,
    entry_slippage_stress: Decimal,
    cost_adjusted_pnl: Decimal,
    conservative_pnl: Decimal,
    r_sum: Decimal,
    cost_adjusted_r_sum: Decimal,
    conservative_r_sum: Decimal,
    r_count: u64,
    initial_risk_sum: Decimal,
    risk_utilization_sum: Decimal,
    mfe_r_sum: Decimal,
    mae_r_sum: Decimal,
    holding_bars: u64,
    level_age_bars: u64,
    confirmation_bars: u64,
    confirmation_close_location_sum: f64,
    distance_atr_sum: f64,
    zone_width_atr_sum: f64,
    displacement_strength_atr_sum: f64,
    ambiguous_exit_bars: u64,
}

impl TradeAggregate {
    /// 聚合精确交易值；即使 PnL 缺失也保留交易计数，使数据质量问题不会被静默隐藏
    fn from_trades<'a>(trades: impl Iterator<Item = &'a ClosedTradeStatistics>) -> Self {
        let mut aggregate = Self::default();
        for trade in trades {
            aggregate.trades += 1;
            aggregate.initial_risk_sum += trade.initial_risk;
            aggregate.risk_utilization_sum += trade.risk_utilization;
            aggregate.mfe_r_sum += trade.mfe_r;
            aggregate.mae_r_sum += trade.mae_r;
            aggregate.holding_bars += trade.holding_bars;
            aggregate.level_age_bars += trade.level_age_bars;
            aggregate.confirmation_bars += trade.confirmation_bars;
            aggregate.confirmation_close_location_sum += trade.confirmation_close_location;
            aggregate.distance_atr_sum += trade.distance_atr;
            aggregate.zone_width_atr_sum += trade.zone_width_atr;
            aggregate.displacement_strength_atr_sum += trade.displacement_strength_atr;
            aggregate.estimated_cost += trade.estimated_cost;
            aggregate.entry_slippage_stress += trade.entry_slippage_stress;
            aggregate.ambiguous_exit_bars += u64::from(trade.ambiguous_exit_bar);
            if let Some(pnl) = trade.realized_pnl {
                let cost_adjusted_pnl = trade
                    .cost_adjusted_pnl()
                    .expect("realized PnL checked above");
                aggregate.realized_pnl += pnl;
                aggregate.cost_adjusted_pnl += cost_adjusted_pnl;
                aggregate.conservative_pnl += trade
                    .conservative_pnl()
                    .expect("realized PnL checked above");
                aggregate.wins += u64::from(pnl > Decimal::ZERO);
                aggregate.cost_adjusted_wins += u64::from(cost_adjusted_pnl > Decimal::ZERO);
                if trade.initial_risk > Decimal::ZERO {
                    aggregate.cost_adjusted_r_sum += cost_adjusted_pnl / trade.initial_risk;
                    aggregate.conservative_r_sum += trade
                        .conservative_pnl()
                        .expect("realized PnL checked above")
                        / trade.initial_risk;
                }
            }
            if let Some(r_multiple) = trade.r_multiple {
                aggregate.r_sum += r_multiple;
                aggregate.r_count += 1;
            }
        }
        aggregate
    }

    /// 格式化总计与 cohort 共用的紧凑绩效字段，并明确输出缺失样本
    fn summary(&self) -> String {
        let win_rate = average(self.wins * 100, self.trades);
        let cost_adjusted_win_rate = average(self.cost_adjusted_wins * 100, self.trades);
        let average_r = decimal_average(self.r_sum, self.r_count);
        let average_cost_adjusted_r = decimal_average(self.cost_adjusted_r_sum, self.r_count);
        let average_conservative_r = decimal_average(self.conservative_r_sum, self.r_count);
        let average_initial_risk = decimal_average(self.initial_risk_sum, self.trades);
        let average_risk_utilization = decimal_average(self.risk_utilization_sum, self.trades);
        let average_mfe_r = decimal_average(self.mfe_r_sum, self.trades);
        let average_mae_r = decimal_average(self.mae_r_sum, self.trades);
        let average_holding_bars = average(self.holding_bars, self.trades);
        let average_level_age_bars = average(self.level_age_bars, self.trades);
        let average_confirmation_bars = average(self.confirmation_bars, self.trades);
        let average_confirmation_close_location =
            float_average(self.confirmation_close_location_sum, self.trades);
        let average_distance_atr = float_average(self.distance_atr_sum, self.trades);
        let average_zone_width_atr = float_average(self.zone_width_atr_sum, self.trades);
        let average_displacement_atr =
            float_average(self.displacement_strength_atr_sum, self.trades);
        format!(
            "trades={}, wins={}, win_rate_pct={win_rate}, cost_adjusted_win_rate_pct={cost_adjusted_win_rate}, realized_pnl={}, estimated_cost={}, entry_slippage_stress={}, cost_adjusted_pnl={}, conservative_pnl={}, average_r={average_r}, average_cost_adjusted_r={average_cost_adjusted_r}, average_conservative_r={average_conservative_r}, average_initial_risk={average_initial_risk}, average_risk_utilization={average_risk_utilization}, average_mfe_r={average_mfe_r}, average_mae_r={average_mae_r}, average_holding_bars={average_holding_bars}, average_level_age_bars={average_level_age_bars}, average_confirmation_bars={average_confirmation_bars}, average_confirmation_close_location={average_confirmation_close_location}, average_distance_atr={average_distance_atr}, average_zone_width_atr={average_zone_width_atr}, average_displacement_atr={average_displacement_atr}, ambiguous_exit_bars={}",
            self.trades,
            self.wins,
            self.realized_pnl,
            self.estimated_cost,
            self.entry_slippage_stress,
            self.cost_adjusted_pnl,
            self.conservative_pnl,
            self.ambiguous_exit_bars,
        )
    }
}

/// 返回稳定的四位小数 Decimal 平均值；无有效样本时输出 `n/a`
fn decimal_average(total: Decimal, count: u64) -> String {
    if count == 0 {
        "n/a".to_string()
    } else {
        (total / Decimal::from(count)).round_dp(4).to_string()
    }
}

/// 将整数累计值转换成适合百分比统计的平均数字符串
fn average(total: u64, count: u64) -> String {
    decimal_average(Decimal::from(total), count)
}

/// 为 ATR 距离、收盘位置等归一化 setup 指标返回稳定的四位浮点平均值
fn float_average(total: f64, count: u64) -> String {
    if count == 0 {
        "n/a".to_string()
    } else {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        format!("{:.4}", total / f64::from(count))
    }
}

/// 使用零无风险利率和样本标准差计算日收益年化 Sharpe；样本不足或零方差返回 None
fn annualized_sharpe(returns: &[f64]) -> Option<f64> {
    if returns.len() < 2 {
        return None;
    }
    let count = u32::try_from(returns.len()).ok()?;
    let mean = returns.iter().sum::<f64>() / f64::from(count);
    let variance = returns
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / f64::from(count - 1);
    (variance > f64::EPSILON).then(|| mean / variance.sqrt() * 252.0_f64.sqrt())
}

/// 应用预先声明的 IS/OOS Sharpe 衰减规则，任何非正样本都不会被判为可接受
fn walk_forward_verdict(in_sample_sharpe: f64, out_of_sample_sharpe: f64) -> &'static str {
    if in_sample_sharpe <= 0.0 {
        "reject_non_positive_is"
    } else if out_of_sample_sharpe <= 0.0 {
        "reject_non_positive_oos"
    } else if out_of_sample_sharpe < in_sample_sharpe * 0.5 {
        "possible_overfit"
    } else {
        "acceptable"
    }
}

/// 检测高低范围同时覆盖止损与目标的 OHLC Bar，此时无法知道真实盘中触发顺序
fn bar_reaches_stop_and_target(stop: Price, target: Price, bar: Bar) -> bool {
    let lower = stop.min(target);
    let upper = stop.max(target);
    bar.low <= lower && bar.high >= upper
}

/// 单标的策略实例运行时所需的已解析配置
///
/// 应用级配置在构造实例时转换成此结构，使交易热路径不再读取配置文件或重复计算交易时段。
#[derive(Clone, Debug)]
struct SlcStrategyConfig {
    instrument_id: InstrumentId,
    five_minute_bar_type: BarType,
    four_hour_bar_type: BarType,
    timezone: TimeZone,
    entry_start_minute: u16,
    entry_end_minute: u16,
    flatten_minute: u16,
    max_trades_per_day: usize,
    risk_amount: Decimal,
    account_risk_limits: AccountRiskLimits,
    max_order_quantity: Quantity,
    max_order_notional: Decimal,
    minimum_risk_utilization: Decimal,
    max_entry_slippage_ticks: u64,
    risk_reward: Decimal,
    stop_buffer_ticks: u64,
    round_trip_cost_per_share: Decimal,
    log_bars: bool,
}

/// 已提交但尚未完全成交的入场意图
///
/// 信号快照一直保留到成交、拒单或撤单，以便部分成交后仍能按原始 setup 构造保护单和统计。
#[derive(Clone, Copy, Debug)]
struct PendingEntry {
    client_order_id: ClientOrderId,
    side: OrderSide,
    level: SignalLevel,
    level_age_bars: u64,
    confirmation_bars: u64,
    confirmation_close_location: f64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    entry_limit: Price,
    stop: Price,
    signal_ts: UnixNanos,
    had_fill: bool,
}

/// 一笔已发生入场成交、正在接受保护和退出管理的交易
///
/// `filled_qty` 与 `protected_qty` 分开累计：每次部分成交后只为新增数量补齐 stop/target，
/// 同时用精确成交金额维护平均入场价、初始风险、MFE 和 MAE。
#[derive(Debug)]
struct ActiveTrade {
    side: OrderSide,
    level: SignalLevel,
    level_age_bars: u64,
    confirmation_bars: u64,
    confirmation_close_location: f64,
    distance_atr: f64,
    zone_width_atr: f64,
    displacement_strength_atr: f64,
    entry_minute: u16,
    entry_limit: Price,
    stop: Price,
    target: Price,
    first_fill_ts: UnixNanos,
    filled_qty: Decimal,
    protected_qty: Decimal,
    fill_notional: Decimal,
    initial_risk: Decimal,
    maximum_favorable_excursion: Decimal,
    maximum_adverse_excursion: Decimal,
    bars_held: u64,
    exit_reason: Option<TradeExitReason>,
}

impl ActiveTrade {
    /// 根据累计成交金额和数量返回所有部分成交的精确加权平均入场价
    fn average_fill(&self) -> Decimal {
        self.fill_notional / self.filled_qty
    }

    /// 用一个可成交或实际成交价格更新最大有利波动 MFE 和最大不利波动 MAE
    fn observe_price(&mut self, price: Price) {
        let entry = self.average_fill();
        let price = price.as_decimal();
        let (favorable, adverse) = match self.side {
            OrderSide::Buy => (price - entry, entry - price),
            OrderSide::Sell => (entry - price, price - entry),
            OrderSide::NoOrderSide => return,
        };
        self.maximum_favorable_excursion = self
            .maximum_favorable_excursion
            .max(favorable.max(Decimal::ZERO));
        self.maximum_adverse_excursion = self
            .maximum_adverse_excursion
            .max(adverse.max(Decimal::ZERO));
    }

    /// 用持仓后的完整 Bar 高低点同时更新 MFE、MAE，并增加持仓 Bar 数
    fn observe_bar(&mut self, bar: Bar) {
        self.observe_price(bar.high);
        self.observe_price(bar.low);
        self.bars_held += 1;
    }

    /// 返回以实际每股初始风险归一化的最大有利波动倍数
    fn mfe_r(&self) -> Decimal {
        self.normalized_excursion(self.maximum_favorable_excursion)
    }

    /// 返回以实际每股初始风险归一化的最大不利波动倍数
    fn mae_r(&self) -> Decimal {
        self.normalized_excursion(self.maximum_adverse_excursion)
    }

    /// 将每股波动转换为 R 倍数，并防止异常成交状态导致除零
    fn normalized_excursion(&self, excursion: Decimal) -> Decimal {
        if self.initial_risk <= Decimal::ZERO || self.filled_qty <= Decimal::ZERO {
            Decimal::ZERO
        } else {
            excursion / (self.initial_risk / self.filled_qty)
        }
    }
}

/// 回测中延迟判定同一根 OHLC Bar 同时触及止损和目标的保守路径探针
#[derive(Clone, Copy, Debug)]
struct AmbiguityProbe {
    stop: Price,
    target: Price,
    close_ts: UnixNanos,
}

/// Nautilus 单标的 SLC 策略实例，连接信号、订单生命周期与账户风险账本
///
/// 每个 symbol 各自维护指标、level、挂单和持仓，避免状态交叉；多个实例只共享 `AccountRisk`
/// 和 `RunStatistics`。这种边界既符合 Nautilus 的订单归属模型，也能对某一标的独立停用。
struct SlcStrategy {
    core: StrategyCore,
    config: SlcStrategyConfig,
    instrument: InstrumentAny,
    signals: SlcSignalState,
    backtest_four_hour_bars: Option<VecDeque<Bar>>,
    account_risk: Arc<AccountRisk>,
    run_statistics: Arc<Mutex<RunStatistics>>,
    pending_entry: Option<PendingEntry>,
    active_trade: Option<ActiveTrade>,
    ambiguity_probe: Option<AmbiguityProbe>,
    last_five_minute_bar: Option<Bar>,
    current_date: Option<jiff::civil::Date>,
    last_five_minute_bar_start: Option<UnixNanos>,
    suppress_warmup_boundary_signal: bool,
    session_disabled: bool,
    exit_pending: bool,
    faulted: bool,
    risk_rejections: u64,
    entries_submitted: u64,
}

/// 构造单个策略实例时由回测或实盘 runner 注入的运行环境差异
struct SlcRunConfig {
    flatten_minute: u16,
    backtest_four_hour_bars: Option<Vec<Bar>>,
    round_trip_cost_per_share: Decimal,
    log_bars: bool,
    run_statistics: Arc<Mutex<RunStatistics>>,
}

/// 为每个 symbol 构造稳定且唯一的策略路由身份，并声明其外部订单归属
fn slc_strategy_config(instrument_id: InstrumentId) -> StrategyConfig {
    StrategyConfig {
        strategy_id: Some(StrategyId::from(
            format!("{STRATEGY_ID}-{}", instrument_id.symbol).as_str(),
        )),
        external_order_claims: Some(vec![instrument_id]),
        manage_stop: true,
        market_exit_max_attempts: 300,
        market_exit_time_in_force: TimeInForce::Day,
        market_exit_reduce_only: false,
        ..Default::default()
    }
}

impl SlcStrategy {
    /// 创建一个信号与订单状态完全隔离的单标的策略实例，仅共享账户风险账本和运行统计
    ///
    /// 构造阶段先回放 warmup，并强制 5 分钟指标初始化成功。4 小时 pivot 未满足时允许节点启动，
    /// 但趋势保持 Neutral 且不能入场；这是数据尚不足的可恢复状态，而不是启动失败。
    fn new(
        app_config: &AppConfig,
        instrument_id: InstrumentId,
        instrument: InstrumentAny,
        five_minute_bars: Vec<Bar>,
        four_hour_bars: Vec<Bar>,
        account_risk: Arc<AccountRisk>,
        run_config: SlcRunConfig,
    ) -> anyhow::Result<Self> {
        let five_minute_bar_type = AppConfig::five_minute_bar_type(instrument_id);
        let four_hour_bar_type = AppConfig::four_hour_bar_type(instrument_id);
        let latest_entry_minute = run_config
            .flatten_minute
            .checked_sub(app_config.minimum_target_time_minutes)
            .context("minimum target time exceeds the available trading session")?;
        let entry_end_minute = app_config.session.entry_end_minute.min(latest_entry_minute);
        anyhow::ensure!(
            app_config.session.entry_start_minute < entry_end_minute,
            "minimum target time leaves no valid SLC entry window",
        );
        let five_minute_warmup_count = five_minute_bars.len();
        let four_hour_warmup_count = four_hour_bars.len();
        let mut signals = SlcSignalState::new(app_config);
        signals.warm_up(
            five_minute_bars,
            four_hour_bars,
            run_config.backtest_four_hour_bars.is_some(),
        );
        anyhow::ensure!(
            signals.indicators_initialized(),
            "SLC warmup did not initialize 5m indicators for {instrument_id}: bar_type={five_minute_bar_type}, received_bars={five_minute_warmup_count}, atr_initialized={}, stochastic_initialized={}",
            signals.atr.initialized(),
            signals.stochastics.initialized(),
        );
        if !signals.structure.initialized() {
            log::warn!(
                "[{instrument_id}] SLC 4h structure is not initialized: bar_type={four_hour_bar_type}, received_bars={four_hour_warmup_count}, confirmed_pivot_highs={}, confirmed_pivot_lows={}; the 4h trend remains Neutral and cannot authorize entries",
                signals.structure.highs.len(),
                signals.structure.lows.len(),
            );
        }
        log::info!(
            "[{instrument_id}] SLC warmup complete: 5m_bar_type={five_minute_bar_type}, 5m_bars={five_minute_warmup_count}, indicators_initialized={}, 4h_bar_type={four_hour_bar_type}, 4h_bars={four_hour_warmup_count}, structure_initialized={}, confirmed_pivot_highs={}, confirmed_pivot_lows={}, initial_4h_trend={:?}",
            signals.indicators_initialized(),
            signals.structure.initialized(),
            signals.structure.highs.len(),
            signals.structure.lows.len(),
            signals.structure.trend(),
        );
        signals.funnel = SignalFunnel::default();
        run_config
            .run_statistics
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC run statistics mutex was poisoned"))?
            .symbols
            .entry(instrument_id)
            .or_default();
        Ok(Self {
            core: StrategyCore::new(slc_strategy_config(instrument_id)),
            config: SlcStrategyConfig {
                instrument_id,
                five_minute_bar_type,
                four_hour_bar_type,
                timezone: app_config.timezone.clone(),
                entry_start_minute: app_config.session.entry_start_minute,
                entry_end_minute,
                flatten_minute: run_config.flatten_minute,
                max_trades_per_day: app_config.session.max_trades_per_day,
                risk_amount: app_config.risk_amount,
                account_risk_limits: AccountRiskLimits {
                    daily_loss: app_config.daily_loss_limit,
                    open_risk: app_config.max_open_risk,
                    account_notional: app_config.max_account_notional,
                    open_positions: app_config.max_open_positions,
                },
                max_order_quantity: app_config.max_order_quantity,
                max_order_notional: app_config.per_position_notional_limit(),
                minimum_risk_utilization: app_config.minimum_risk_utilization,
                max_entry_slippage_ticks: app_config.max_entry_slippage_ticks,
                risk_reward: app_config.risk_reward,
                stop_buffer_ticks: app_config.stop_buffer_ticks,
                round_trip_cost_per_share: run_config.round_trip_cost_per_share,
                log_bars: run_config.log_bars,
            },
            instrument,
            signals,
            backtest_four_hour_bars: run_config.backtest_four_hour_bars.map(VecDeque::from),
            account_risk,
            run_statistics: Arc::clone(&run_config.run_statistics),
            pending_entry: None,
            active_trade: None,
            ambiguity_probe: None,
            last_five_minute_bar: None,
            current_date: None,
            last_five_minute_bar_start: None,
            suppress_warmup_boundary_signal: true,
            session_disabled: false,
            exit_pending: false,
            faulted: false,
            risk_rejections: 0,
            entries_submitted: 0,
        })
    }

    /// 更新非关键运行诊断；统计锁损坏只记录错误，不反向中断订单风险处理
    fn update_run_statistics(&self, update: impl FnOnce(&mut SymbolRunStatistics)) {
        match self.run_statistics.lock() {
            Ok(mut statistics) => update(
                statistics
                    .symbols
                    .entry(self.config.instrument_id)
                    .or_default(),
            ),
            Err(_) => log::error!(
                "[{}] Failed to update SLC run statistics because the mutex was poisoned",
                self.config.instrument_id,
            ),
        }
    }

    /// 记录有效信号首先命中的账户风险拒绝原因，便于区分信号不足和容量不足
    fn record_risk_rejection(&mut self, reason: RiskRejectionReason) {
        self.risk_rejections += 1;
        self.update_run_statistics(|statistics| {
            *statistics.risk_rejections.entry(reason).or_default() += 1;
        });
    }

    /// 在多次部分成交之间保留退出原因，并识别止损、目标和管理退出混合成交
    fn mark_exit_reason(&mut self, reason: TradeExitReason) {
        if let Some(active) = self.active_trade.as_mut() {
            active.exit_reason = Some(TradeExitReason::combine(active.exit_reason, reason));
        }
    }

    /// 根据策略自有订单 tag 把成交归类为止损、目标或管理式风险退出
    fn record_exit_fill(&mut self, event: &OrderFilled) {
        let reason = self
            .cache()
            .order(&event.client_order_id)
            .and_then(|order| {
                let tags = order.tags()?;
                if tags.iter().any(|tag| tag.as_str() == "SLC_TARGET") {
                    Some(TradeExitReason::Target)
                } else if tags.iter().any(|tag| tag.as_str() == "SLC_STOP") {
                    Some(TradeExitReason::Stop)
                } else if tags.iter().any(|tag| tag.as_str() == "SLC_EXIT") {
                    Some(TradeExitReason::RiskExit)
                } else {
                    None
                }
            });
        if let Some(reason) = reason
            && let Some(active) = self.active_trade.as_mut()
        {
            active.observe_price(event.last_px);
            if reason != TradeExitReason::RiskExit || active.exit_reason.is_none() {
                active.exit_reason = Some(TradeExitReason::combine(active.exit_reason, reason));
            }
        }
    }

    /// 当同一根回测 OHLC Bar 同时触及两个保护价时，把刚结束的交易标为路径不确定
    fn inspect_ambiguous_exit_bar(&mut self, bar: Bar) {
        let Some(probe) = self.ambiguity_probe else {
            return;
        };
        if bar.ts_init < probe.close_ts {
            return;
        }
        self.ambiguity_probe = None;
        if bar.ts_init != probe.close_ts
            || !bar_reaches_stop_and_target(probe.stop, probe.target, bar)
        {
            return;
        }
        self.mark_ambiguous_exit_bar(probe, bar);
    }

    /// 在识别模糊 Bar 后反向更新对应已平仓统计，供保守 PnL 将目标重估为止损
    fn mark_ambiguous_exit_bar(&self, probe: AmbiguityProbe, bar: Bar) {
        self.update_run_statistics(|statistics| {
            if let Some(trade) = statistics
                .trades
                .iter_mut()
                .rev()
                .find(|trade| trade.close_ts == probe.close_ts)
            {
                trade.ambiguous_exit_bar = true;
            }
        });
        log::warn!(
            "[{}] SLC backtest exit is intrabar ambiguous: bar_type={}, bar_close={}, stop={}, target={}, low={}, high={}",
            self.config.instrument_id,
            bar.bar_type,
            bar.ts_init,
            probe.stop,
            probe.target,
            bar.low,
            bar.high,
        );
    }

    /// 把一根确认的 4 小时 Bar 应用于结构判断，不让高周期数据进入订单撮合
    fn process_finalized_four_hour_bar(&mut self, bar: Bar) -> anyhow::Result<()> {
        let local = bar
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
        if (RTH_OPEN_MINUTE..RTH_CLOSE_MINUTE).contains(&minute) {
            self.signals.process_four_hour(bar);
            if self.config.log_bars {
                log::info!(
                    "[{}] 4h structure updated: start={}, open={}, high={}, low={}, close={}, structure_initialized={}, confirmed_pivot_highs={}, confirmed_pivot_lows={}, trend={:?}",
                    self.config.instrument_id,
                    local,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    self.signals.structure.initialized(),
                    self.signals.structure.highs.len(),
                    self.signals.structure.lows.len(),
                    self.signals.structure.trend(),
                );
            }
        }
        Ok(())
    }

    /// 仅当回放时钟已进入下一周期时推进历史 4 小时 Bar，防止回测提前看到完整高周期数据
    fn advance_backtest_four_hour_bars(&mut self, timestamp: UnixNanos) -> anyhow::Result<()> {
        loop {
            let Some(next) = self
                .backtest_four_hour_bars
                .as_ref()
                .and_then(|bars| bars.front().copied())
                .filter(|bar| bar.ts_event <= timestamp)
            else {
                return Ok(());
            };
            self.backtest_four_hour_bars
                .as_mut()
                .expect("backtest bars checked above")
                .pop_front();
            if let Some(finalized) = self.signals.finalize_four_hour(next) {
                self.process_finalized_four_hour_bar(finalized)?;
            }
        }
    }

    /// 判断本策略是否拥有任何未结订单、在途订单或持仓，用于阻止重复暴露
    fn has_exposure(&self) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let instrument_id = self.config.instrument_id;
        let cache = self.cache();
        cache.orders_open_count(None, Some(&instrument_id), Some(&strategy_id), None, None) > 0
            || cache.orders_inflight_count(
                None,
                Some(&instrument_id),
                Some(&strategy_id),
                None,
                None,
            ) > 0
            || !cache
                .positions_open(None, Some(&instrument_id), Some(&strategy_id), None, None)
                .is_empty()
    }

    /// 判断本策略是否拥有当前 symbol 的未平仓头寸
    fn has_open_position(&self) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        !self
            .cache()
            .positions_open(
                None,
                Some(&self.config.instrument_id),
                Some(&strategy_id),
                None,
                None,
            )
            .is_empty()
    }

    /// 判断本策略是否拥有当前 symbol 的开放或仍在途订单
    fn has_open_orders(&self) -> bool {
        let strategy_id = self.strategy_id().expect("strategy is registered");
        let cache = self.cache();
        cache.orders_open_count(
            None,
            Some(&self.config.instrument_id),
            Some(&strategy_id),
            None,
            None,
        ) > 0
            || cache.orders_inflight_count(
                None,
                Some(&self.config.instrument_id),
                Some(&strategy_id),
                None,
                None,
            ) > 0
    }

    /// 为本策略拥有的当前 symbol 全部头寸提交带 `SLC_EXIT` 标签的市价退出
    fn close_positions(&mut self) -> anyhow::Result<()> {
        self.close_all_positions(
            self.config.instrument_id,
            None,
            None,
            Some(vec![Ustr::from("SLC_EXIT")]),
            Some(TimeInForce::Day),
            Some(false),
            Some(false),
            None,
        )
    }

    /// 订单生命周期失败后永久禁止本次运行的新入场，并启动框架管理式市价退出恢复
    fn disable_after_order_failure(&mut self, reason: &str) {
        self.faulted = true;
        self.mark_exit_reason(TradeExitReason::RiskExit);
        log::error!(
            "[{}] SLC trading disabled after order failure: {reason}",
            self.config.instrument_id,
        );
        if let Err(e) = self.market_exit() {
            log::error!(
                "[{}] Failed to flatten SLC exposure after order failure: {e:#}",
                self.config.instrument_id,
            );
        }
    }

    /// 按确认 Bar 收盘价生成最坏可成交限价，预留账户容量后提交一根 Bar 有效的入场单
    fn submit_signal(
        &mut self,
        signal: Signal,
        local_date: jiff::civil::Date,
    ) -> anyhow::Result<()> {
        let (stop, entry_limit) = entry_prices(
            signal,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.stop_buffer_ticks,
            self.config.max_entry_slippage_ticks,
        )?;
        let lot_size = self
            .instrument
            .lot_size()
            .unwrap_or_else(|| Quantity::from(1));
        let Some(quantity) = risk_sized_quantity(
            entry_limit,
            stop,
            self.config.risk_amount,
            self.config.max_order_quantity,
            self.config.max_order_notional,
            lot_size,
        )?
        else {
            self.record_risk_rejection(RiskRejectionReason::ZeroQuantity);
            log::warn!(
                "[{}] Skipping SLC signal: risk_rejection={}, level={}, entry_limit={}, stop={}, risk_budget={}, max_quantity={}, max_notional={}, lot_size={}",
                self.config.instrument_id,
                RiskRejectionReason::ZeroQuantity,
                signal.level,
                entry_limit,
                stop,
                self.config.risk_amount,
                self.config.max_order_quantity,
                self.config.max_order_notional,
                lot_size,
            );
            return Ok(());
        };
        let quantity_decimal = quantity.as_decimal();
        let reservation = RiskReservation {
            risk: (entry_limit.as_decimal() - stop.as_decimal()).abs() * quantity_decimal,
            notional: entry_limit.as_decimal() * quantity_decimal,
        };
        anyhow::ensure!(
            reservation.notional <= self.config.max_order_notional,
            "SLC quantity sizing exceeded configured order notional",
        );
        let risk_utilization = risk_utilization(reservation.risk, self.config.risk_amount);
        if risk_utilization < self.config.minimum_risk_utilization {
            self.record_risk_rejection(RiskRejectionReason::RiskUnderutilized);
            log::warn!(
                "[{}] Skipping SLC signal: risk_rejection={}, level={}, candidate_risk={}, risk_budget={}, risk_utilization={}, minimum_risk_utilization={}, candidate_notional={}, max_notional={}",
                self.config.instrument_id,
                RiskRejectionReason::RiskUnderutilized,
                signal.level,
                reservation.risk,
                self.config.risk_amount,
                risk_utilization.round_dp(4),
                self.config.minimum_risk_utilization,
                reservation.notional,
                self.config.max_order_notional,
            );
            return Ok(());
        }
        let symbol = self.config.instrument_id.symbol.to_string();
        let (outcome, snapshot) = self.account_risk.reserve_entry(
            &symbol,
            local_date,
            reservation,
            self.config.max_trades_per_day,
            self.config.account_risk_limits,
        )?;
        if let ReservationOutcome::Rejected(reason) = outcome {
            self.record_risk_rejection(reason);
            log::warn!(
                "[{}] Skipping SLC signal: risk_rejection={}, level={}, candidate_risk={}, candidate_notional={}, halted={}, daily_pnl={}, daily_loss_limit={}, open_risk={}, open_risk_limit={}, account_notional={}, account_notional_limit={}, open_positions={}, open_positions_limit={}, symbol_entries={}, symbol_trade_limit={}",
                self.config.instrument_id,
                reason,
                signal.level,
                reservation.risk,
                reservation.notional,
                snapshot.halted,
                snapshot.realized_pnl,
                self.config.account_risk_limits.daily_loss,
                snapshot.open_risk,
                self.config.account_risk_limits.open_risk,
                snapshot.account_notional,
                self.config.account_risk_limits.account_notional,
                snapshot.open_positions,
                self.config.account_risk_limits.open_positions,
                snapshot.entries_for_symbol,
                self.config.max_trades_per_day,
            );
            return Ok(());
        }

        let order = self.order().limit(
            self.config.instrument_id,
            signal.side,
            quantity,
            entry_limit,
            Some(TimeInForce::Day),
            None,
            Some(false),
            Some(false),
            Some(false),
            None,
            None,
            None,
            None,
            None,
            Some(vec![Ustr::from("SLC_ENTRY")]),
            None,
        );
        let client_order_id = order.client_order_id();
        self.pending_entry = Some(PendingEntry {
            client_order_id,
            side: signal.side,
            level: signal.level,
            level_age_bars: signal.level_age_bars,
            confirmation_bars: signal.confirmation_bars,
            confirmation_close_location: signal.confirmation_close_location,
            distance_atr: signal.distance_atr,
            zone_width_atr: signal.zone_width_atr,
            displacement_strength_atr: signal.displacement_strength_atr,
            entry_limit,
            stop,
            signal_ts: signal.ts_event,
            had_fill: false,
        });
        if let Err(e) = self.submit_order(order, None, None, None) {
            self.pending_entry = None;
            self.account_risk.release_unfilled(&symbol)?;
            return Err(e);
        }
        self.entries_submitted += 1;
        self.update_run_statistics(|statistics| statistics.entries_submitted += 1);
        log::info!(
            "[{}] Submitted SLC {} entry: level={}, confirmation=stochastic_reentry, level_age_bars={}, confirmation_bars={}, quantity={}, signal_close={}, entry_limit={}, stop={}, target={}R, reserved_risk={}, risk_utilization={}, reserved_notional={}",
            self.config.instrument_id,
            signal.side,
            signal.level,
            signal.level_age_bars,
            signal.confirmation_bars,
            quantity,
            signal.entry,
            entry_limit,
            stop,
            self.config.risk_reward,
            reservation.risk,
            risk_utilization.round_dp(4),
            reservation.notional,
        );
        Ok(())
    }

    /// 逐次保护每一笔入场成交；回测挂 OUO 止损/目标，实盘向 Longbridge 提交 MIT 止损
    ///
    /// 部分成交会累计成交数量和金额，实际平均价变化后重新计算整个头寸的目标。每个 fill 都按
    /// 相同区域止损单独保护，只有保护单成功提交后才更新 `protected_qty`。实盘目标不挂在券商，
    /// 而由报价触发；因此进程中断时仍有止损，但没有自动 2R 止盈。
    fn protect_entry_fill(&mut self, event: &OrderFilled) -> anyhow::Result<()> {
        let Some(pending) = self.pending_entry else {
            return Ok(());
        };
        if event.client_order_id != pending.client_order_id {
            return Ok(());
        }
        if let Some(active) = self.pending_entry.as_mut() {
            active.had_fill = true;
        }
        let exit_side = match pending.side {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
            OrderSide::NoOrderSide => anyhow::bail!("entry side is unspecified"),
        };
        anyhow::ensure!(
            match pending.side {
                OrderSide::Buy => event.last_px <= pending.entry_limit,
                OrderSide::Sell => event.last_px >= pending.entry_limit,
                OrderSide::NoOrderSide => false,
            },
            "entry fill exceeded the configured SLC slippage limit",
        );
        let previous_qty = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.filled_qty);
        let previous_notional = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.fill_notional);
        let previous_initial_risk = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.initial_risk);
        let exit_reason = self
            .active_trade
            .as_ref()
            .and_then(|active| active.exit_reason);
        let maximum_favorable_excursion = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.maximum_favorable_excursion);
        let maximum_adverse_excursion = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.maximum_adverse_excursion);
        let bars_held = self
            .active_trade
            .as_ref()
            .map_or(0, |active| active.bars_held);
        let fill_local = event
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let fill_minute =
            u16::try_from(fill_local.hour())? * 60 + u16::try_from(fill_local.minute())?;
        let entry_minute = self
            .active_trade
            .as_ref()
            .map_or(fill_minute, |active| active.entry_minute);
        let first_fill_ts = self.active_trade.as_ref().map_or(event.ts_event, |active| {
            active.first_fill_ts.min(event.ts_event)
        });
        let filled_qty = previous_qty + event.last_qty.as_decimal();
        let fill_notional =
            previous_notional + event.last_px.as_decimal() * event.last_qty.as_decimal();
        let initial_risk = previous_initial_risk
            + (event.last_px.as_decimal() - pending.stop.as_decimal()).abs()
                * event.last_qty.as_decimal();
        let average_fill = fill_notional / filled_qty;
        let target = target_price(
            pending.side,
            average_fill,
            pending.stop,
            self.instrument.price_increment(),
            self.instrument.price_precision(),
            self.config.risk_reward,
        )?;
        let is_backtest = self.backtest_four_hour_bars.is_some();
        let protective_target = if is_backtest {
            target_price(
                pending.side,
                event.last_px.as_decimal(),
                pending.stop,
                self.instrument.price_increment(),
                self.instrument.price_precision(),
                self.config.risk_reward,
            )?
        } else {
            target
        };
        match protective_stop_order_type(is_backtest) {
            OrderType::StopMarket => {
                let (order_list_id, stop_order_id, target_order_id) = {
                    let orders = self.order();
                    (
                        orders.generate_order_list_id(),
                        orders.generate_client_order_id(),
                        orders.generate_client_order_id(),
                    )
                };
                let protective_orders = backtest_protective_orders(
                    event,
                    exit_side,
                    pending.stop,
                    protective_target,
                    order_list_id,
                    stop_order_id,
                    target_order_id,
                )?;
                self.submit_order_list(protective_orders, event.position_id, None, None)?;
            }
            OrderType::MarketIfTouched => {
                let stop_order = self.order().market_if_touched(
                    self.config.instrument_id,
                    exit_side,
                    event.last_qty,
                    pending.stop,
                    None,
                    Some(TimeInForce::Day),
                    None,
                    Some(false),
                    Some(false),
                    None,
                    None,
                    None,
                    None,
                    Some(vec![Ustr::from("SLC_STOP")]),
                    None,
                );
                self.submit_order(stop_order, None, None, None)?;
            }
            _ => unreachable!("unsupported SLC protective stop order type"),
        }
        let protected_qty = self
            .active_trade
            .as_ref()
            .map_or(Decimal::ZERO, |active| active.protected_qty)
            + event.last_qty.as_decimal();
        anyhow::ensure!(
            protected_qty == filled_qty,
            "SLC protected quantity does not equal filled quantity",
        );
        self.active_trade = Some(ActiveTrade {
            side: pending.side,
            level: pending.level,
            level_age_bars: pending.level_age_bars,
            confirmation_bars: pending.confirmation_bars,
            confirmation_close_location: pending.confirmation_close_location,
            distance_atr: pending.distance_atr,
            zone_width_atr: pending.zone_width_atr,
            displacement_strength_atr: pending.displacement_strength_atr,
            entry_minute,
            entry_limit: pending.entry_limit,
            stop: pending.stop,
            target,
            first_fill_ts,
            filled_qty,
            protected_qty,
            fill_notional,
            initial_risk,
            maximum_favorable_excursion,
            maximum_adverse_excursion,
            bars_held,
            exit_reason,
        });

        let entry_closed = self
            .cache()
            .order(&pending.client_order_id)
            .is_some_and(|order| order.is_closed());
        if entry_closed {
            self.pending_entry = None;
        }
        log::info!(
            "[{}] Protected SLC entry fill: last_quantity={}, total_quantity={}, average_fill={}, stop={}, target={}, target_execution={}",
            self.config.instrument_id,
            event.last_qty,
            filled_qty,
            average_fill,
            pending.stop,
            protective_target,
            if is_backtest {
                "resting OUO limit"
            } else {
                "executable quote trigger"
            },
        );
        if self.exit_pending {
            self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        }
        Ok(())
    }

    /// 下一根已完成 5 分钟 Bar 到达后取消尚未成交的 entry remainder，防止追逐过期信号
    fn cancel_stale_entry(&mut self, bar: Bar) -> anyhow::Result<()> {
        let Some(pending) = self.pending_entry else {
            return Ok(());
        };
        if bar.ts_event <= pending.signal_ts {
            return Ok(());
        }
        let cancelable = self
            .cache()
            .order(&pending.client_order_id)
            .is_some_and(|order| order.is_open() || order.is_inflight());
        if cancelable {
            log::warn!(
                "[{}] Canceling stale SLC entry {} after one completed bar",
                self.config.instrument_id,
                pending.client_order_id,
            );
            self.cancel_order(pending.client_order_id, None, None)?;
        }
        Ok(())
    }

    /// 判断已完成实盘 Bar 是否触及目标，作为实时一档报价触发可能遗漏时的兜底
    fn target_reached(&self, bar: Bar) -> bool {
        !self.exit_pending
            && self.active_trade.as_ref().is_some_and(|active| {
                bar_reaches_target(active.side, active.target, active.first_fill_ts, bar)
            })
    }

    /// 识别 OUO 组合一侧成交后另一侧被正常取消，避免把预期联动误报为保护失效
    fn expected_protective_cancel(&self, client_order_id: ClientOrderId) -> bool {
        let Some(order) = self.cache().order(&client_order_id) else {
            return false;
        };
        order.contingency_type() == Some(ContingencyType::Ouo)
            && order.linked_order_ids().is_some_and(|linked_order_ids| {
                linked_order_ids.iter().any(|linked_order_id| {
                    self.cache()
                        .order(linked_order_id)
                        .is_some_and(|linked_order| !linked_order.filled_qty().is_zero())
                })
            })
    }

    /// 清理已终结入场；仅在从未产生成交敞口时同时回退交易次数和全部风险预留
    fn clear_terminal_entry(&mut self, client_order_id: ClientOrderId) -> anyhow::Result<bool> {
        let Some(pending) = self
            .pending_entry
            .filter(|pending| pending.client_order_id == client_order_id)
        else {
            return Ok(false);
        };
        self.pending_entry = None;
        if self.active_trade.is_none() && !self.has_open_position() {
            if pending.had_fill {
                self.account_risk
                    .release_reservation(self.config.instrument_id.symbol.as_str())?;
            } else {
                self.account_risk
                    .release_unfilled(self.config.instrument_id.symbol.as_str())?;
            }
        }
        Ok(true)
    }

    /// 先取消当前 symbol 的入场与保护单，再进入等待确认后市价平仓的退出状态
    fn request_exit(&mut self, reason: TradeExitReason, detail: &str) -> anyhow::Result<()> {
        self.mark_exit_reason(reason);
        self.exit_pending = true;
        log::info!(
            "[{}] Requesting SLC position exit: exit_reason={reason}, detail={detail}",
            self.config.instrument_id,
        );
        if self.has_open_orders() {
            self.cancel_all_orders(self.config.instrument_id, None, None, None)?;
        } else {
            self.finish_exit()?;
        }
        Ok(())
    }

    /// 仅在所有旧订单确认关闭后提交市价平仓，避免遗留保护单在平仓后反向开仓
    fn finish_exit(&mut self) -> anyhow::Result<()> {
        if !self.exit_pending || self.has_open_orders() {
            return Ok(());
        }
        if !self.has_open_position() {
            if let Some(pending) = self.pending_entry {
                self.clear_terminal_entry(pending.client_order_id)?;
            }
            self.exit_pending = false;
            return Ok(());
        }
        log::info!(
            "[{}] All SLC protective orders canceled; submitting market exit",
            self.config.instrument_id,
        );
        self.close_positions()
    }
}

impl Debug for SlcStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(SlcStrategy))
            .field("config", &self.config)
            .field("session_disabled", &self.session_disabled)
            .field("faulted", &self.faulted)
            .finish_non_exhaustive()
    }
}

nautilus_strategy!(SlcStrategy, {
    // 在处理后续生命周期事件前优先为入场成交建立保护
    fn on_order_filled(&mut self, event: &OrderFilled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        self.record_exit_fill(event);
        if let Err(e) = self.protect_entry_fill(event) {
            self.disable_after_order_failure(&format!(
                "failed to protect entry fill {}: {e:#}",
                event.client_order_id,
            ));
        }
    }

    // 券商拒单后释放未成交入场风险，并停止该策略本次运行的新交易
    fn on_order_rejected(&mut self, event: OrderRejected) {
        if event.instrument_id == self.config.instrument_id {
            if let Err(e) = self.clear_terminal_entry(event.client_order_id) {
                log::error!(
                    "[{}] Failed to release rejected SLC entry risk: {e:#}",
                    self.config.instrument_id,
                );
            }
            self.disable_after_order_failure(&event.reason);
        }
    }

    // 将本地 RiskEngine 拒单视为终止性策略故障，避免配置错误后继续尝试
    fn on_order_denied(&mut self, event: OrderDenied) {
        if event.instrument_id == self.config.instrument_id {
            if let Err(e) = self.clear_terminal_entry(event.client_order_id) {
                log::error!(
                    "[{}] Failed to release denied SLC entry risk: {e:#}",
                    self.config.instrument_id,
                );
            }
            self.disable_after_order_failure(&event.reason);
        }
    }

    // 区分正常到期的入场余量与危险的保护单到期
    fn on_order_expired(&mut self, event: OrderExpired) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        match self.clear_terminal_entry(event.client_order_id) {
            Ok(true) => log::warn!(
                "[{}] SLC entry expired before its remaining quantity filled",
                self.config.instrument_id,
            ),
            Ok(false) if self.has_exposure() => {
                self.disable_after_order_failure(
                    "a protective SLC order expired while exposure remained",
                );
            }
            Ok(false) => {}
            Err(e) => self.disable_after_order_failure(&format!(
                "failed to release expired SLC entry risk: {e:#}",
            )),
        }
    }

    // 推进先撤单后平仓状态机，同时容忍预期的过期 entry 或 OUO 联动取消
    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        if self.exit_pending {
            if let Err(e) = self.finish_exit() {
                self.disable_after_order_failure(&format!("failed to submit SLC exit: {e:#}"));
            }
        } else if self
            .pending_entry
            .is_some_and(|pending| pending.client_order_id == event.client_order_id)
        {
            if let Err(e) = self.clear_terminal_entry(event.client_order_id) {
                self.disable_after_order_failure(&format!(
                    "failed to release canceled SLC entry risk: {e:#}",
                ));
            } else {
                log::info!(
                    "[{}] SLC entry remainder canceled; filled exposure remains protected={}",
                    self.config.instrument_id,
                    self.active_trade.is_some(),
                );
            }
        } else if self.expected_protective_cancel(event.client_order_id) {
            log::info!(
                "[{}] SLC protective OUO sibling canceled after its paired exit filled: {}",
                self.config.instrument_id,
                event.client_order_id,
            );
        } else if !self.is_exiting() && self.has_open_position() {
            self.disable_after_order_failure("a protective SLC order was canceled unexpectedly");
        }
    }

    // 撤单被拒后升级到框架管理式退出和 reconciliation 恢复路径
    fn on_order_cancel_rejected(&mut self, event: OrderCancelRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.faulted = true;
            self.exit_pending = false;
            self.mark_exit_reason(TradeExitReason::RiskExit);
            log::error!(
                "[{}] SLC order cancellation was rejected; switching to managed market-exit recovery: {}",
                self.config.instrument_id,
                event.reason,
            );
            if !self.is_exiting()
                && let Err(e) = self.market_exit()
            {
                log::error!(
                    "[{}] Failed to start managed SLC exit after cancel rejection: {e:#}",
                    self.config.instrument_id,
                );
            }
        }
    }

    // 持久化账户 PnL；若 entry remainder 仍可能成交则继续保留风险 reservation
    fn on_position_closed(&mut self, event: PositionClosed) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        let entry_remainder_open = self.pending_entry.is_some_and(|pending| {
            self.cache()
                .order(&pending.client_order_id)
                .is_some_and(|order| order.is_open() || order.is_inflight())
        });
        if !entry_remainder_open {
            self.pending_entry = None;
        }
        let active_trade = self.active_trade.take();
        self.exit_pending = entry_remainder_open;
        if let Err(e) = self.cancel_all_orders(self.config.instrument_id, None, None, None) {
            log::error!(
                "[{}] Failed to cancel residual SLC orders after position close: {e:#}",
                self.config.instrument_id,
            );
            self.faulted = true;
        }
        let local_date = event
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone())
            .date();
        let realized_pnl = event.realized_pnl.map(|pnl| pnl.as_decimal());
        let mut exit_reason = TradeExitReason::Unknown;
        let mut level = None;
        let mut initial_risk = None;
        let mut risk_utilization = None;
        let mut r_multiple = None;
        let mut holding_bars = None;
        let mut mfe_r = None;
        let mut mae_r = None;
        let mut estimated_cost = None;
        let mut entry_slippage_stress = None;
        if let Some(active) = active_trade {
            exit_reason = active.exit_reason.unwrap_or(TradeExitReason::Unknown);
            level = Some(active.level);
            initial_risk = Some(active.initial_risk);
            let utilization = active.initial_risk / self.config.risk_amount;
            let trade_mfe_r = active.mfe_r();
            let trade_mae_r = active.mae_r();
            let trade_estimated_cost = active.filled_qty * self.config.round_trip_cost_per_share;
            let trade_entry_slippage_stress = if self.backtest_four_hour_bars.is_some() {
                (active.entry_limit.as_decimal() - active.average_fill()).abs() * active.filled_qty
            } else {
                Decimal::ZERO
            };
            risk_utilization = Some(utilization);
            holding_bars = Some(active.bars_held);
            mfe_r = Some(trade_mfe_r);
            mae_r = Some(trade_mae_r);
            estimated_cost = Some(trade_estimated_cost);
            entry_slippage_stress = Some(trade_entry_slippage_stress);
            r_multiple = if active.initial_risk > Decimal::ZERO {
                realized_pnl.map(|pnl| pnl / active.initial_risk)
            } else {
                None
            };
            let close_ts = event.ts_closed.unwrap_or(event.ts_event);
            self.update_run_statistics(|statistics| {
                statistics.trades.push(ClosedTradeStatistics {
                    side: active.side,
                    level: active.level,
                    level_age_bars: active.level_age_bars,
                    confirmation_bars: active.confirmation_bars,
                    confirmation_close_location: active.confirmation_close_location,
                    distance_atr: active.distance_atr,
                    zone_width_atr: active.zone_width_atr,
                    displacement_strength_atr: active.displacement_strength_atr,
                    entry_minute: active.entry_minute,
                    holding_bars: active.bars_held,
                    mfe_r: trade_mfe_r,
                    mae_r: trade_mae_r,
                    exit_reason,
                    realized_pnl,
                    estimated_cost: trade_estimated_cost,
                    entry_slippage_stress: trade_entry_slippage_stress,
                    initial_risk: active.initial_risk,
                    risk_utilization: utilization,
                    r_multiple,
                    close_ts,
                    ambiguous_exit_bar: false,
                });
            });
            if self.backtest_four_hour_bars.is_some()
                && matches!(exit_reason, TradeExitReason::Target | TradeExitReason::Stop)
            {
                let probe = AmbiguityProbe {
                    stop: active.stop,
                    target: active.target,
                    close_ts,
                };
                if let Some(bar) = self
                    .last_five_minute_bar
                    .filter(|bar| bar.ts_init == close_ts)
                {
                    if bar_reaches_stop_and_target(probe.stop, probe.target, bar) {
                        self.mark_ambiguous_exit_bar(probe, bar);
                    }
                } else {
                    self.ambiguity_probe = Some(probe);
                }
            }
        }
        match self.account_risk.record_close(
            self.config.instrument_id.symbol.as_str(),
            local_date,
            realized_pnl,
            !entry_remainder_open,
        ) {
            Ok(snapshot) => {
                self.session_disabled |= snapshot.halted;
                log::info!(
                    "[{}] SLC position closed: exit_reason={}, level={}, realized_pnl={:?}, estimated_cost={}, entry_slippage_stress={}, initial_risk={}, risk_utilization={}, actual_r={}, holding_bars={}, mfe_r={}, mae_r={}, account_halted={}, account_daily_pnl={}, open_risk={}, account_notional={}, open_positions={}",
                    self.config.instrument_id,
                    exit_reason,
                    level.map_or_else(|| "unknown".to_string(), |level| level.to_string()),
                    realized_pnl,
                    estimated_cost.map_or_else(|| "n/a".to_string(), |cost| cost.to_string()),
                    entry_slippage_stress
                        .map_or_else(|| "n/a".to_string(), |cost| cost.to_string()),
                    initial_risk.map_or_else(|| "n/a".to_string(), |risk| risk.to_string()),
                    risk_utilization
                        .map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    r_multiple
                        .map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    holding_bars.map_or_else(|| "n/a".to_string(), |bars| bars.to_string()),
                    mfe_r.map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    mae_r.map_or_else(|| "n/a".to_string(), |value| value.round_dp(4).to_string()),
                    snapshot.halted,
                    snapshot.realized_pnl,
                    snapshot.open_risk,
                    snapshot.account_notional,
                    snapshot.open_positions,
                );
            }
            Err(e) => {
                self.faulted = true;
                log::error!(
                    "[{}] Failed to persist SLC account risk after close: {e:#}",
                    self.config.instrument_id,
                );
            }
        }
    }
});

impl DataActor for SlcStrategy {
    /// 启动时校验 Bar 合约和 instrument，恢复共享风险状态并建立行情订阅
    ///
    /// 回测只订阅 5 分钟数据，4 小时历史由回放时钟显式推进。实盘同时订阅 5 分钟、4 小时和
    /// Quote；若 reconciliation 发现已有敞口，则先禁止新信号并执行平仓，而不是猜测丢失的
    /// active trade、止损和目标状态。
    fn on_start(&mut self) -> anyhow::Result<()> {
        validate_bar_type(self.config.five_minute_bar_type, 5, BarAggregation::Minute)?;
        validate_bar_type(self.config.four_hour_bar_type, 4, BarAggregation::Hour)?;
        anyhow::ensure!(
            self.config.entry_start_minute < self.config.entry_end_minute,
            "effective entry window ends before it starts on this trading day",
        );
        self.cache().try_instrument(&self.config.instrument_id)?;
        self.subscribe_bars(self.config.five_minute_bar_type, None, None);
        if self.backtest_four_hour_bars.is_some() {
            log::info!(
                "[{}] SLC backtest active: direction={}, 5m={}, historical_4h_bars={}, entry_window={:02}:{:02}-{:02}:{:02}, flatten={:02}:{:02}, target={}R, minimum_risk_utilization={}, estimated_round_trip_cost_per_share={}",
                self.config.instrument_id,
                self.signals.rules.trade_direction,
                self.config.five_minute_bar_type,
                self.backtest_four_hour_bars
                    .as_ref()
                    .map_or(0, VecDeque::len),
                self.config.entry_start_minute / 60,
                self.config.entry_start_minute % 60,
                self.config.entry_end_minute / 60,
                self.config.entry_end_minute % 60,
                self.config.flatten_minute / 60,
                self.config.flatten_minute % 60,
                self.config.risk_reward,
                self.config.minimum_risk_utilization,
                self.config.round_trip_cost_per_share,
            );
            return Ok(());
        }
        let local_date = Timestamp::now()
            .to_zoned(self.config.timezone.clone())
            .date();
        let has_exposure = self.has_exposure();
        let snapshot = self.account_risk.reconcile_symbol(
            self.config.instrument_id.symbol.as_str(),
            local_date,
            has_exposure,
        )?;
        self.current_date = Some(local_date);
        self.session_disabled = snapshot.halted;
        self.subscribe_bars(self.config.four_hour_bar_type, None, None);
        self.subscribe_quotes(self.config.instrument_id, None, None);
        log::info!(
            "[{}] SLC subscriptions active: direction={}, quotes=true, 5m={}, 4h={}, entry_window={:02}:{:02}-{:02}:{:02}, flatten={:02}:{:02}, target={}R, minimum_risk_utilization={}, account_halted={}, account_daily_pnl={}, open_risk={}, account_notional={}, open_positions={}, symbol_entries={}",
            self.config.instrument_id,
            self.signals.rules.trade_direction,
            self.config.five_minute_bar_type,
            self.config.four_hour_bar_type,
            self.config.entry_start_minute / 60,
            self.config.entry_start_minute % 60,
            self.config.entry_end_minute / 60,
            self.config.entry_end_minute % 60,
            self.config.flatten_minute / 60,
            self.config.flatten_minute % 60,
            self.config.risk_reward,
            self.config.minimum_risk_utilization,
            snapshot.halted,
            snapshot.realized_pnl,
            snapshot.open_risk,
            snapshot.account_notional,
            snapshot.open_positions,
            snapshot.entries_for_symbol,
        );
        if has_exposure {
            log::warn!(
                "[{}] Flattening reconciled SLC exposure before accepting new signals",
                self.config.instrument_id,
            );
            self.request_exit(TradeExitReason::RiskExit, "reconciled startup exposure")?;
        }
        Ok(())
    }

    /// 管理式停止完成订单与持仓对账后取消行情订阅，并输出最终信号漏斗
    fn on_stop(&mut self) -> anyhow::Result<()> {
        log::info!(
            "[{}] SLC signal funnel: 5m_bars={}, directional_4h_bars={}, zones_created={}, level_touches={}, stochastic_extremes={}, stochastic_reentries={}, signals={}, risk_rejections={}, entries_submitted={}",
            self.config.instrument_id,
            self.signals.funnel.five_minute_bars,
            self.signals.funnel.directional_bars,
            self.signals.funnel.zones_created,
            self.signals.funnel.level_touches,
            self.signals.funnel.stochastic_extremes,
            self.signals.funnel.stochastic_reentries,
            self.signals.funnel.signals,
            self.risk_rejections,
            self.entries_submitted,
        );
        self.unsubscribe_bars(self.config.five_minute_bar_type, None, None);
        if self.backtest_four_hour_bars.is_none() {
            self.unsubscribe_bars(self.config.four_hour_bar_type, None, None);
            self.unsubscribe_quotes(self.config.instrument_id, None, None);
        }
        Ok(())
    }

    /// 当可立即成交的一档报价达到实际成交价计算的固定 R 目标时启动退出
    ///
    /// 多头使用 bid、空头使用 ask，避免用不可成交的报价另一侧虚假触发盈利目标。该路径依赖
    /// 本地进程和实时 Quote；完成 Bar 检测只承担兜底职责，不能消除撤单与市价成交之间的延迟。
    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        if quote.instrument_id != self.config.instrument_id
            || self.faulted
            || self.exit_pending
            || self.is_exiting()
        {
            return Ok(());
        }
        let (side, target) = {
            let Some(active) = self.active_trade.as_mut() else {
                return Ok(());
            };
            let executable_price = match active.side {
                OrderSide::Buy => quote.bid_price,
                OrderSide::Sell => quote.ask_price,
                OrderSide::NoOrderSide => return Ok(()),
            };
            active.observe_price(executable_price);
            (active.side, active.target)
        };
        if !quote_reaches_target(side, target, quote) {
            return Ok(());
        }
        log::info!(
            "[{}] Realtime SLC target reached: side={}, target={}, bid={}, ask={}, ts_event={}",
            self.config.instrument_id,
            side,
            target,
            quote.bid_price,
            quote.ask_price,
            quote.ts_event,
        );
        self.request_exit(
            TradeExitReason::Target,
            "executable top-of-book quote reached the actual-fill-based SLC target",
        )
    }

    /// 按结构、数据完整性、时段、风险和执行顺序处理已完成 Bar
    ///
    /// 4 小时 Bar 只更新结构。5 分钟 Bar 先检查跨日与断档，再更新持仓 MFE/MAE、计算信号、
    /// 执行收盘前退出、取消过期入场、检查实盘目标兜底，最后才允许新订单进入风险
    /// 预留流程。任一故障、会话禁用、已有敞口或退出未完成都会阻断新入场。
    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if bar.bar_type == self.config.four_hour_bar_type {
            let Some(finalized) = self.signals.finalize_four_hour(*bar) else {
                return Ok(());
            };
            self.process_finalized_four_hour_bar(finalized)?;
            return Ok(());
        }
        if bar.bar_type != self.config.five_minute_bar_type {
            return Ok(());
        }
        let finalized = if self.backtest_four_hour_bars.is_some() {
            self.advance_backtest_four_hour_bars(bar.ts_init)?;
            *bar
        } else {
            let Some(finalized) = self.signals.finalize_five_minute(*bar) else {
                return Ok(());
            };
            finalized
        };
        self.last_five_minute_bar = Some(finalized);
        self.inspect_ambiguous_exit_bar(finalized);
        let suppress_signal = self.suppress_warmup_boundary_signal;
        self.suppress_warmup_boundary_signal = false;
        let local = finalized
            .ts_event
            .to_datetime_utc()
            .to_zoned(self.config.timezone.clone());
        let minute = u16::try_from(local.hour())? * 60 + u16::try_from(local.minute())?;
        let close_minute = minute.saturating_add(FIVE_MINUTES);
        if !(RTH_OPEN_MINUTE..RTH_CLOSE_MINUTE).contains(&minute) {
            return Ok(());
        }
        let new_date = self.current_date != Some(local.date());
        if new_date {
            self.current_date = Some(local.date());
            self.last_five_minute_bar_start = None;
            self.session_disabled = false;
            let has_exposure = self.has_exposure();
            let snapshot = self.account_risk.reconcile_symbol(
                self.config.instrument_id.symbol.as_str(),
                local.date(),
                has_exposure,
            )?;
            self.session_disabled = snapshot.halted;
            if has_exposure {
                self.request_exit(TradeExitReason::RiskExit, "unexpected overnight exposure")?;
            }
        }
        if let Some(previous) = self.last_five_minute_bar_start
            && has_five_minute_gap(previous, finalized.ts_event)
        {
            self.session_disabled = true;
            log::error!(
                "[{}] SLC 5m data gap detected: previous={}, current={}; disabling entries for the session",
                self.config.instrument_id,
                previous,
                finalized.ts_event,
            );
            if self.has_exposure() {
                self.request_exit(TradeExitReason::RiskExit, "five-minute market data gap")?;
            }
        }
        self.last_five_minute_bar_start = Some(finalized.ts_event);
        if let Some(active) = self
            .active_trade
            .as_mut()
            .filter(|active| finalized.ts_event >= active.first_fill_ts)
        {
            active.observe_bar(finalized);
        }
        let within_entry_window = close_minute >= self.config.entry_start_minute
            && close_minute <= self.config.entry_end_minute;
        let allow_signal = within_entry_window
            && !suppress_signal
            && !self.faulted
            && !self.session_disabled
            && !self.exit_pending
            && !self.has_exposure();
        let signal = self.signals.process_five_minute(finalized, allow_signal);
        let funnel = self.signals.funnel;
        self.update_run_statistics(|statistics| statistics.funnel = funnel);
        if self.config.log_bars {
            log::info!(
                "[{}] 5m bar collected: start={}, open={}, high={}, low={}, close={}, volume={}, 4h_trend={:?}, atr={:.6}, stochastic_k={:.2}, demand_zones={}, supply_zones={}, indicators_initialized={}, structure_initialized={}, session_disabled={}",
                self.config.instrument_id,
                local,
                finalized.open,
                finalized.high,
                finalized.low,
                finalized.close,
                finalized.volume,
                self.signals.structure.trend(),
                self.signals.atr.value,
                self.signals.stochastics.value_k,
                self.signals.demand.len(),
                self.signals.supply.len(),
                self.signals.indicators_initialized(),
                self.signals.structure.initialized(),
                self.session_disabled,
            );
        }
        if close_minute >= self.config.flatten_minute {
            self.session_disabled = true;
            if should_request_preclose_exit(
                close_minute,
                self.config.flatten_minute,
                self.has_exposure(),
                self.exit_pending,
            ) {
                self.request_exit(TradeExitReason::PreClose, "pre-close risk cutoff")?;
            }
            return Ok(());
        }
        self.cancel_stale_entry(finalized)?;
        if self.backtest_four_hour_bars.is_none() && self.target_reached(finalized) {
            self.request_exit(
                TradeExitReason::Target,
                "five-minute bar traded through the actual-fill-based SLC target",
            )?;
            return Ok(());
        }
        if self.faulted
            || self.exit_pending
            || self.session_disabled
            || !within_entry_window
            || self.has_exposure()
        {
            return Ok(());
        }
        if let Some(signal) = signal {
            self.submit_signal(signal, local.date())?;
        }
        Ok(())
    }
}

/// 校验 SLC 状态机要求的精确 external LAST Bar 合约，防止错误周期或内部聚合混入
fn validate_bar_type(
    bar_type: BarType,
    step: usize,
    aggregation: BarAggregation,
) -> anyhow::Result<()> {
    let spec = bar_type.spec();
    anyhow::ensure!(
        spec.step.get() == step
            && spec.aggregation == aggregation
            && spec.price_type == PriceType::Last
            && bar_type.aggregation_source() == AggregationSource::External,
        "invalid SLC bar type {bar_type}",
    );
    Ok(())
}

/// 根据 zone 边界、tick buffer 和最大允许滑点返回止损价及可成交入场限价
fn entry_prices(
    signal: Signal,
    increment: Price,
    precision: u8,
    stop_buffer_ticks: u64,
    max_entry_slippage_ticks: u64,
) -> anyhow::Result<(Price, Price)> {
    let increment = increment.as_decimal();
    let stop_buffer = increment * Decimal::from(stop_buffer_ticks);
    let entry_buffer = increment * Decimal::from(max_entry_slippage_ticks);
    let (stop, entry_limit) = match signal.side {
        OrderSide::Buy => {
            let stop = signal.zone_low.as_decimal() - stop_buffer;
            let entry_limit = signal.entry.as_decimal() + entry_buffer;
            anyhow::ensure!(stop < entry_limit, "long stop must be below entry limit");
            (stop, entry_limit)
        }
        OrderSide::Sell => {
            let stop = signal.zone_high.as_decimal() + stop_buffer;
            let entry_limit = signal.entry.as_decimal() - entry_buffer;
            anyhow::ensure!(stop > entry_limit, "short stop must be above entry limit");
            (stop, entry_limit)
        }
        OrderSide::NoOrderSide => anyhow::bail!("signal side is unspecified"),
    };
    anyhow::ensure!(stop > Decimal::ZERO, "stop price must be positive");
    anyhow::ensure!(entry_limit > Decimal::ZERO, "entry limit must be positive");
    Ok((
        Price::from_decimal_dp(stop, precision)?,
        Price::from_decimal_dp(entry_limit, precision)?,
    ))
}

/// 从实际成交价和固定止损计算目标，并向有利方向取整到合法 tick，确保不少于目标 R 倍数
fn target_price(
    side: OrderSide,
    entry: Decimal,
    stop: Price,
    increment: Price,
    precision: u8,
    risk_reward: Decimal,
) -> anyhow::Result<Price> {
    let stop = stop.as_decimal();
    let increment = increment.as_decimal();
    let target = match side {
        OrderSide::Buy => {
            let risk = entry - stop;
            anyhow::ensure!(risk > Decimal::ZERO, "long fill must be above stop");
            ((entry + risk * risk_reward) / increment).ceil() * increment
        }
        OrderSide::Sell => {
            let risk = stop - entry;
            anyhow::ensure!(risk > Decimal::ZERO, "short fill must be below stop");
            ((entry - risk * risk_reward) / increment).floor() * increment
        }
        OrderSide::NoOrderSide => anyhow::bail!("entry side is unspecified"),
    };
    anyhow::ensure!(target > Decimal::ZERO, "target price must be positive");
    Ok(Price::from_decimal_dp(target, precision)?)
}

/// 使用可立即成交的报价侧判断目标：多头看 bid，空头看 ask
fn quote_reaches_target(side: OrderSide, target: Price, quote: &QuoteTick) -> bool {
    match side {
        OrderSide::Buy => quote.bid_price >= target,
        OrderSide::Sell => quote.ask_price <= target,
        OrderSide::NoOrderSide => false,
    }
}

/// 判断不早于首次成交的已完成 Bar 是否触及目标，避免入场前价格造成错误退出
fn bar_reaches_target(side: OrderSide, target: Price, first_fill_ts: UnixNanos, bar: Bar) -> bool {
    bar.ts_event >= first_fill_ts
        && match side {
            OrderSide::Buy => bar.high >= target,
            OrderSide::Sell => bar.low <= target,
            OrderSide::NoOrderSide => false,
        }
}

/// 返回同时满足风险、最大数量和最坏入场价名义上限的最大整手数量
///
/// 先按每股风险得到理论数量，再依次应用数量与名义金额上限，最后向下取整到 lot size。
/// 任一条件使结果小于一手时返回 None，不通过增加杠杆或四舍五入突破风险预算。
fn risk_sized_quantity(
    entry: Price,
    stop: Price,
    risk_amount: Decimal,
    max_quantity: Quantity,
    max_notional: Decimal,
    lot_size: Quantity,
) -> anyhow::Result<Option<Quantity>> {
    anyhow::ensure!(entry.as_decimal() > Decimal::ZERO, "entry must be positive");
    let risk_per_share = (entry.as_decimal() - stop.as_decimal()).abs();
    anyhow::ensure!(
        risk_per_share > Decimal::ZERO,
        "risk per share must be positive"
    );
    let lot = lot_size.as_decimal();
    anyhow::ensure!(lot > Decimal::ZERO, "instrument lot size must be positive");
    anyhow::ensure!(
        max_notional > Decimal::ZERO,
        "maximum notional must be positive"
    );
    let risk_quantity = ((risk_amount / risk_per_share) / lot).floor() * lot;
    let notional_quantity = ((max_notional / entry.as_decimal()) / lot).floor() * lot;
    let capped = risk_quantity
        .min(max_quantity.as_decimal())
        .min(notional_quantity);
    let normalized = (capped / lot).floor() * lot;
    if normalized <= Decimal::ZERO {
        return Ok(None);
    }
    Ok(Some(Quantity::from_decimal(normalized)?))
}

/// 返回订单预留风险占目标单笔风险的精确比例，用于识别资金约束导致的风险不足
fn risk_utilization(reserved_risk: Decimal, risk_amount: Decimal) -> Decimal {
    reserved_risk / risk_amount
}

/// 实盘启动前已完整获取并验证的单标的数据与当日交易时段
struct PreparedInputs {
    instrument_id: InstrumentId,
    instrument: InstrumentAny,
    five_minute_bars: Vec<Bar>,
    four_hour_bars: Vec<Bar>,
    market_close: Timestamp,
    market_close_minute: u16,
}

/// 为每个配置 symbol 一次性加载 Longbridge 静态信息，并构造精确 Instrument 定义
async fn load_instruments(
    config: &AppConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<(InstrumentId, InstrumentAny)>> {
    let symbols = config
        .instruments
        .iter()
        .map(|instrument| instrument.instrument_id.symbol.as_str())
        .collect::<Vec<_>>();
    let started = Instant::now();
    println!(
        "SLC data download: requesting static security info for {} symbols",
        symbols.len(),
    );
    let static_info = quote_api_call_with_retry(|| context.static_info(symbols.clone()))
        .await
        .context("failed to request Longbridge static security info")?;
    println!(
        "SLC data download: static security info ready, symbols={}, elapsed={:.1}s",
        static_info.len(),
        started.elapsed().as_secs_f64(),
    );
    let mut static_info_by_symbol = static_info
        .into_iter()
        .map(|info| (info.symbol.clone(), info))
        .collect::<HashMap<_, _>>();
    let mut instruments = Vec::with_capacity(config.instruments.len());
    for configured in &config.instruments {
        let instrument_id = configured.instrument_id;
        let symbol = instrument_id.symbol.as_str();
        let static_security_info = static_info_by_symbol.remove(symbol).with_context(|| {
            format!("Longbridge did not return exact static security info for {symbol}")
        })?;
        let instrument = parse_instrument(
            &static_security_info,
            configured.price_increment,
            UnixNanos::default(),
        )?;
        anyhow::ensure!(
            instrument.quote_currency() == Currency::USD(),
            "SLC risk controls currently require a USD-quoted equity: {instrument_id}",
        );
        instruments.push((instrument_id, instrument));
    }
    Ok(instruments)
}

/// 实盘节点启动前加载全部 instrument、完整 5 分钟/4 小时 warmup 及当日收盘时间
async fn prepare_inputs(
    config: &AppConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<PreparedInputs>> {
    let (market_close, market_close_minute) = current_us_market_close(context).await?;
    let mut prepared = Vec::with_capacity(config.instruments.len());

    for (instrument_id, instrument) in load_instruments(config, context).await? {
        let symbol = instrument_id.symbol.as_str();
        let five_minute_bars = load_warmup_bars(
            context,
            symbol,
            Period::FiveMinute,
            AppConfig::five_minute_bar_type(instrument_id),
            config.five_minute_warmup,
            instrument.price_precision(),
        )
        .await?;
        let four_hour_bars = load_warmup_bars(
            context,
            symbol,
            Period::FourHour,
            AppConfig::four_hour_bar_type(instrument_id),
            config.four_hour_warmup,
            instrument.price_precision(),
        )
        .await?;

        prepared.push(PreparedInputs {
            instrument_id,
            instrument,
            five_minute_bars,
            four_hour_bars,
            market_close,
            market_close_minute,
        });
    }

    Ok(prepared)
}

/// 加载指定周期最近、完整、去重的常规时段 warmup，并排除仍在形成的末根 K 线
async fn load_warmup_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    count: usize,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let request_count = count
        .checked_add(1)
        .filter(|request_count| *request_count <= MAX_WARMUP_BARS)
        .context("warmup count must leave room for one in-progress Longbridge bar")?;
    let candlesticks = quote_api_call_with_retry(|| {
        context.candlesticks(
            symbol,
            period,
            request_count,
            AdjustType::NoAdjust,
            TradeSessions::Intraday,
        )
    })
    .await
    .with_context(|| format!("failed to request {period:?} warmup bars for {symbol}"))?;
    let bars = parse_warmup_bars(
        symbol,
        period,
        bar_type,
        candlesticks,
        count,
        OffsetDateTime::now_utc(),
        price_precision,
    )?;
    let latest = bars
        .last()
        .expect("warmup count was validated above")
        .ts_event;
    let now = UnixNanos::from(Timestamp::now());
    anyhow::ensure!(
        latest <= now && now.as_u64().saturating_sub(latest.as_u64()) <= MAX_WARMUP_AGE_NANOS,
        "Longbridge returned stale or future {period:?} warmup data for {symbol}: latest={latest}",
    );
    Ok(bars)
}

/// 从 warmup 响应删除未完成 K 线，再解析、排序、去重并严格校验请求数量
fn parse_warmup_bars(
    symbol: &str,
    period: Period,
    bar_type: BarType,
    candlesticks: Vec<Candlestick>,
    count: usize,
    now: OffsetDateTime,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let bar_duration = match period {
        Period::FiveMinute => time::Duration::minutes(5),
        Period::FourHour => time::Duration::hours(4),
        _ => anyhow::bail!("unsupported SLC warmup period: {period:?}"),
    };
    let mut bars = candlesticks
        .into_iter()
        .filter(|candlestick| {
            candlestick
                .timestamp
                .checked_add(bar_duration)
                .is_some_and(|closed_at| closed_at <= now)
        })
        .map(|candlestick| {
            parse_bar_with_price_precision(
                bar_type,
                candlestick,
                UnixNanos::default(),
                price_precision,
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    anyhow::ensure!(
        bars.len() >= count,
        "Longbridge returned {} of {count} required {period:?} warmup bars for {symbol}",
        bars.len(),
    );
    if bars.len() > count {
        bars = bars.split_off(bars.len() - count);
    }
    Ok(bars)
}

/// 回测单标的的 warmup 与样本期数据；克隆仅用于参数候选复用同一份输入
#[derive(Clone)]
struct PreparedBacktestInputs {
    instrument_id: InstrumentId,
    instrument: InstrumentAny,
    five_minute_warmup: Vec<Bar>,
    four_hour_warmup: Vec<Bar>,
    five_minute_bars: Vec<Bar>,
    four_hour_bars: Vec<Bar>,
}

/// 一个参数候选在指定样本上的保守评估结果
///
/// walk-forward 只使用这里的保守指标选择 IS 胜者，避免用引擎未纳入全部压力成本的原始
/// Sharpe 直接调参；OOS 只评估胜者，不再次选择。
#[derive(Clone, Copy, Debug)]
struct BacktestEvaluation {
    trade_direction: TradeDirection,
    risk_reward: Decimal,
    trades: u64,
    conservative_pnl: Decimal,
    conservative_cost_adjusted_sharpe: Option<f64>,
    conservative_max_drawdown_pct: Option<f64>,
    conservative_annualized_return_pct: Option<f64>,
    conservative_calmar: Option<f64>,
    positive_days: u64,
    negative_days: u64,
    flat_days: u64,
    engine_sharpe: Option<f64>,
}

/// 单次引擎运行的参数评估及逐日保守 PnL，后者用于无重叠 OOS 组合汇总
#[derive(Debug)]
struct BacktestRunResult {
    evaluation: BacktestEvaluation,
    conservative_daily_pnl: Vec<Decimal>,
}

impl BacktestEvaluation {
    /// 生成单行且便于 grep 的参数评估记录，供 IS 选择和人工复核
    fn summary(self, sample: &str) -> String {
        format!(
            "SLC parameter evaluation: sample={sample}, direction={}, risk_reward={}, trades={}, conservative_pnl={}, conservative_cost_adjusted_sharpe={}, conservative_max_drawdown_pct={}, conservative_annualized_return_pct={}, conservative_calmar={}, positive_days={}, negative_days={}, flat_days={}, engine_sharpe={}",
            self.trade_direction,
            self.risk_reward,
            self.trades,
            self.conservative_pnl,
            format_optional_metric(self.conservative_cost_adjusted_sharpe),
            format_optional_metric(self.conservative_max_drawdown_pct),
            format_optional_metric(self.conservative_annualized_return_pct),
            format_optional_metric(self.conservative_calmar),
            self.positive_days,
            self.negative_days,
            self.flat_days,
            format_optional_metric(self.engine_sharpe),
        )
    }
}

/// 格式化可选浮点指标；没有证据时输出 `n/a`，不使用零伪装缺失数据
fn format_optional_metric(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| format!("{value:.4}"))
}

/// 仅按保守 IS Sharpe 选择候选；并列时依次比较 PnL 和较小目标以保证结果确定
fn select_in_sample_winner(
    evaluations: &[BacktestEvaluation],
    minimum_trades: u64,
) -> anyhow::Result<BacktestEvaluation> {
    evaluations
        .iter()
        .copied()
        .filter(|evaluation| {
            evaluation.trades >= minimum_trades
                && evaluation.conservative_cost_adjusted_sharpe.is_some()
        })
        .max_by(|left, right| {
            left.conservative_cost_adjusted_sharpe
                .expect("filtered above")
                .total_cmp(
                    &right
                        .conservative_cost_adjusted_sharpe
                        .expect("filtered above"),
                )
                .then_with(|| left.conservative_pnl.cmp(&right.conservative_pnl))
                .then_with(|| right.risk_reward.cmp(&left.risk_reward))
        })
        .with_context(|| {
            format!(
                "no target candidate produced at least {minimum_trades} trades and a defined in-sample Sharpe",
            )
        })
}

/// 判断单折是否同时通过 IS 稳定性及预先声明的 OOS 样本量、收益、Sharpe 和回撤门槛
fn walk_forward_fold_passes(
    in_sample_sharpe: f64,
    evaluation: BacktestEvaluation,
    settings: WalkForwardSettings,
) -> bool {
    let Some(out_of_sample_sharpe) = evaluation.conservative_cost_adjusted_sharpe else {
        return false;
    };
    evaluation.trades >= settings.minimum_oos_trades
        && evaluation.conservative_pnl > Decimal::ZERO
        && out_of_sample_sharpe >= settings.minimum_oos_sharpe
        && walk_forward_verdict(in_sample_sharpe, out_of_sample_sharpe) == "acceptable"
        && evaluation
            .conservative_max_drawdown_pct
            .is_some_and(|drawdown| drawdown >= -settings.maximum_oos_drawdown_pct)
}

/// 返回 OOS 边界之前最近的完整 Bar 作为指标 warmup，且不把这些 Bar 再计入 OOS 绩效
fn split_warmup_bars(
    initial_warmup: &[Bar],
    replay_bars: &[Bar],
    split: UnixNanos,
    count: usize,
    instrument_id: InstrumentId,
    bar_type: BarType,
) -> anyhow::Result<Vec<Bar>> {
    let mut bars = initial_warmup
        .iter()
        .chain(replay_bars)
        .copied()
        .filter(|bar| bar.ts_event < split)
        .collect::<Vec<_>>();
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    anyhow::ensure!(
        bars.len() >= count,
        "walk-forward warmup requires {count} bars for {instrument_id}: bar_type={bar_type}, available={}",
        bars.len(),
    );
    Ok(bars.split_off(bars.len() - count))
}

/// 一组严格按交易日滚动且 OOS 不重叠的 IS/OOS 时间边界
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WalkForwardWindow {
    train_start: UnixNanos,
    test_start: UnixNanos,
    test_end: UnixNanos,
}

/// 从全部标的实际出现的交易日生成滚动窗口，避免按自然日错误计算节假日
fn rolling_walk_forward_windows(
    inputs: &[PreparedBacktestInputs],
    config: &AppConfig,
    end: Timestamp,
    settings: WalkForwardSettings,
) -> anyhow::Result<Vec<WalkForwardWindow>> {
    let mut day_starts = BTreeMap::<String, UnixNanos>::new();
    for input in inputs {
        for bar in &input.five_minute_bars {
            let date = bar
                .ts_event
                .to_datetime_utc()
                .to_zoned(config.timezone.clone())
                .date()
                .to_string();
            day_starts
                .entry(date)
                .and_modify(|start| *start = (*start).min(bar.ts_event))
                .or_insert(bar.ts_event);
        }
    }
    walk_forward_windows_from_day_starts(
        &day_starts.into_values().collect::<Vec<_>>(),
        UnixNanos::from(end),
        settings,
    )
}

/// 根据有序交易日起点生成固定长度窗口；独立函数使边界与非重叠性质可直接测试
fn walk_forward_windows_from_day_starts(
    day_starts: &[UnixNanos],
    end: UnixNanos,
    settings: WalkForwardSettings,
) -> anyhow::Result<Vec<WalkForwardWindow>> {
    let window_days = settings
        .train_days
        .checked_add(settings.test_days)
        .context("walk-forward window length overflowed")?;
    anyhow::ensure!(
        day_starts.len() >= window_days,
        "walk-forward requires at least {window_days} trading days, available={}",
        day_starts.len(),
    );

    let mut windows = Vec::new();
    let mut first_day = 0;
    while first_day + window_days <= day_starts.len() {
        let test_end_index = first_day + window_days;
        windows.push(WalkForwardWindow {
            train_start: day_starts[first_day],
            test_start: day_starts[first_day + settings.train_days],
            test_end: day_starts.get(test_end_index).copied().unwrap_or(end),
        });
        first_day += settings.step_days;
    }
    anyhow::ensure!(
        windows.len() >= settings.minimum_folds,
        "walk-forward produced {} folds, below configured minimum {}",
        windows.len(),
        settings.minimum_folds,
    );
    Ok(windows)
}

/// 截取同一时间范围内的全部标的，并从边界之前重新构造各自 warmup
fn slice_backtest_inputs(
    inputs: &[PreparedBacktestInputs],
    config: &AppConfig,
    start: UnixNanos,
    end: UnixNanos,
) -> anyhow::Result<Vec<PreparedBacktestInputs>> {
    let mut sliced = Vec::with_capacity(inputs.len());
    for input in inputs {
        let sample = PreparedBacktestInputs {
            instrument_id: input.instrument_id,
            instrument: input.instrument.clone(),
            five_minute_warmup: split_warmup_bars(
                &input.five_minute_warmup,
                &input.five_minute_bars,
                start,
                config.five_minute_warmup,
                input.instrument_id,
                AppConfig::five_minute_bar_type(input.instrument_id),
            )?,
            four_hour_warmup: split_warmup_bars(
                &input.four_hour_warmup,
                &input.four_hour_bars,
                start,
                config.four_hour_warmup,
                input.instrument_id,
                AppConfig::four_hour_bar_type(input.instrument_id),
            )?,
            five_minute_bars: input
                .five_minute_bars
                .iter()
                .copied()
                .filter(|bar| bar.ts_event >= start && bar.ts_event < end)
                .collect(),
            four_hour_bars: input
                .four_hour_bars
                .iter()
                .copied()
                .filter(|bar| bar.ts_event >= start && bar.ts_event < end)
                .collect(),
        };
        anyhow::ensure!(
            !sample.five_minute_bars.is_empty(),
            "walk-forward window {start}..{end} leaves an empty 5m sample for {}",
            input.instrument_id,
        );
        sliced.push(sample);
    }
    Ok(sliced)
}

/// 通过共享限流的 Longbridge QuoteContext 为所有 symbol 加载 warmup 和回放数据
async fn prepare_backtest_inputs(
    config: &SlcBacktestConfig,
    context: &QuoteContext,
) -> anyhow::Result<Vec<PreparedBacktestInputs>> {
    let start_date = us_market_date(config.start)?;
    let end_date = us_market_date(config.end)?;
    println!("SLC data download: querying US trading calendar, range={start_date}..={end_date}",);
    let mut half_days = BTreeSet::new();
    let mut cursor = start_date;
    while cursor <= end_date {
        // Longbridge 要求每次 trading-days 查询区间短于一个自然月
        let chunk_end = cursor
            .checked_add(time::Duration::days(TRADING_DAYS_CHUNK_DAYS - 1))
            .unwrap_or(end_date)
            .min(end_date);
        println!("SLC data download: trading calendar chunk {cursor}..={chunk_end}");
        half_days.extend(
            quote_api_call_with_retry(|| context.trading_days(Market::US, cursor, chunk_end))
                .await
                .with_context(|| {
                    format!(
                        "failed to query US half trading days from {cursor} through {chunk_end}",
                    )
                })?
                .half_trading_days,
        );
        if chunk_end == end_date {
            break;
        }
        cursor = chunk_end
            .next_day()
            .context("trading-day date range exceeded the supported calendar")?;
    }
    let mut prepared = Vec::with_capacity(config.strategy.instruments.len());
    let instruments = load_instruments(&config.strategy, context).await?;
    let symbol_count = instruments.len();
    for (index, (instrument_id, instrument)) in instruments.into_iter().enumerate() {
        let symbol = instrument_id.symbol.as_str();
        let five_minute_bar_type = AppConfig::five_minute_bar_type(instrument_id);
        let four_hour_bar_type = AppConfig::four_hour_bar_type(instrument_id);
        let started = Instant::now();
        println!(
            "[{}/{}] [{instrument_id}] SLC data download: requesting 5m warmup",
            index + 1,
            symbol_count,
        );
        let five_minute_warmup = load_backtest_warmup_bars(
            context,
            symbol,
            Period::FiveMinute,
            five_minute_bar_type,
            config.strategy.five_minute_warmup,
            config.start,
            instrument.price_precision(),
        )
        .await?;
        println!(
            "[{}/{}] [{instrument_id}] SLC data download: 5m warmup ready, bars={}",
            index + 1,
            symbol_count,
            five_minute_warmup.len(),
        );
        let four_hour_warmup = load_backtest_warmup_bars(
            context,
            symbol,
            Period::FourHour,
            four_hour_bar_type,
            config.strategy.four_hour_warmup,
            config.start,
            instrument.price_precision(),
        )
        .await?;
        println!(
            "[{}/{}] [{instrument_id}] SLC data download: 4h warmup ready, bars={}",
            index + 1,
            symbol_count,
            four_hour_warmup.len(),
        );
        println!(
            "[{}/{}] [{instrument_id}] SLC data download: requesting 5m replay {start_date}..={end_date}",
            index + 1,
            symbol_count,
        );
        let downloaded_five_minute_bars = load_backtest_bars(
            context,
            symbol,
            Period::FiveMinute,
            five_minute_bar_type,
            config.start,
            config.end,
            instrument.price_precision(),
        )
        .await?;
        let mut five_minute_bars = Vec::with_capacity(downloaded_five_minute_bars.len());
        for bar in downloaded_five_minute_bars {
            let timestamp = Timestamp::from_nanosecond(i128::from(bar.ts_event.as_u64()))?;
            if !half_days.contains(&us_market_date(timestamp)?) {
                five_minute_bars.push(bar);
            }
        }
        anyhow::ensure!(
            !five_minute_bars.is_empty(),
            "Longbridge returned no complete non-half-day 5m bars for {symbol} in {}..={}",
            config.start,
            config.end,
        );
        println!(
            "[{}/{}] [{instrument_id}] SLC data download: 5m replay ready, bars={}",
            index + 1,
            symbol_count,
            five_minute_bars.len(),
        );
        let four_hour_bars = load_backtest_bars(
            context,
            symbol,
            Period::FourHour,
            four_hour_bar_type,
            config.start,
            config.end,
            instrument.price_precision(),
        )
        .await?;
        println!(
            "[{}/{}] [{instrument_id}] SLC data ready: 5m_warmup={}, 4h_warmup={}, 5m_bars={}, 4h_bars={}, skipped_half_days={}, elapsed={:.1}s",
            index + 1,
            symbol_count,
            five_minute_warmup.len(),
            four_hour_warmup.len(),
            five_minute_bars.len(),
            four_hour_bars.len(),
            half_days.len(),
            started.elapsed().as_secs_f64(),
        );
        prepared.push(PreparedBacktestInputs {
            instrument_id,
            instrument,
            five_minute_warmup,
            four_hour_warmup,
            five_minute_bars,
            four_hour_bars,
        });
    }
    Ok(prepared)
}

/// 加载结束时间不晚于回测起点的历史 Bar，用于初始化指标和高周期结构
async fn load_backtest_warmup_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    count: usize,
    start: Timestamp,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let request_count = count
        .checked_add(1)
        .filter(|request_count| *request_count <= MAX_WARMUP_BARS)
        .context("warmup count must leave room for the first non-warmup bar")?;
    let start_datetime = us_market_datetime(start)?;
    let candlesticks = quote_api_call_with_retry(|| {
        context.history_candlesticks_by_offset(
            symbol,
            period,
            AdjustType::NoAdjust,
            false,
            Some(start_datetime),
            request_count,
            TradeSessions::Intraday,
        )
    })
    .await
    .with_context(|| format!("failed to request historical {period:?} warmup for {symbol}"))?;
    parse_warmup_bars(
        symbol,
        period,
        bar_type,
        candlesticks,
        count,
        OffsetDateTime::from_unix_timestamp_nanos(start.as_nanosecond())?,
        price_precision,
    )
}

/// 将日期区间拆成小批下载，避免 Longbridge 的单次上限静默截断 5 分钟数据
async fn load_backtest_bars(
    context: &QuoteContext,
    symbol: &str,
    period: Period,
    bar_type: BarType,
    start: Timestamp,
    end: Timestamp,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let mut candlesticks = Vec::new();
    let mut cursor = us_market_date(start)?;
    let end_date = us_market_date(end)?;
    while cursor <= end_date {
        let chunk_end = cursor
            .checked_add(time::Duration::days(HISTORY_CHUNK_DAYS - 1))
            .unwrap_or(end_date)
            .min(end_date);
        candlesticks.extend(
            quote_api_call_with_retry(|| {
                context.history_candlesticks_by_date(
                    symbol,
                    period,
                    AdjustType::NoAdjust,
                    Some(cursor),
                    Some(chunk_end),
                    TradeSessions::Intraday,
                )
            })
            .await
            .with_context(|| {
                format!(
                    "failed to request historical {period:?} bars for {symbol} from {cursor} through {chunk_end}",
                )
            })?,
        );
        if chunk_end == end_date {
            break;
        }
        cursor = chunk_end
            .next_day()
            .context("historical date range exceeded the supported calendar")?;
    }
    parse_backtest_bars(
        symbol,
        period,
        bar_type,
        candlesticks,
        start,
        end,
        price_precision,
    )
}

/// 解析完整历史 K 线，并把 Bar 收盘时刻设为回放到达时间以维持无前视语义
fn parse_backtest_bars(
    symbol: &str,
    period: Period,
    bar_type: BarType,
    candlesticks: Vec<Candlestick>,
    start: Timestamp,
    end: Timestamp,
    price_precision: u8,
) -> anyhow::Result<Vec<Bar>> {
    let duration = match period {
        Period::FiveMinute => FIVE_MINUTE_NANOS,
        Period::FourHour => FOUR_HOUR_NANOS,
        _ => anyhow::bail!("unsupported SLC backtest period: {period:?}"),
    };
    let start = UnixNanos::from(start);
    let end = UnixNanos::from(end);
    let mut bars = candlesticks
        .into_iter()
        .map(|candlestick| {
            parse_bar_with_price_precision(
                bar_type,
                candlestick,
                UnixNanos::default(),
                price_precision,
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);
    bars.retain_mut(|bar| {
        let Some(close) = bar.ts_event.checked_add(duration) else {
            return false;
        };
        if bar.ts_event < start || close > end {
            return false;
        }
        bar.ts_init = close;
        true
    });
    anyhow::ensure!(
        !bars.is_empty(),
        "Longbridge returned no complete {period:?} bars for {symbol}: bar_type={bar_type}, start={start}, end={end}",
    );
    Ok(bars)
}

/// 将 UTC 时间戳转换为 Longbridge offset history 接口要求的美股市场本地时间
fn us_market_datetime(timestamp: Timestamp) -> anyhow::Result<PrimitiveDateTime> {
    let local = timestamp.to_zoned(get_timezone(US_TIMEZONE)?);
    let date = local.date();
    Ok(PrimitiveDateTime::new(
        Date::from_calendar_date(
            i32::from(date.year()),
            Month::try_from(u8::try_from(date.month())?)?,
            u8::try_from(date.day())?,
        )?,
        Time::from_hms(
            u8::try_from(local.hour())?,
            u8::try_from(local.minute())?,
            0,
        )?,
    ))
}

/// 从 Longbridge 交易日历返回当日权威美股常规收盘时刻及本地分钟数
async fn current_us_market_close(context: &QuoteContext) -> anyhow::Result<(Timestamp, u16)> {
    let now = Timestamp::now();
    let market_date = us_market_date(now)?;
    let trading_days =
        quote_api_call_with_retry(|| context.trading_days(Market::US, market_date, market_date))
            .await
            .context("failed to query the current US trading day from Longbridge")?;
    if !trading_days.trading_days.contains(&market_date) {
        anyhow::bail!("{market_date} is not a US trading day");
    }

    let close_time = if trading_days.half_trading_days.contains(&market_date) {
        time::macros::time!(13:00)
    } else {
        quote_api_call_with_retry(|| context.trading_session())
            .await
            .context("failed to query the current US trading session from Longbridge")?
            .into_iter()
            .find(|session| session.market == Market::US)
            .and_then(|session| {
                session
                    .trade_sessions
                    .into_iter()
                    .find(|session| session.trade_session == TradeSession::Intraday)
            })
            .map(|session| session.end_time)
            .context("Longbridge did not return a US regular trading session")?
    };
    let market_close_minute = u16::from(close_time.hour()) * 60 + u16::from(close_time.minute());
    Ok((us_market_close_at(now, close_time)?, market_close_minute))
}

/// 将时间戳转换成 America/New_York 时区下的美股市场日历日期
fn us_market_date(now: Timestamp) -> anyhow::Result<Date> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let local_date = now.to_zoned(timezone).date();
    Ok(Date::from_calendar_date(
        i32::from(local_date.year()),
        Month::try_from(u8::try_from(local_date.month())?)?,
        u8::try_from(local_date.day())?,
    )?)
}

/// 在夏令时切换下解析美股本地收盘时间；遇到缺失或歧义时拒绝猜测
fn us_market_close_at(now: Timestamp, close_time: Time) -> anyhow::Result<Timestamp> {
    let timezone = get_timezone(US_TIMEZONE)?;
    let local_date = now.to_zoned(timezone.clone()).date();
    let civil_close_time = CivilTime::new(
        i8::try_from(close_time.hour())?,
        i8::try_from(close_time.minute())?,
        i8::try_from(close_time.second())?,
        i32::try_from(close_time.nanosecond())?,
    )?;
    timezone
        .to_ambiguous_timestamp(local_date.to_datetime(civil_close_time))
        .unambiguous()
        .context("US market close must resolve to one timestamp")
}

/// 在策略已经经过收盘前退出窗口后，于权威 session close 定时停止节点
fn schedule_market_close_stop(node: &LiveNode, market_close: Timestamp) -> anyhow::Result<()> {
    let delay = Duration::try_from(Timestamp::now().duration_until(market_close))
        .context("US market close must be in the future")?;
    let handle = node.handle();
    log::info!("Longbridge SLC node will stop at market close {market_close}");
    get_runtime().spawn(async move {
        tokio::time::sleep(delay).await;
        handle.stop();
    });
    Ok(())
}

/// 构造 Longbridge 执行配置，默认使用保证金账户并禁止常规时段外成交
fn execution_config(config: &AppConfig) -> LongbridgeExecClientConfig {
    LongbridgeExecClientConfig {
        oauth_client_id: Some(config.oauth_client_id.clone()),
        oauth_callback_port: config.oauth_callback_port,
        account_type: AccountType::Margin,
        papertrading: config.papertrading,
        outside_rth: false,
        ..Default::default()
    }
}

/// 配置执行引擎级订单节流及各 instrument 的精确单笔名义金额上限
fn risk_engine_config(config: &AppConfig) -> LiveRiskEngineConfig {
    LiveRiskEngineConfig {
        bypass: false,
        max_order_submit_rate: "6/00:00:01".to_string(),
        max_order_modify_rate: "6/00:00:01".to_string(),
        max_notional_per_order: config
            .instruments
            .iter()
            .map(|instrument| {
                (
                    instrument.instrument_id.to_string(),
                    config.per_position_notional_limit().to_string(),
                )
            })
            .collect(),
        ..Default::default()
    }
}

struct TemporaryRiskState(PathBuf);

impl Drop for TemporaryRiskState {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "Failed to remove temporary SLC backtest risk state {}: {error}",
                self.0.display(),
            );
        }
    }
}

/// runner 停止后在同一把锁内复制所有 symbol 统计，获得一致的诊断快照
fn run_statistics_lines(run_statistics: &Mutex<RunStatistics>) -> anyhow::Result<Vec<String>> {
    Ok(run_statistics
        .lock()
        .map_err(|_| anyhow::anyhow!("SLC run statistics mutex was poisoned"))?
        .lines())
}

/// 仅把 5 分钟 Bar 送入撮合；4 小时 Bar 在策略内部按回放时钟推进结构
///
/// 这种隔离防止高周期 OHLC 被误当成可成交行情，同时保证实盘和回测的结构确认时刻一致。
/// 每次运行使用临时风险状态文件，避免参数候选之间共享 reservation 或日内 PnL。
fn run_backtest_engine(
    config: &SlcBacktestConfig,
    prepared: Vec<PreparedBacktestInputs>,
    sample: &str,
) -> anyhow::Result<BacktestRunResult> {
    let _cleanup = TemporaryRiskState(config.strategy.risk_state_path.clone());
    let account_risk = Arc::new(AccountRisk::load(config.strategy.risk_state_path.clone())?);
    let run_statistics = Arc::new(Mutex::new(RunStatistics::default()));
    let trading_days = prepared
        .iter()
        .flat_map(|input| input.five_minute_bars.iter())
        .map(|bar| {
            bar.ts_event
                .to_datetime_utc()
                .to_zoned(config.strategy.timezone.clone())
                .date()
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    println!(
        "SLC backtest run: sample={sample}, direction={}, risk_reward={}, trading_days={}",
        config.strategy.trade_direction,
        config.strategy.risk_reward,
        trading_days.len(),
    );
    let flatten_minute = RTH_CLOSE_MINUTE
        .checked_sub(config.strategy.session.flatten_before_close_minutes)
        .context("flatten buffer exceeds the regular US trading session")?;
    anyhow::ensure!(
        config.strategy.session.entry_start_minute < flatten_minute,
        "entry window starts after the SLC backtest pre-close flatten time",
    );

    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(
                config
                    .strategy
                    .instruments
                    .first()
                    .context("SLC backtest requires at least one instrument")?
                    .instrument_id
                    .venue,
            )
            .oms_type(OmsType::Netting)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![config.starting_balance])
            .reject_stop_orders(false)
            .bar_adaptive_high_low_ordering(true)
            .build()?,
    )?;

    let symbol_count = prepared.len();
    let mut bar_count = 0;
    let mut data = Vec::new();
    for prepared in prepared {
        engine.add_instrument(&prepared.instrument)?;
        bar_count += prepared.five_minute_bars.len();
        data.extend(prepared.five_minute_bars.into_iter().map(Data::Bar));
        engine.add_strategy(SlcStrategy::new(
            &config.strategy,
            prepared.instrument_id,
            prepared.instrument,
            prepared.five_minute_warmup,
            prepared.four_hour_warmup,
            Arc::clone(&account_risk),
            SlcRunConfig {
                flatten_minute,
                backtest_four_hour_bars: Some(prepared.four_hour_bars),
                round_trip_cost_per_share: config.round_trip_cost_per_share,
                log_bars: config.log_bars,
                run_statistics: Arc::clone(&run_statistics),
            },
        )?)?;
    }
    engine.add_data(data, None, true, true)?;
    engine.run(None, None, None, false)?;

    for line in run_statistics_lines(&run_statistics)? {
        println!("{line}");
    }

    let (aggregate, conservative_daily_pnl) = {
        let statistics = run_statistics
            .lock()
            .map_err(|_| anyhow::anyhow!("SLC run statistics mutex was poisoned"))?;
        (
            TradeAggregate::from_trades(
                statistics
                    .symbols
                    .values()
                    .flat_map(|symbol| symbol.trades.iter()),
            ),
            conservative_daily_pnl(&statistics, &trading_days, &config.strategy.timezone)
                .context("SLC conservative PnL is unavailable for one or more closed trades")?,
        )
    };
    let risk_metrics = risk_metrics_from_daily_pnl(
        &conservative_daily_pnl,
        config.starting_balance.as_decimal(),
    )
    .unwrap_or_default();

    let result = engine.get_result();
    println!(
        "SLC backtest complete: symbols={symbol_count}, 5m_bars={bar_count}, orders={}, positions={}",
        result.total_orders, result.total_positions,
    );
    println!("Summary: {:?}", result.summary);
    println!("PnL statistics: {:?}", result.stats_pnls);
    println!("Return statistics: {:?}", result.stats_returns);
    println!("General statistics: {:?}", result.stats_general);
    let evaluation = BacktestEvaluation {
        trade_direction: config.strategy.trade_direction,
        risk_reward: config.strategy.risk_reward,
        trades: aggregate.trades,
        conservative_pnl: aggregate.conservative_pnl,
        conservative_cost_adjusted_sharpe: risk_metrics.sharpe,
        conservative_max_drawdown_pct: risk_metrics.max_drawdown_pct,
        conservative_annualized_return_pct: risk_metrics.annualized_return_pct,
        conservative_calmar: risk_metrics.calmar,
        positive_days: risk_metrics.positive_days,
        negative_days: risk_metrics.negative_days,
        flat_days: risk_metrics.flat_days,
        engine_sharpe: result.stats_returns.get("Sharpe Ratio (252 days)").copied(),
    };
    println!("{}", evaluation.summary(sample));
    Ok(BacktestRunResult {
        evaluation,
        conservative_daily_pnl,
    })
}

/// 从第一个可选命令行参数读取 TOML 路径；未提供时使用仓库内示例配置
fn config_path_from_args() -> anyhow::Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    anyhow::ensure!(
        args.next().is_none(),
        "expected at most one argument: path to the SLC TOML configuration",
    );
    Ok(path)
}

/// 根据命令行模式选择实盘或历史 runner，始终复用同一份 TOML 和 SLC 策略实现
pub(super) async fn run(backtest: bool) -> anyhow::Result<()> {
    let config_path = config_path_from_args()?;
    if backtest {
        run_backtest(&config_path).await
    } else {
        run_live(&config_path).await
    }
}

/// 下载 Longbridge 历史数据并运行参数化多标的回测或滚动 walk-forward 评估
///
/// 每个滚动折只在 IS 比较目标候选，只把胜出参数运行于紧随其后的非重叠 OOS；无滚动配置时
/// 对全部候选分别运行完整样本。参数选择只依据保守指标，原始 Nautilus 统计仅用于核对。
async fn run_backtest(config_path: &Path) -> anyhow::Result<()> {
    let config = SlcBacktestConfig::load(config_path)?;
    println!("SLC configuration loaded from {}", config_path.display());
    let prepared = tokio::time::timeout(Duration::from_secs(config.timeout_secs), async {
        let sdk_config = config.strategy.data_config().sdk_config().await?;
        let (context, _receiver) = QuoteContext::new(sdk_config);
        prepare_backtest_inputs(&config, &context).await
    })
    .await
    .with_context(|| {
        format!(
            "SLC historical download exceeded {} seconds",
            config.timeout_secs,
        )
    })??;
    if let Some(settings) = config.walk_forward {
        let windows =
            rolling_walk_forward_windows(&prepared, &config.strategy, config.end, settings)?;
        let fold_count = windows.len();
        let mut passing_folds = 0_usize;
        let mut total_oos_trades = 0_u64;
        let mut total_oos_pnl = Decimal::ZERO;
        let mut combined_oos_daily_pnl = Vec::new();
        let mut selected_targets = BTreeMap::<Decimal, u64>::new();

        for (index, window) in windows.into_iter().enumerate() {
            let fold = index + 1;
            let in_sample = slice_backtest_inputs(
                &prepared,
                &config.strategy,
                window.train_start,
                window.test_start,
            )?;
            let out_of_sample = slice_backtest_inputs(
                &prepared,
                &config.strategy,
                window.test_start,
                window.test_end,
            )?;
            let mut in_sample_evaluations = Vec::with_capacity(config.risk_rewards.len());
            for risk_reward in &config.risk_rewards {
                let mut candidate = config.clone();
                candidate.strategy.risk_reward = *risk_reward;
                let sample = format!("WF-{fold:02}-IS");
                in_sample_evaluations
                    .push(run_backtest_engine(&candidate, in_sample.clone(), &sample)?.evaluation);
            }
            let winner =
                select_in_sample_winner(&in_sample_evaluations, settings.minimum_is_trades)?;
            *selected_targets.entry(winner.risk_reward).or_default() += 1;

            let mut candidate = config.clone();
            candidate.strategy.risk_reward = winner.risk_reward;
            let sample = format!("WF-{fold:02}-OOS");
            let out_of_sample = run_backtest_engine(&candidate, out_of_sample, &sample)?;
            let evaluation = out_of_sample.evaluation;
            let in_sample_sharpe = winner
                .conservative_cost_adjusted_sharpe
                .expect("winner requires a defined Sharpe");
            let stability_verdict = evaluation
                .conservative_cost_adjusted_sharpe
                .map_or("reject_undefined_oos", |sharpe| {
                    walk_forward_verdict(in_sample_sharpe, sharpe)
                });
            let degradation = evaluation
                .conservative_cost_adjusted_sharpe
                .filter(|_| in_sample_sharpe != 0.0)
                .map(|sharpe| sharpe / in_sample_sharpe);
            let passes = walk_forward_fold_passes(in_sample_sharpe, evaluation, settings);
            passing_folds += usize::from(passes);
            total_oos_trades += evaluation.trades;
            total_oos_pnl += evaluation.conservative_pnl;
            combined_oos_daily_pnl.extend(out_of_sample.conservative_daily_pnl);
            println!(
                "SLC walk-forward fold: fold={fold}/{fold_count}, train={}..{}, test={}..{}, selected_risk_reward={}, is_trades={}, oos_trades={}, is_conservative_sharpe={}, oos_conservative_sharpe={}, degradation_ratio={}, oos_conservative_pnl={}, oos_max_drawdown_pct={}, stability_verdict={stability_verdict}, pass={passes}",
                window.train_start,
                window.test_start,
                window.test_start,
                window.test_end,
                winner.risk_reward,
                winner.trades,
                evaluation.trades,
                format_optional_metric(Some(in_sample_sharpe)),
                format_optional_metric(evaluation.conservative_cost_adjusted_sharpe),
                format_optional_metric(degradation),
                evaluation.conservative_pnl,
                format_optional_metric(evaluation.conservative_max_drawdown_pct),
            );
        }

        let pass_rate =
            f64::from(u32::try_from(passing_folds)?) / f64::from(u32::try_from(fold_count)?);
        let aggregate_metrics = risk_metrics_from_daily_pnl(
            &combined_oos_daily_pnl,
            config.starting_balance.as_decimal(),
        )
        .context("rolling OOS samples produced no daily risk metrics")?;
        let eligible_for_paper_risk_scaling = pass_rate >= settings.minimum_pass_rate
            && total_oos_pnl > Decimal::ZERO
            && aggregate_metrics
                .sharpe
                .is_some_and(|sharpe| sharpe >= settings.minimum_oos_sharpe)
            && aggregate_metrics
                .max_drawdown_pct
                .is_some_and(|drawdown| drawdown >= -settings.maximum_oos_drawdown_pct);
        let selected_targets = selected_targets
            .into_iter()
            .map(|(risk_reward, folds)| format!("{risk_reward}:{folds}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "SLC rolling OOS aggregate: folds={fold_count}, passing_folds={passing_folds}, pass_rate={pass_rate:.4}, selected_targets={selected_targets}, trades={total_oos_trades}, conservative_pnl={total_oos_pnl}, conservative_sharpe={}, max_drawdown_pct={}, annualized_return_pct={}, calmar={}, positive_days={}, negative_days={}, flat_days={}, eligible_for_paper_risk_scaling={eligible_for_paper_risk_scaling}",
            format_optional_metric(aggregate_metrics.sharpe),
            format_optional_metric(aggregate_metrics.max_drawdown_pct),
            format_optional_metric(aggregate_metrics.annualized_return_pct),
            format_optional_metric(aggregate_metrics.calmar),
            aggregate_metrics.positive_days,
            aggregate_metrics.negative_days,
            aggregate_metrics.flat_days,
        );
        Ok(())
    } else {
        for risk_reward in &config.risk_rewards {
            let mut candidate = config.clone();
            candidate.strategy.risk_reward = *risk_reward;
            run_backtest_engine(&candidate, prepared.clone(), "FULL")?;
        }
        Ok(())
    }
}

/// 在构建带 reconciliation 的实盘节点前准备全部 symbol，任何一个失败都阻止整体启动
async fn run_live(config_path: &Path) -> anyhow::Result<()> {
    let config = AppConfig::load(config_path, true)?;
    log::info!("SLC configuration loaded from {}", config_path.display());
    let account_risk = Arc::new(AccountRisk::load(config.risk_state_path.clone())?);
    let run_statistics = Arc::new(Mutex::new(RunStatistics::default()));
    log::info!(
        "SLC account risk state loaded from {}",
        config.risk_state_path.display(),
    );
    let prepared = {
        let sdk_config = config.data_config().sdk_config().await?;
        let (context, _receiver) = QuoteContext::new(sdk_config);
        prepare_inputs(&config, &context).await?
    };
    let market_close = prepared
        .first()
        .context("SLC requires at least one prepared instrument")?
        .market_close;
    let market_close_minute = prepared
        .first()
        .expect("prepared instruments checked above")
        .market_close_minute;
    let flatten_minute = market_close_minute
        .checked_sub(config.session.flatten_before_close_minutes)
        .context("flatten buffer exceeds the current US trading session")?;
    anyhow::ensure!(
        config.session.entry_start_minute < flatten_minute,
        "entry window starts after today's pre-close flatten time",
    );

    let environment = if config.papertrading {
        Environment::Sandbox
    } else {
        Environment::Live
    };
    let trader_id = TraderId::from(TRADER_ID);
    let account_id = AccountId::from(ACCOUNT_ID);
    let exec_engine_config = LiveExecEngineConfig {
        reconciliation_lookback_mins: Some(24 * 60),
        reconciliation_instrument_ids: Some(
            config
                .instruments
                .iter()
                .map(|instrument| instrument.instrument_id.to_string())
                .collect(),
        ),
        open_check_interval_secs: Some(10.0),
        position_check_interval_secs: Some(15.0),
        ..Default::default()
    };
    let mut node = LiveNode::builder(trader_id, environment)?
        .with_name(NODE_NAME.to_string())
        .with_load_state(false)
        .with_save_state(false)
        .with_exec_engine_config(exec_engine_config)
        .with_risk_engine_config(risk_engine_config(&config))
        .with_reconciliation(true)
        .with_delay_post_stop_secs(10)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(config.data_config()),
        )?
        .add_exec_client(
            None,
            Box::new(LongbridgeExecutionClientFactory::new(trader_id, account_id)),
            Box::new(execution_config(&config)),
        )?
        .build()?;

    for prepared in prepared {
        node.kernel()
            .cache()
            .borrow_mut()
            .add_instrument(prepared.instrument.clone())?;
        node.add_strategy(SlcStrategy::new(
            &config,
            prepared.instrument_id,
            prepared.instrument,
            prepared.five_minute_bars,
            prepared.four_hour_bars,
            Arc::clone(&account_risk),
            SlcRunConfig {
                flatten_minute,
                backtest_four_hour_bars: None,
                round_trip_cost_per_share: Decimal::ZERO,
                log_bars: true,
                run_statistics: Arc::clone(&run_statistics),
            },
        )?)?;
    }
    schedule_market_close_stop(&node, market_close)?;
    let result = node.run().await;
    match run_statistics_lines(&run_statistics) {
        Ok(lines) => {
            for line in lines {
                log::info!("{line}");
            }
        }
        Err(e) => log::error!("Failed to report SLC run statistics: {e:#}"),
    }
    result
}

#[cfg(test)]
mod tests {
    use nautilus_model::{
        enums::LiquiditySide,
        identifiers::{TradeId, VenueOrderId},
    };

    use super::*;

    fn sdk_candlestick(
        open: &str,
        high: &str,
        low: &str,
        close: &str,
        timestamp: &str,
    ) -> Candlestick {
        toml::from_str(&format!(
            r#"
open = "{open}"
high = "{high}"
low = "{low}"
close = "{close}"
volume = 100
turnover = "10000"
timestamp = "{timestamp}"
trade_session = "Intraday"
open_updated = true
"#,
        ))
        .unwrap()
    }

    fn bar(
        bar_type: BarType,
        open: &str,
        high: &str,
        low: &str,
        close: &str,
        timestamp: u64,
    ) -> Bar {
        let price =
            |value: &str| Price::from_decimal_dp(value.parse::<Decimal>().unwrap(), 2).unwrap();
        Bar::new(
            bar_type,
            price(open),
            price(high),
            price(low),
            price(close),
            Quantity::from(100),
            UnixNanos::from(timestamp),
            UnixNanos::from(timestamp),
        )
    }

    fn five_minute_bar(open: &str, high: &str, low: &str, close: &str, timestamp: u64) -> Bar {
        bar(
            BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            open,
            high,
            low,
            close,
            timestamp,
        )
    }

    #[test]
    fn warmup_ignores_invalid_in_progress_bar() {
        let bars = parse_warmup_bars(
            "QQQ.US",
            Period::FiveMinute,
            BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL"),
            vec![
                sdk_candlestick(
                    "100.00",
                    "101.00",
                    "99.00",
                    "100.50",
                    "2026-09-04T13:25:00Z",
                ),
                sdk_candlestick("0.00", "101.00", "100.00", "100.50", "2026-09-04T13:30:00Z"),
            ],
            1,
            time::macros::datetime!(2026-09-04 13:31 UTC),
            2,
        )
        .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(
            bars[0].ts_event,
            UnixNanos::from(
                u64::try_from(
                    time::macros::datetime!(2026-09-04 13:25 UTC).unix_timestamp_nanos(),
                )
                .unwrap(),
            ),
        );
    }

    #[test]
    fn completed_warmup_bar_normalizes_provider_ohlc_envelope() {
        let bar_type = BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL");
        let bars = parse_warmup_bars(
            "QQQ.US",
            Period::FiveMinute,
            bar_type,
            vec![sdk_candlestick(
                "99.00",
                "101.00",
                "100.00",
                "100.50",
                "2026-09-04T13:25:00Z",
            )],
            1,
            time::macros::datetime!(2026-09-04 13:31 UTC),
            2,
        )
        .unwrap();

        assert_eq!(bars[0].open, Price::from("99.00"));
        assert_eq!(bars[0].high, Price::from("101.00"));
        assert_eq!(bars[0].low, Price::from("99.00"));
        assert_eq!(bars[0].close, Price::from("100.50"));
    }

    #[rstest::rstest]
    fn backtest_bars_are_deduplicated_and_arrive_at_close() {
        let bar_type = BarType::from("QQQ.US.LONGBRIDGE-5-MINUTE-LAST-EXTERNAL");
        let start = "2026-09-04T13:25:00Z".parse::<Timestamp>().unwrap();
        let end = "2026-09-04T13:35:00Z".parse::<Timestamp>().unwrap();
        let bars = parse_backtest_bars(
            "QQQ.US",
            Period::FiveMinute,
            bar_type,
            vec![
                sdk_candlestick(
                    "100.00",
                    "101.00",
                    "99.00",
                    "100.50",
                    "2026-09-04T13:20:00Z",
                ),
                sdk_candlestick(
                    "100.123",
                    "101.234",
                    "99.012",
                    "100.567",
                    "2026-09-04T13:25:00Z",
                ),
                sdk_candlestick(
                    "100.123",
                    "102.345",
                    "99.012",
                    "101.567",
                    "2026-09-04T13:25:00Z",
                ),
                sdk_candlestick(
                    "101.567",
                    "102.345",
                    "101.123",
                    "101.789",
                    "2026-09-04T13:30:00Z",
                ),
                sdk_candlestick(
                    "101.75",
                    "103.00",
                    "101.00",
                    "102.50",
                    "2026-09-04T13:35:00Z",
                ),
            ],
            start,
            end,
            2,
        )
        .unwrap();

        assert_eq!(bars.len(), 2);
        assert!(bars.iter().all(|bar| {
            [bar.open, bar.high, bar.low, bar.close]
                .iter()
                .all(|price| price.precision == 2)
        }));
        assert_eq!(bars[0].ts_event, UnixNanos::from(start));
        assert_eq!(
            bars[0].ts_init,
            UnixNanos::from(start)
                .checked_add(FIVE_MINUTE_NANOS)
                .unwrap(),
        );
        assert_eq!(bars[1].ts_init, UnixNanos::from(end));
    }

    fn quote(bid: &str, ask: &str, timestamp: u64) -> QuoteTick {
        QuoteTick::new(
            InstrumentId::from("QQQ.US.LONGBRIDGE"),
            Price::from(bid),
            Price::from(ask),
            Quantity::from(100),
            Quantity::from(100),
            UnixNanos::from(timestamp),
            UnixNanos::from(timestamp),
        )
    }

    fn signal_rules() -> SignalRules {
        SignalRules {
            trade_direction: TradeDirection::Both,
            zone_ttl_bars: 234,
            minimum_fresh_level_age_bars: 1,
            max_zones_per_side: 8,
            confirmation_window_bars: 6,
            displacement_atr_multiple: 1.0,
            displacement_close_fraction: 0.35,
            displacement_max_bars: 3,
            level_extreme_lookback_bars: 3,
            oversold: 20.0,
            overbought: 80.0,
        }
    }

    fn signal_state() -> SlcSignalState {
        SlcSignalState {
            five_minute_bars: FinalBarBuffer::default(),
            four_hour_bars: FinalBarBuffer::default(),
            structure: PivotStructure::new(2),
            atr: AverageTrueRange::new(1, Some(MovingAverageType::Wilder), Some(true), None),
            stochastics: Stochastics::new_with_params(
                1,
                1,
                1,
                MovingAverageType::Simple,
                StochasticsDMethod::MovingAverage,
            ),
            recent_five_minute_bars: VecDeque::new(),
            level_trend: Trend::Neutral,
            last_demand_source: None,
            last_supply_source: None,
            previous_k: None,
            demand: VecDeque::new(),
            supply: VecDeque::new(),
            funnel: SignalFunnel::default(),
            rules: signal_rules(),
        }
    }

    #[test]
    fn five_minute_indicators_initialize_without_four_hour_structure() {
        let mut signals = signal_state();

        let _ = signals.process_five_minute(five_minute_bar("100", "101", "99", "100", 1), false);

        assert!(signals.atr.initialized());
        assert!(signals.stochastics.initialized());
        assert!(!signals.structure.initialized());
        assert_eq!(signals.structure.trend(), Trend::Neutral);
        assert!(signals.indicators_initialized());
    }

    #[test]
    fn high_timeframe_trend_change_discards_old_levels() {
        let mut signals = signal_state();
        signals.level_trend = Trend::Up;
        signals.last_demand_source = Some(UnixNanos::from(1));
        signals.last_supply_source = Some(UnixNanos::from(2));
        signals.demand.push_back(Zone::from_bar(
            ZoneKind::Demand,
            five_minute_bar("100", "101", "99", "100", 1),
        ));
        signals.supply.push_back(Zone::from_bar(
            ZoneKind::Supply,
            five_minute_bar("100", "101", "99", "100", 1),
        ));

        signals.align_levels_with_trend(Trend::Down);

        assert_eq!(signals.level_trend, Trend::Down);
        assert!(signals.demand.is_empty());
        assert!(signals.supply.is_empty());
        assert_eq!(signals.last_demand_source, None);
        assert_eq!(signals.last_supply_source, None);
    }

    #[test]
    fn backtest_uses_standard_stop_market_semantics() {
        assert_eq!(protective_stop_order_type(true), OrderType::StopMarket);
        assert_eq!(
            protective_stop_order_type(false),
            OrderType::MarketIfTouched,
        );
    }

    #[test]
    fn backtest_protection_links_resting_target_and_stop() {
        let event = OrderFilled::new(
            TraderId::from("TRADER-001"),
            StrategyId::from("SLC-001"),
            InstrumentId::from("QQQ.US.LONGBRIDGE"),
            ClientOrderId::from("ENTRY-001"),
            VenueOrderId::from("VENUE-001"),
            AccountId::from("ACCOUNT-001"),
            TradeId::from("TRADE-001"),
            OrderSide::Buy,
            OrderType::Limit,
            Quantity::from(10),
            Price::from("100.00"),
            Currency::USD(),
            LiquiditySide::Taker,
            UUID4::new(),
            UnixNanos::from(1),
            UnixNanos::from(1),
            false,
            None,
            None,
            None,
        );
        let stop_order_id = ClientOrderId::from("STOP-001");
        let target_order_id = ClientOrderId::from("TARGET-001");
        let orders = backtest_protective_orders(
            &event,
            OrderSide::Sell,
            Price::from("99.00"),
            Price::from("102.00"),
            OrderListId::from("OL-001"),
            stop_order_id,
            target_order_id,
        )
        .unwrap();

        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].order_type(), OrderType::StopMarket);
        assert_eq!(orders[0].trigger_price(), Some(Price::from("99.00")));
        assert_eq!(orders[0].linked_order_ids(), Some(&[target_order_id][..]));
        assert_eq!(orders[1].order_type(), OrderType::Limit);
        assert_eq!(orders[1].price(), Some(Price::from("102.00")));
        assert_eq!(orders[1].linked_order_ids(), Some(&[stop_order_id][..]));
        assert!(orders.iter().all(|order| {
            order.contingency_type() == Some(ContingencyType::Ouo)
                && order.order_list_id() == Some(OrderListId::from("OL-001"))
                && order.is_reduce_only()
                && order.time_in_force() == TimeInForce::Day
                && order.parent_order_id().is_none()
        }));
    }

    #[test]
    fn configured_symbols_have_unique_order_id_tags() {
        let qqq = StrategyCore::new(slc_strategy_config(InstrumentId::from("QQQ.US.LONGBRIDGE")));
        let aapl = StrategyCore::new(slc_strategy_config(InstrumentId::from(
            "AAPL.US.LONGBRIDGE",
        )));

        assert_eq!(qqq.order_id_tag(), Some("QQQ.US"));
        assert_eq!(aapl.order_id_tag(), Some("AAPL.US"));
    }

    #[rstest::rstest]
    fn test_parse_multiple_symbol_configs() {
        let instruments = parse_instruments(&[
            SymbolConfigEntry {
                symbol: "QQQ.US".to_string(),
                price_increment: "0.01".to_string(),
            },
            SymbolConfigEntry {
                symbol: "AAPL.US".to_string(),
                price_increment: "0.01".to_string(),
            },
            SymbolConfigEntry {
                symbol: "MSFT.US".to_string(),
                price_increment: "0.01".to_string(),
            },
        ])
        .unwrap();

        assert_eq!(instruments.len(), 3);
        assert_eq!(instruments[0].instrument_id.symbol.as_str(), "QQQ.US");
        assert_eq!(instruments[1].instrument_id.symbol.as_str(), "AAPL.US");
        assert_eq!(instruments[2].instrument_id.symbol.as_str(), "MSFT.US");
        assert_eq!(instruments[0].price_increment, Price::from("0.01"));
    }

    #[rstest::rstest]
    fn test_parse_symbol_config_rejects_duplicates() {
        assert!(
            parse_instruments(&[
                SymbolConfigEntry {
                    symbol: "QQQ.US".to_string(),
                    price_increment: "0.01".to_string(),
                },
                SymbolConfigEntry {
                    symbol: "QQQ.US".to_string(),
                    price_increment: "0.01".to_string(),
                },
            ])
            .is_err()
        );
    }

    #[rstest::rstest]
    fn test_parse_symbol_config_rejects_non_us_equities() {
        assert!(
            parse_instruments(&[SymbolConfigEntry {
                symbol: "0700.HK".to_string(),
                price_increment: "0.001".to_string(),
            }])
            .is_err()
        );
    }

    #[rstest::rstest]
    fn example_toml_contains_a_complete_valid_configuration() {
        let file: SlcFileConfig = toml::from_str(include_str!("../slc_symbols.toml")).unwrap();
        let config =
            AppConfig::from_file_config(&file, Path::new(DEFAULT_CONFIG_PATH), false).unwrap();

        assert_eq!(config.instruments.len(), file.symbols.len());
        assert!(!config.instruments.is_empty());
        assert_eq!(config.risk_amount, Decimal::from(100));
        assert_eq!(config.max_account_notional, Decimal::from(60_000));
        assert_eq!(config.max_order_quantity, Quantity::from(1_000));
        assert_eq!(config.max_order_notional, Decimal::from(20_000));
        assert_eq!(config.per_position_notional_limit(), Decimal::from(20_000));
        assert_eq!(config.trade_direction, TradeDirection::Both);
        assert!(config.papertrading);

        let backtest = SlcBacktestConfig::load(Path::new(DEFAULT_CONFIG_PATH)).unwrap();
        assert!(backtest.start < backtest.end);
        assert_eq!(backtest.risk_rewards, vec![Decimal::from(2)],);
        assert!(backtest.walk_forward.is_none());
    }

    #[rstest::rstest]
    fn test_live_guard_requires_explicit_acknowledgement() {
        assert!(validate_live_guard(true, None).is_ok());
        assert!(validate_live_guard(false, None).is_err());
        assert!(validate_live_guard(false, Some(LIVE_ACK)).is_ok());
    }

    #[rstest::rstest]
    fn test_final_bar_buffer_replaces_updates_and_emits_once() {
        let mut buffer = FinalBarBuffer::default();
        let partial = five_minute_bar("100", "101", "99", "100", 1);
        let updated = five_minute_bar("100", "102", "99", "101", 1);
        let next = five_minute_bar("101", "103", "100", "102", 2);

        assert_eq!(buffer.update(partial), None);
        assert_eq!(buffer.update(updated), None);
        assert_eq!(buffer.update(next), Some(updated));
    }

    #[rstest::rstest]
    fn test_five_minute_gap_detection() {
        let first = UnixNanos::from(1_000_000_000);

        assert!(!has_five_minute_gap(
            first,
            UnixNanos::from(first.as_u64() + FIVE_MINUTE_NANOS),
        ));
        assert!(has_five_minute_gap(
            first,
            UnixNanos::from(first.as_u64() + FIVE_MINUTE_NANOS * 2),
        ));
    }

    #[rstest::rstest]
    #[case(949, true, false, false)]
    #[case(950, false, false, false)]
    #[case(950, true, true, false)]
    #[case(950, true, false, true)]
    fn preclose_exit_requires_unmanaged_exposure(
        #[case] close_minute: u16,
        #[case] has_exposure: bool,
        #[case] exit_pending: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(
            should_request_preclose_exit(close_minute, 950, has_exposure, exit_pending),
            expected,
        );
    }

    #[rstest::rstest]
    fn test_supply_and_demand_displacement_rules() {
        let rules = signal_rules();
        let bearish_base = five_minute_bar("100.0", "101.0", "98.0", "99.0", 1);
        let bullish_displacement = five_minute_bar("99.0", "104.0", "99.0", "103.5", 2);
        let bullish_base = five_minute_bar("100.0", "102.0", "99.0", "101.0", 3);
        let bearish_displacement = five_minute_bar("101.0", "101.0", "96.0", "96.5", 4);

        assert_eq!(
            displacement_zone(
                &VecDeque::from([bearish_base, bullish_displacement]),
                2.0,
                rules,
            )
            .map(|(kind, source, _, _)| (kind, source)),
            Some((ZoneKind::Demand, bearish_base)),
        );
        assert_eq!(
            displacement_zone(
                &VecDeque::from([bullish_base, bearish_displacement]),
                2.0,
                rules,
            )
            .map(|(kind, source, _, _)| (kind, source)),
            Some((ZoneKind::Supply, bullish_base)),
        );
    }

    #[rstest::rstest]
    fn test_displacement_can_complete_across_three_bars() {
        let rules = signal_rules();
        let source = five_minute_bar("100.0", "101.0", "98.0", "99.0", 1);
        let first = five_minute_bar("99.0", "100.0", "98.8", "99.8", 2);
        let second = five_minute_bar("99.8", "100.8", "99.6", "100.6", 3);
        let third = five_minute_bar("100.6", "102.0", "100.5", "101.8", 4);

        assert_eq!(
            displacement_zone(&VecDeque::from([source, first, second]), 2.0, rules)
                .map(|(kind, source, _, _)| (kind, source)),
            None,
        );
        assert_eq!(
            displacement_zone(&VecDeque::from([source, first, second, third]), 2.0, rules,)
                .map(|(kind, source, _, _)| (kind, source)),
            Some((ZoneKind::Demand, source)),
        );
    }

    #[test]
    fn non_extreme_displacement_cannot_enter_fresh() {
        let prior_low = five_minute_bar("98.0", "100.0", "97.0", "99.0", 1);
        let source = five_minute_bar("100.0", "101.0", "98.0", "99.0", 2);
        let displacement = five_minute_bar("99.0", "104.0", "99.0", "103.5", 3);

        assert_eq!(
            displacement_zone(
                &VecDeque::from([prior_low, source, displacement]),
                2.0,
                signal_rules(),
            )
            .map(|(kind, candidate, _, fresh_entry_eligible)| {
                (kind, candidate, fresh_entry_eligible)
            }),
            Some((ZoneKind::Demand, source, false)),
        );
    }

    #[rstest::rstest]
    fn test_zone_confirmation_expires_after_configured_window() {
        let rules = SignalRules {
            confirmation_window_bars: 1,
            ..signal_rules()
        };
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch = five_minute_bar("102", "102", "100", "101", 2);
        let later = five_minute_bar("102", "103", "102", "103", 3);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Demand, source)]);
        let no_confirmation = Confirmation {
            extreme: false,
            reentry: false,
        };

        assert_eq!(
            observe_zones(
                &mut zones,
                touch,
                no_confirmation,
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert_eq!(zones.len(), 1);
        assert_eq!(
            observe_zones(
                &mut zones,
                later,
                no_confirmation,
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn zone_rejects_reentry_when_the_extreme_precedes_the_touch() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch_and_reentry = five_minute_bar("102", "102", "100", "101.2", 2);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Demand, source)]);

        assert_eq!(
            observe_zones(
                &mut zones,
                touch_and_reentry,
                Confirmation {
                    extreme: false,
                    reentry: true,
                },
                true,
                OrderSide::Buy,
                rules,
            ),
            None,
        );
        assert_eq!(zones[0].state, ZoneState::AwaitingConfirmation);
    }

    #[test]
    fn premature_fresh_retest_invalidates_the_level() {
        let rules = SignalRules {
            minimum_fresh_level_age_bars: 3,
            ..signal_rules()
        };
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let mut zone = Zone::from_bar(ZoneKind::Demand, source);

        assert_eq!(
            zone.observe(
                five_minute_bar("102", "102", "100", "101.2", 2),
                Confirmation {
                    extreme: true,
                    reentry: false,
                },
                true,
                OrderSide::Buy,
                rules,
            ),
            ZoneObservation::Remove,
        );
    }

    #[test]
    fn non_extreme_level_requires_break_and_reclaim() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let mut zones = VecDeque::from([Zone::from_displacement(
            ZoneKind::Demand,
            source,
            2.0,
            1.5,
            false,
        )]);

        let fresh = observe_zones(
            &mut zones,
            five_minute_bar("102", "102", "100", "101.2", 2),
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        assert_eq!(fresh, None);
        assert_eq!(zones[0].state, ZoneState::Fresh);

        let _ = observe_zones(
            &mut zones,
            five_minute_bar("100", "100", "98", "98.5", 3),
            Confirmation {
                extreme: false,
                reentry: false,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        let _ = observe_zones(
            &mut zones,
            five_minute_bar("99", "102", "99", "101.5", 4),
            Confirmation {
                extreme: false,
                reentry: false,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        let _ = observe_zones(
            &mut zones,
            five_minute_bar("101", "101.5", "100", "101.2", 5),
            Confirmation {
                extreme: true,
                reentry: false,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        let reclaimed = observe_zones(
            &mut zones,
            five_minute_bar("101.2", "102", "101", "101.8", 6),
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Buy,
            rules,
        );

        assert!(reclaimed.is_some_and(|signal| signal.level == SignalLevel::Reclaimed));
    }

    #[rstest::rstest]
    fn test_zone_accepts_stochastic_reentry_within_confirmation_window() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let touch = five_minute_bar("101", "101", "100", "100.5", 2);
        let confirmation = five_minute_bar("101.5", "102.5", "101.5", "102", 3);
        let mut zones = VecDeque::from([Zone::from_displacement(
            ZoneKind::Demand,
            source,
            2.0,
            1.5,
            true,
        )]);

        let _ = observe_zones(
            &mut zones,
            touch,
            Confirmation {
                extreme: true,
                reentry: false,
            },
            true,
            OrderSide::Buy,
            rules,
        );
        let signal = observe_zones(
            &mut zones,
            confirmation,
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Buy,
            rules,
        );

        assert!(signal.is_some());
        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_broken_supply_reclaims_and_confirms_once() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let broken = five_minute_bar("100", "103", "100", "102", 2);
        let still_above = five_minute_bar("102", "104", "101", "103", 3);
        let reclaimed = five_minute_bar("102", "102", "98", "98.5", 4);
        let retest = five_minute_bar("98", "100", "97", "99.5", 5);
        let confirmed = five_minute_bar("99", "100", "98", "98.5", 6);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Supply, source)]);
        let idle = Confirmation {
            extreme: false,
            reentry: false,
        };

        assert_eq!(
            observe_zones(&mut zones, broken, idle, true, OrderSide::Sell, rules),
            None,
        );
        assert_eq!(zones[0].state, ZoneState::BrokenOnce);
        assert_eq!(
            observe_zones(&mut zones, still_above, idle, true, OrderSide::Sell, rules,),
            None,
        );
        assert_eq!(
            observe_zones(&mut zones, reclaimed, idle, true, OrderSide::Sell, rules,),
            None,
        );
        assert_eq!(zones[0].state, ZoneState::Reclaimed);
        assert_eq!(
            observe_zones(
                &mut zones,
                retest,
                Confirmation {
                    extreme: true,
                    reentry: false,
                },
                true,
                OrderSide::Sell,
                rules,
            ),
            None,
        );
        let signal = observe_zones(
            &mut zones,
            confirmed,
            Confirmation {
                extreme: false,
                reentry: true,
            },
            true,
            OrderSide::Sell,
            rules,
        );

        assert_eq!(signal.map(|signal| signal.side), Some(OrderSide::Sell));
        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_reclaimed_supply_is_invalid_after_second_break() {
        let rules = signal_rules();
        let source = five_minute_bar("100", "101", "99", "100", 1);
        let broken = five_minute_bar("100", "103", "100", "102", 2);
        let reclaimed = five_minute_bar("102", "102", "98", "98.5", 3);
        let second_break = five_minute_bar("99", "103", "99", "102", 4);
        let mut zones = VecDeque::from([Zone::from_bar(ZoneKind::Supply, source)]);
        let idle = Confirmation {
            extreme: false,
            reentry: false,
        };

        let _ = observe_zones(&mut zones, broken, idle, true, OrderSide::Sell, rules);
        let _ = observe_zones(&mut zones, reclaimed, idle, true, OrderSide::Sell, rules);
        let _ = observe_zones(&mut zones, second_break, idle, true, OrderSide::Sell, rules);

        assert!(zones.is_empty());
    }

    #[rstest::rstest]
    fn test_entry_target_and_risk_sizing_use_exact_decimals() {
        let signal = Signal {
            side: OrderSide::Buy,
            level: SignalLevel::Fresh,
            entry: Price::from("100.00"),
            zone_low: Price::from("98.00"),
            zone_high: Price::from("99.00"),
            level_age_bars: 1,
            confirmation_bars: 0,
            confirmation_close_location: 0.75,
            distance_atr: 0.0,
            zone_width_atr: 0.5,
            displacement_strength_atr: 1.0,
            ts_event: UnixNanos::from(1),
        };
        let (stop, entry_limit) = entry_prices(signal, Price::from("0.01"), 2, 1, 5).unwrap();
        let quantity = risk_sized_quantity(
            entry_limit,
            stop,
            Decimal::from(25),
            Quantity::from(20),
            Decimal::from(5_000),
            Quantity::from(1),
        )
        .unwrap();
        let target = target_price(
            OrderSide::Buy,
            Decimal::from_str("100.03").unwrap(),
            stop,
            Price::from("0.01"),
            2,
            Decimal::from(2),
        )
        .unwrap();

        assert_eq!(stop, Price::from("97.99"));
        assert_eq!(entry_limit, Price::from("100.05"));
        assert_eq!(target, Price::from("104.11"));
        assert_eq!(quantity, Some(Quantity::from(12)));
    }

    #[rstest::rstest]
    fn test_risk_sizing_reduces_quantity_to_the_notional_cap() {
        let quantity = risk_sized_quantity(
            Price::from("500.00"),
            Price::from("499.50"),
            Decimal::from(25),
            Quantity::from(50),
            Decimal::from(5_000),
            Quantity::from(1),
        )
        .unwrap();

        assert_eq!(quantity, Some(Quantity::from(10)));
    }

    #[rstest::rstest]
    fn test_signal_quality_uses_directional_close_location() {
        let weak = five_minute_bar("100", "102", "98", "100", 1);
        let strong_long = five_minute_bar("100", "102", "98", "101", 2);
        let strong_short = five_minute_bar("100", "102", "98", "99", 3);

        assert_eq!(directional_close_location(weak, OrderSide::Buy), 0.5);
        assert_eq!(
            directional_close_location(strong_long, OrderSide::Buy),
            0.75
        );
        assert_eq!(
            directional_close_location(strong_short, OrderSide::Sell),
            0.75
        );
    }

    #[rstest::rstest]
    fn test_risk_utilization_is_exact_fraction_of_budget() {
        assert_eq!(
            risk_utilization(Decimal::from(5), Decimal::from(25)),
            Decimal::new(2, 1),
        );
    }

    #[rstest::rstest]
    fn test_realtime_target_uses_executable_quote_side() {
        let target = Price::from("100.00");

        assert!(!quote_reaches_target(
            OrderSide::Buy,
            target,
            &quote("99.99", "100.05", 1),
        ));
        assert!(quote_reaches_target(
            OrderSide::Buy,
            target,
            &quote("100.00", "100.05", 2),
        ));
        assert!(!quote_reaches_target(
            OrderSide::Sell,
            target,
            &quote("99.95", "100.01", 3),
        ));
        assert!(quote_reaches_target(
            OrderSide::Sell,
            target,
            &quote("99.95", "100.00", 4),
        ));
    }

    #[rstest::rstest]
    fn test_bar_target_fallback_ignores_prices_before_first_fill() {
        let target = Price::from("100.00");
        let first_fill_ts = UnixNanos::from(100);
        let fill_bar = five_minute_bar("99.00", "101.00", "98.00", "100.00", 90);
        let later_bar = five_minute_bar("99.00", "100.00", "99.00", "100.00", 101);

        assert!(!bar_reaches_target(
            OrderSide::Buy,
            target,
            first_fill_ts,
            fill_bar,
        ));
        assert!(bar_reaches_target(
            OrderSide::Buy,
            target,
            first_fill_ts,
            later_bar,
        ));
        assert!(!bar_reaches_target(
            OrderSide::Sell,
            target,
            first_fill_ts,
            fill_bar,
        ));
        assert!(bar_reaches_target(
            OrderSide::Sell,
            target,
            first_fill_ts,
            later_bar,
        ));
    }

    #[rstest::rstest]
    fn test_bar_reports_ambiguous_exit_when_stop_and_target_are_both_reached() {
        let bar = five_minute_bar("100.00", "102.00", "98.00", "101.00", 1);

        assert!(bar_reaches_stop_and_target(
            Price::from("99.00"),
            Price::from("101.50"),
            bar,
        ));
        assert!(!bar_reaches_stop_and_target(
            Price::from("97.00"),
            Price::from("101.50"),
            bar,
        ));
    }

    #[rstest::rstest]
    fn test_confirmation_distance_is_normalized_by_atr() {
        let zone = Zone::from_bar(
            ZoneKind::Demand,
            five_minute_bar("99.00", "101.00", "98.00", "100.00", 1),
        );

        assert_eq!(zone_distance_atr(zone, Price::from("101.50"), 2.0), 0.25,);
        assert_eq!(zone_distance_atr(zone, Price::from("100.00"), 2.0), 0.0);
    }

    #[rstest::rstest]
    fn test_order_notional_reserves_capacity_for_each_position_slot() {
        assert_eq!(
            per_position_notional_limit(Decimal::from(5_000), 2, Decimal::from(5_000)),
            Decimal::from(2_500),
        );
        assert_eq!(
            per_position_notional_limit(Decimal::from(10_000), 2, Decimal::from(2_000)),
            Decimal::from(2_000),
        );
    }

    #[rstest::rstest]
    fn test_risk_reward_grid_is_sorted_and_deduplicated() {
        assert_eq!(
            parse_decimal_grid(&[
                "2".to_string(),
                "1.5".to_string(),
                "1.75".to_string(),
                "2".to_string(),
            ])
            .unwrap(),
            vec![Decimal::new(15, 1), Decimal::new(175, 2), Decimal::from(2),],
        );
    }

    #[rstest::rstest]
    fn test_trade_direction_parsing_and_entry_filter() {
        let both = "both".parse::<TradeDirection>().unwrap();
        let long = "long".parse::<TradeDirection>().unwrap();
        let short = "short".parse::<TradeDirection>().unwrap();

        assert!(both.allows(OrderSide::Buy) && both.allows(OrderSide::Sell));
        assert!(long.allows(OrderSide::Buy) && !long.allows(OrderSide::Sell));
        assert!(short.allows(OrderSide::Sell) && !short.allows(OrderSide::Buy));
        assert!("invalid".parse::<TradeDirection>().is_err());
    }

    #[rstest::rstest]
    fn test_annualized_sharpe_requires_variance_and_preserves_sign() {
        assert_eq!(annualized_sharpe(&[0.0, 0.0]), None);
        assert!(annualized_sharpe(&[0.01, 0.02, 0.0]).is_some_and(|value| value > 0.0));
        assert!(annualized_sharpe(&[0.01, -0.02, -0.01]).is_some_and(|value| value < 0.0),);
    }

    #[rstest::rstest]
    fn test_walk_forward_verdict_rejects_loss_and_large_degradation() {
        assert_eq!(walk_forward_verdict(1.0, 0.6), "acceptable");
        assert_eq!(walk_forward_verdict(1.0, 0.4), "possible_overfit");
        assert_eq!(walk_forward_verdict(1.0, -0.1), "reject_non_positive_oos");
        assert_eq!(walk_forward_verdict(-0.1, 0.2), "reject_non_positive_is");
    }

    #[rstest::rstest]
    fn test_walk_forward_selects_only_the_best_is_sharpe() {
        let evaluations = [
            BacktestEvaluation {
                trade_direction: TradeDirection::Both,
                risk_reward: Decimal::new(15, 1),
                trades: 20,
                conservative_pnl: Decimal::from(10),
                conservative_cost_adjusted_sharpe: Some(0.8),
                conservative_max_drawdown_pct: Some(-0.1),
                conservative_annualized_return_pct: Some(0.2),
                conservative_calmar: Some(2.0),
                positive_days: 2,
                negative_days: 1,
                flat_days: 1,
                engine_sharpe: Some(1.0),
            },
            BacktestEvaluation {
                trade_direction: TradeDirection::Both,
                risk_reward: Decimal::from(2),
                trades: 20,
                conservative_pnl: Decimal::from(50),
                conservative_cost_adjusted_sharpe: Some(0.6),
                conservative_max_drawdown_pct: Some(-0.2),
                conservative_annualized_return_pct: Some(0.3),
                conservative_calmar: Some(1.5),
                positive_days: 2,
                negative_days: 1,
                flat_days: 1,
                engine_sharpe: Some(2.0),
            },
        ];

        assert_eq!(
            select_in_sample_winner(&evaluations, 20)
                .unwrap()
                .risk_reward,
            Decimal::new(15, 1),
        );
        assert!(select_in_sample_winner(&evaluations, 21).is_err());
    }

    #[rstest::rstest]
    fn test_rolling_walk_forward_windows_are_fixed_and_non_overlapping() {
        let settings = WalkForwardSettings {
            train_days: 3,
            test_days: 2,
            step_days: 2,
            minimum_folds: 2,
            minimum_is_trades: 1,
            minimum_oos_trades: 1,
            minimum_oos_sharpe: 0.0,
            maximum_oos_drawdown_pct: 1.0,
            minimum_pass_rate: 0.5,
        };
        let days = (1_u64..=7).map(UnixNanos::from).collect::<Vec<_>>();

        let windows =
            walk_forward_windows_from_day_starts(&days, UnixNanos::from(8), settings).unwrap();

        assert_eq!(
            windows,
            vec![
                WalkForwardWindow {
                    train_start: UnixNanos::from(1),
                    test_start: UnixNanos::from(4),
                    test_end: UnixNanos::from(6),
                },
                WalkForwardWindow {
                    train_start: UnixNanos::from(3),
                    test_start: UnixNanos::from(6),
                    test_end: UnixNanos::from(8),
                },
            ],
        );
    }

    #[rstest::rstest]
    fn test_walk_forward_fold_requires_every_acceptance_threshold() {
        let settings = WalkForwardSettings {
            train_days: 3,
            test_days: 2,
            step_days: 2,
            minimum_folds: 1,
            minimum_is_trades: 1,
            minimum_oos_trades: 20,
            minimum_oos_sharpe: 1.0,
            maximum_oos_drawdown_pct: 1.0,
            minimum_pass_rate: 0.5,
        };
        let accepted = BacktestEvaluation {
            trade_direction: TradeDirection::Both,
            risk_reward: Decimal::from(2),
            trades: 20,
            conservative_pnl: Decimal::ONE,
            conservative_cost_adjusted_sharpe: Some(1.0),
            conservative_max_drawdown_pct: Some(-1.0),
            conservative_annualized_return_pct: Some(1.0),
            conservative_calmar: Some(1.0),
            positive_days: 1,
            negative_days: 0,
            flat_days: 0,
            engine_sharpe: Some(1.0),
        };

        assert!(walk_forward_fold_passes(1.0, accepted, settings));
        assert!(!walk_forward_fold_passes(
            1.0,
            BacktestEvaluation {
                conservative_pnl: Decimal::ZERO,
                ..accepted
            },
            settings,
        ));
        assert!(!walk_forward_fold_passes(-0.1, accepted, settings));
    }

    #[rstest::rstest]
    fn test_daily_risk_metrics_include_flat_days_and_peak_to_trough_drawdown() {
        let metrics = risk_metrics_from_daily_pnl(
            &[
                Decimal::from(10),
                Decimal::ZERO,
                Decimal::from(-20),
                Decimal::from(5),
            ],
            Decimal::from(100),
        )
        .unwrap();

        assert_eq!(
            (
                metrics.positive_days,
                metrics.negative_days,
                metrics.flat_days
            ),
            (2, 1, 1)
        );
        assert!(metrics.sharpe.is_some_and(|value| value < 0.0));
        assert!(
            metrics
                .max_drawdown_pct
                .is_some_and(|value| (value + 18.1818).abs() < 0.001)
        );
        assert!(metrics.calmar.is_some_and(|value| value < 0.0));
    }

    #[rstest::rstest]
    fn test_walk_forward_warmup_excludes_split_and_later_bars() {
        let bar_type = AppConfig::five_minute_bar_type(InstrumentId::from("QQQ.US.LONGBRIDGE"));
        let initial = vec![
            five_minute_bar("99", "101", "98", "100", 1),
            five_minute_bar("100", "102", "99", "101", 2),
        ];
        let replay = vec![
            five_minute_bar("101", "103", "100", "102", 3),
            five_minute_bar("102", "104", "101", "103", 4),
            five_minute_bar("103", "105", "102", "104", 5),
        ];

        let warmup = split_warmup_bars(
            &initial,
            &replay,
            UnixNanos::from(5),
            3,
            InstrumentId::from("QQQ.US.LONGBRIDGE"),
            bar_type,
        )
        .unwrap();

        assert_eq!(
            warmup
                .iter()
                .map(|bar| bar.ts_event.as_u64())
                .collect::<Vec<_>>(),
            vec![2, 3, 4],
        );
    }

    #[rstest::rstest]
    fn test_run_statistics_report_exit_and_signal_cohorts() {
        let instrument_id = InstrumentId::from("QQQ.US.LONGBRIDGE");
        let trade = ClosedTradeStatistics {
            side: OrderSide::Buy,
            level: SignalLevel::Reclaimed,
            level_age_bars: 12,
            confirmation_bars: 2,
            confirmation_close_location: 0.75,
            distance_atr: 0.2,
            zone_width_atr: 0.5,
            displacement_strength_atr: 1.5,
            entry_minute: 600,
            holding_bars: 8,
            mfe_r: Decimal::from(2),
            mae_r: Decimal::new(5, 1),
            exit_reason: TradeExitReason::Target,
            realized_pnl: Some(Decimal::from(40)),
            estimated_cost: Decimal::from(1),
            entry_slippage_stress: Decimal::from(2),
            initial_risk: Decimal::from(20),
            risk_utilization: Decimal::new(8, 1),
            r_multiple: Some(Decimal::from(2)),
            close_ts: UnixNanos::from(1),
            ambiguous_exit_bar: false,
        };
        let mut ambiguous = trade;
        ambiguous.ambiguous_exit_bar = true;
        assert_eq!(trade.conservative_pnl(), Some(Decimal::from(37)));
        assert_eq!(ambiguous.conservative_pnl(), Some(Decimal::from(-23)));
        let statistics = RunStatistics {
            symbols: HashMap::from([(
                instrument_id,
                SymbolRunStatistics {
                    funnel: SignalFunnel {
                        signals: 1,
                        ..Default::default()
                    },
                    entries_submitted: 1,
                    risk_rejections: BTreeMap::new(),
                    trades: vec![trade],
                },
            )]),
        };
        let output = statistics.lines().join("\n");

        assert!(output.contains("exit_reason=target"));
        assert!(output.contains("side=BUY, level=reclaimed"));
        assert!(output.contains("realized_pnl=40"));
        assert!(output.contains("cost_adjusted_pnl=39"));
        assert!(output.contains("entry_slippage_stress=2"));
        assert!(output.contains("conservative_pnl=37"));
        assert!(output.contains("average_r=2"));
        assert!(output.contains("average_conservative_r=1.85"));
        assert!(output.contains("average_confirmation_close_location=0.7500"));
        assert!(output.contains("average_mfe_r=2"));
        assert!(output.contains("bucket=10:00"));
    }

    #[rstest::rstest]
    fn test_account_risk_rejection_reports_the_binding_limit() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-rejection-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()).as_u64(),
        ));
        let date = "2026-09-01".parse().unwrap();
        let limits = AccountRiskLimits {
            daily_loss: Decimal::from(50),
            open_risk: Decimal::from(100),
            account_notional: Decimal::from(10_000),
            open_positions: 1,
        };
        let reservation = RiskReservation {
            risk: Decimal::from(25),
            notional: Decimal::from(1_000),
        };
        let risk = AccountRisk::load(path.clone()).unwrap();

        risk.reserve_entry("QQQ.US", date, reservation, 1, limits)
            .unwrap();
        let rejected = risk
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();

        assert_eq!(
            rejected.0,
            ReservationOutcome::Rejected(RiskRejectionReason::OpenPositions),
        );
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    fn test_account_daily_loss_persists_across_restart() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()),
        ));
        let date = "2026-09-01".parse().unwrap();
        let limits = AccountRiskLimits {
            daily_loss: Decimal::from(50),
            open_risk: Decimal::from(100),
            account_notional: Decimal::from(10_000),
            open_positions: 1,
        };
        let reservation = RiskReservation {
            risk: Decimal::from(25),
            notional: Decimal::from(1_000),
        };
        let risk = AccountRisk::load(path.clone()).unwrap();

        assert_eq!(
            risk.reserve_entry("QQQ.US", date, reservation, 1, limits)
                .unwrap()
                .0,
            ReservationOutcome::Reserved,
        );
        assert_eq!(
            risk.reserve_entry("AAPL.US", date, reservation, 1, limits)
                .unwrap()
                .0,
            ReservationOutcome::Rejected(RiskRejectionReason::OpenPositions),
        );
        risk.record_close("QQQ.US", date, Some(Decimal::from(-50)), true)
            .unwrap();
        drop(risk);
        let restored = AccountRisk::load(path.clone()).unwrap();
        let (outcome, snapshot) = restored
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();

        assert_eq!(
            outcome,
            ReservationOutcome::Rejected(RiskRejectionReason::DailyLoss),
        );
        assert_eq!(snapshot.realized_pnl, Decimal::from(-50));
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    fn test_missing_realized_pnl_halts_entries_until_next_session() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-missing-pnl-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()).as_u64(),
        ));
        let date = "2026-09-01".parse().unwrap();
        let next_date = "2026-09-02".parse().unwrap();
        let limits = AccountRiskLimits {
            daily_loss: Decimal::from(50),
            open_risk: Decimal::from(100),
            account_notional: Decimal::from(10_000),
            open_positions: 1,
        };
        let reservation = RiskReservation {
            risk: Decimal::from(25),
            notional: Decimal::from(1_000),
        };
        let risk = AccountRisk::load(path.clone()).unwrap();

        risk.reserve_entry("QQQ.US", date, reservation, 1, limits)
            .unwrap();
        let halted = risk.record_close("QQQ.US", date, None, true).unwrap();
        let same_day = risk
            .reserve_entry("AAPL.US", date, reservation, 1, limits)
            .unwrap();
        let next_day = risk
            .reserve_entry("AAPL.US", next_date, reservation, 1, limits)
            .unwrap();

        assert!(halted.halted);
        assert_eq!(
            same_day.0,
            ReservationOutcome::Rejected(RiskRejectionReason::AccountHalted),
        );
        assert_eq!(next_day.0, ReservationOutcome::Reserved);
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    fn test_unreserved_reconciled_exposure_halts_account() {
        let path = env::temp_dir().join(format!(
            "nautilus-slc-risk-untracked-{}-{}.toml",
            std::process::id(),
            UnixNanos::from(Timestamp::now()).as_u64(),
        ));
        let date = "2026-09-01".parse().unwrap();
        let risk = AccountRisk::load(path.clone()).unwrap();

        let snapshot = risk.reconcile_symbol("QQQ.US", date, true).unwrap();

        assert!(snapshot.halted);
        assert_eq!(snapshot.open_positions, 0);
        let _ = fs::remove_file(path);
    }

    #[rstest::rstest]
    #[case("2026-06-29T14:00:00Z", 16, "2026-06-29T20:00:00Z")]
    #[case("2026-01-16T15:00:00Z", 16, "2026-01-16T21:00:00Z")]
    fn test_us_market_close_handles_dst(
        #[case] now: &str,
        #[case] close_hour: u8,
        #[case] expected: &str,
    ) {
        let now = now.parse::<Timestamp>().unwrap();
        let close_time = Time::from_hms(close_hour, 0, 0).unwrap();

        assert_eq!(
            us_market_close_at(now, close_time).unwrap(),
            expected.parse::<Timestamp>().unwrap(),
        );
    }
}
