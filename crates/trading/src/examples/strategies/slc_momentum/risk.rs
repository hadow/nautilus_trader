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

//! 下单前的仓位计算和组合风险预留。
//!
//! 单笔风险、总风险、行业风险、名义敞口和相关性在一次报价快照上共同取最小容量；
//! 风险在发送入场单前原子预留，在订单/持仓终态后释放，不能用成交后的检查替代。

use std::collections::BTreeMap;

use nautilus_core::UnixNanos;
use nautilus_model::{
    identifiers::InstrumentId,
    instruments::{Instrument, InstrumentAny},
    types::{Money, Price, Quantity},
};
use nautilus_risk::sizing::calculate_fixed_risk_position_size;
use rust_decimal::Decimal;
use serde::Serialize;

use super::{
    ExitConfig, MarketRegime, NoTrade, RiskConfig, SlcMomentumConfig, SlcSignal, TargetMode,
    TradeSide,
};

#[derive(Clone, Debug, Serialize)]
pub struct RiskAllocation {
    pub entry: Price,
    pub stop: Price,
    pub target: Price,
    pub quantity: Quantity,
    pub risk_per_share: Decimal,
    pub reserved_risk: Decimal,
    pub allocated_risk_fraction: Decimal,
    pub notional: Decimal,
}

#[derive(Clone, Debug)]
pub struct RiskHolding {
    pub sector: String,
    pub risk: Decimal,
    pub notional: Decimal,
}

/// Strategy-level reservations supplement, rather than replace, the native risk engine.
#[derive(Debug, Default)]
pub struct PortfolioRiskManager {
    pub holdings: BTreeMap<InstrumentId, RiskHolding>,
    pub(super) session: Option<UnixNanos>,
    pub(super) opening_equity: Decimal,
    pub(super) halted: bool,
    pub(super) consecutive_losses: usize,
}

impl PortfolioRiskManager {
    /// Updates the session loss latch using authoritative mark-to-market account equity.
    pub fn mark_equity(&mut self, session_open: UnixNanos, equity: Decimal, c: &RiskConfig) {
        if self.session != Some(session_open) {
            self.session = Some(session_open);
            self.opening_equity = equity;
            self.halted = false;
            self.consecutive_losses = 0;
        }
        self.halted |= equity <= Decimal::ZERO
            || equity <= self.opening_equity * (Decimal::ONE - c.max_daily_loss);
    }

    /// Records a completed net trade and releases its reservation.
    pub fn closed(&mut self, id: InstrumentId, pnl: Decimal, c: &RiskConfig) {
        self.holdings.remove(&id);
        if pnl < Decimal::ZERO {
            self.consecutive_losses += 1;
        } else if pnl > Decimal::ZERO {
            self.consecutive_losses = 0;
        }
        self.halted |= self.consecutive_losses >= c.max_consecutive_losses;
    }

    /// Reserves risk atomically before an entry is dispatched.
    ///
    /// # Errors
    /// Returns `DuplicateOrder` if this symbol already has committed risk.
    pub fn reserve(
        &mut self,
        id: InstrumentId,
        sector: String,
        allocation: &RiskAllocation,
    ) -> Result<(), NoTrade> {
        if self.holdings.contains_key(&id) {
            return Err(NoTrade::DuplicateOrder);
        }
        self.holdings.insert(
            id,
            RiskHolding {
                sector,
                risk: allocation.reserved_risk,
                notional: allocation.notional,
            },
        );
        Ok(())
    }

    /// Computes exact, lot-rounded quantity with common portfolio and participation caps.
    ///
    /// # Errors
    /// Returns a typed rejection if any risk constraint has no remaining capacity.
    #[expect(
        clippy::too_many_arguments,
        reason = "risk inputs are explicit and captured at one quote"
    )]
    pub fn allocate(
        &self,
        signal: &SlcSignal,
        instrument: &InstrumentAny,
        entry: Price,
        equity: Money,
        free: Decimal,
        correlated: usize,
        c: &SlcMomentumConfig,
        liquidity_volume: Decimal,
        structure_target: Option<Price>,
    ) -> Result<RiskAllocation, NoTrade> {
        let r = &c.risk;
        if !c.directions.contains(&signal.side) {
            return Err(if signal.side == TradeSide::Short {
                NoTrade::ShortDisabled
            } else {
                NoTrade::DirectionDisabled
            });
        }
        if self.halted {
            return Err(NoTrade::MaxDailyLoss);
        }
        if self.holdings.contains_key(&signal.symbol) {
            return Err(NoTrade::DuplicateOrder);
        }
        if self.holdings.len() >= r.max_positions {
            return Err(NoTrade::MaxExposure);
        }
        let sector = self
            .holdings
            .values()
            .filter(|h| h.sector == signal.sector)
            .collect::<Vec<_>>();
        if sector.len() >= r.max_sector_positions || correlated >= r.max_correlated_positions {
            return Err(NoTrade::MaxExposure);
        }
        let amount = equity.as_decimal();
        if amount <= Decimal::ZERO || free <= Decimal::ZERO {
            return Err(NoTrade::RiskTooHigh);
        }
        let side = signal.side;
        let boundary = if side == TradeSide::Long {
            signal.level_low
        } else {
            signal.level_high
        };
        let stop = stop_price(boundary, signal.atr, instrument, r, side)?;
        let distance = side.sign() * (entry.as_decimal() - stop.as_decimal());
        if distance < r.minimum_stop_distance || stop.as_decimal() <= Decimal::ZERO {
            return Err(NoTrade::RiskTooHigh);
        }
        let multiplier = if signal.market_regime == MarketRegime::Neutral && c.ablation.regime() {
            r.neutral_multiplier
        } else {
            Decimal::ONE
        } * if correlated > 0 {
            r.correlation_penalty
        } else {
            Decimal::ONE
        };
        let used_risk = self.holdings.values().map(|h| h.risk).sum::<Decimal>();
        let sector_risk = sector.iter().map(|h| h.risk).sum::<Decimal>();
        let budget = (amount * r.risk_per_trade * multiplier)
            .min(amount * r.max_total_risk - used_risk)
            .min(amount * r.max_sector_risk - sector_risk);
        let used_notional = self.holdings.values().map(|h| h.notional).sum::<Decimal>();
        let sector_notional = sector.iter().map(|h| h.notional).sum::<Decimal>();
        let capital = r
            .max_position_value
            .min(r.max_notional - used_notional)
            .min(amount * r.max_total_exposure - used_notional)
            .min(amount * r.max_sector_exposure - sector_notional)
            .min(free - used_notional);
        if budget <= Decimal::ZERO || capital <= Decimal::ZERO {
            return Err(NoTrade::MaxExposure);
        }
        let price = entry.as_decimal();
        if price <= Decimal::ZERO {
            return Err(NoTrade::RiskTooHigh);
        }
        let risk_per_share = distance
            + price
                * (r.commission_rate * Decimal::from(2)
                    + c.max_slippage_bps / Decimal::from(10_000));
        let margin = if side == TradeSide::Short {
            r.short_margin_ratio
        } else {
            Decimal::ONE
        };
        let cap = (budget / risk_per_share)
            .min(capital / (price * (margin + r.commission_rate)))
            .min(liquidity_volume * r.participation);
        let lot = instrument
            .lot_size()
            .map_or(Decimal::ONE, |q| q.as_decimal())
            .max(Decimal::ONE);
        let hard = (cap / lot).floor() * lot;
        if hard < lot {
            return Err(NoTrade::RiskTooHigh);
        }
        let quantity = calculate_fixed_risk_position_size(
            instrument,
            entry,
            stop,
            equity,
            budget / amount,
            r.commission_rate,
            Decimal::ONE,
            Some(hard),
            lot,
            1,
        )
        .map_err(|_| NoTrade::RiskTooHigh)?;
        if quantity.as_decimal() <= Decimal::ZERO {
            return Err(NoTrade::RiskTooHigh);
        }
        let target = target_price(
            entry,
            stop,
            signal.atr,
            instrument,
            &c.exit,
            structure_target,
            side,
        )?;
        Ok(RiskAllocation {
            entry,
            stop,
            target,
            quantity,
            risk_per_share,
            reserved_risk: risk_per_share * quantity.as_decimal(),
            allocated_risk_fraction: risk_per_share * quantity.as_decimal() / amount,
            notional: price * quantity.as_decimal(),
        })
    }
}

pub(super) fn rounded_price(
    value: Decimal,
    instrument: &InstrumentAny,
    up: bool,
) -> Result<Price, NoTrade> {
    let tick = instrument.price_increment().as_decimal();
    if value <= Decimal::ZERO || tick <= Decimal::ZERO {
        return Err(NoTrade::RiskTooHigh);
    }
    let ticks = if up {
        (value / tick).ceil()
    } else {
        (value / tick).floor()
    };
    instrument
        .try_make_price_from_decimal(ticks * tick)
        .map_err(|_| NoTrade::RiskTooHigh)
}

pub(super) fn stop_price(
    low: Price,
    atr: Decimal,
    instrument: &InstrumentAny,
    c: &RiskConfig,
    side: TradeSide,
) -> Result<Price, NoTrade> {
    if atr <= Decimal::ZERO {
        return Err(NoTrade::RiskTooHigh);
    }
    rounded_price(
        low.as_decimal() - side.sign() * atr * c.stop_buffer_atr,
        instrument,
        side == TradeSide::Short,
    )
}

pub(super) fn target_price(
    entry: Price,
    stop: Price,
    atr: Decimal,
    instrument: &InstrumentAny,
    c: &ExitConfig,
    structure: Option<Price>,
    side: TradeSide,
) -> Result<Price, NoTrade> {
    let risk = side.sign() * (entry.as_decimal() - stop.as_decimal());
    let target = match c.target_mode {
        TargetMode::FixedR => entry.as_decimal() + side.sign() * risk * c.target_r,
        TargetMode::Atr => {
            entry.as_decimal() + side.sign() * (atr * c.atr_multiplier).max(risk * c.target_r)
        }
        TargetMode::Structure => structure
            .filter(|p| side.sign() * (p.as_decimal() - entry.as_decimal()) >= risk * c.target_r)
            .ok_or(NoTrade::LevelInvalid)?
            .as_decimal(),
    };
    if risk <= Decimal::ZERO || side.sign() * (target - entry.as_decimal()) <= Decimal::ZERO {
        return Err(NoTrade::RiskTooHigh);
    }
    rounded_price(target, instrument, side == TradeSide::Long)
}

pub(super) fn trailing_stop(
    old: Price,
    close: Price,
    high: Price,
    atr: Decimal,
    instrument: &InstrumentAny,
    c: &ExitConfig,
    side: TradeSide,
) -> Result<Price, NoTrade> {
    let reference = if c.chandelier { high } else { close };
    let proposed = reference.as_decimal() - side.sign() * atr * c.trailing_atr;
    if side.sign() * (proposed - old.as_decimal()) <= Decimal::ZERO {
        return Ok(old);
    }
    rounded_price(proposed, instrument, side == TradeSide::Short).map(|p| side.tighter(p, old))
}
