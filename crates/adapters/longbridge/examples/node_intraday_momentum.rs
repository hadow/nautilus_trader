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

//! Longbridge paper/live runner for the intraday noise-area momentum strategy.
//!
//! Paper trading is the default. Live routing requires both `papertrading = false` and `--live`.

mod intraday_calendar;

use intraday_calendar::calendar;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::Path,
    time::Duration,
};

use anyhow::Context;
use jiff::Timestamp;
use longbridge::quote::{AdjustType, Period, QuoteContext, TradeSessions};
use nautilus_common::{enums::Environment, live::get_runtime};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_live::{
    config::{LiveExecEngineConfig, LiveRiskEngineConfig},
    node::LiveNode,
};
use nautilus_longbridge::{
    LongbridgeDataClientConfig, LongbridgeDataClientFactory, LongbridgeExecClientConfig,
    LongbridgeExecutionClientFactory,
    common::{
        parse::parse_bar_with_price_precision,
        rate_limit::{history_api_call_with_retry, quote_api_call_with_retry},
    },
};
use nautilus_model::{
    data::{Bar, BarType},
    enums::{AccountType, OmsType},
    identifiers::{AccountId, InstrumentId, StrategyId, TraderId},
    types::Price,
};
use nautilus_trading::examples::strategies::{
    IntradayMomentumConfig, IntradayMomentumReport, IntradayMomentumSession,
    IntradayMomentumStrategy,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use time::{Date, Month};

const DEFAULT_CONFIG_PATH: &str = "crates/adapters/longbridge/examples/intraday_momentum_live.toml";
const MINUTE: u64 = 60_000_000_000;
const US_TIMEZONE: &str = "America/New_York";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppConfig {
    trader_id: String,
    account_id: String,
    node_name: String,
    papertrading: bool,
    max_notional_per_order: Decimal,
    warmup_batches: usize,
    instruments: Vec<InstrumentId>,
    dividends: HashMap<String, HashMap<String, Decimal>>,
    instrument_price_increments: HashMap<String, String>,
    strategy: IntradayMomentumConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            trader_id: "INTRADAY-MOMENTUM-001".to_string(),
            account_id: "LONGBRIDGE-001".to_string(),
            node_name: "LONGBRIDGE-INTRADAY-MOMENTUM-001".to_string(),
            papertrading: true,
            max_notional_per_order: Decimal::from(400_000),
            warmup_batches: 8,
            instruments: vec![InstrumentId::from("SPY.US.LONGBRIDGE")],
            dividends: HashMap::new(),
            instrument_price_increments: HashMap::new(),
            strategy: IntradayMomentumConfig::default(),
        }
    }
}

impl AppConfig {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading live config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("invalid live config {}", path.display()))
    }

    fn data_config(&self) -> LongbridgeDataClientConfig {
        LongbridgeDataClientConfig {
            instrument_price_increments: self.instrument_price_increments.clone(),
            ..Default::default()
        }
    }

    fn strategy_configs(
        &self,
        sessions: &[IntradayMomentumSession],
    ) -> anyhow::Result<Vec<IntradayMomentumConfig>> {
        anyhow::ensure!(
            !self.instruments.is_empty(),
            "at least one instrument is required"
        );
        anyhow::ensure!(
            self.max_notional_per_order > Decimal::ZERO,
            "max_notional_per_order must be positive",
        );
        anyhow::ensure!(
            (1..=20).contains(&self.warmup_batches),
            "warmup_batches must be in [1, 20]",
        );
        anyhow::ensure!(
            self.strategy.flatten_before_close_minutes > 0,
            "Longbridge live routing must flatten before the closing auction",
        );
        self.data_config().validate()?;
        let unique = self.instruments.iter().copied().collect::<BTreeSet<_>>();
        anyhow::ensure!(
            unique.len() == self.instruments.len(),
            "strategy instruments must be unique",
        );
        let allocated =
            self.strategy.capital_fraction * Decimal::from(u64::try_from(self.instruments.len())?);
        anyhow::ensure!(
            allocated <= Decimal::ONE,
            "combined capital_fraction is {allocated}; it must not exceed 1",
        );

        let timezone = get_timezone(US_TIMEZONE)?;
        self.instruments
            .iter()
            .copied()
            .enumerate()
            .map(|(index, instrument_id)| {
                anyhow::ensure!(
                    instrument_id.venue.as_str() == "LONGBRIDGE"
                        && instrument_id.symbol.as_str().ends_with(".US"),
                    "strategy instrument {instrument_id} must be a US Longbridge instrument",
                );
                anyhow::ensure!(
                    self.instrument_price_increments
                        .contains_key(&instrument_id.to_string()),
                    "missing exact price increment for {instrument_id}",
                );
                let dividends = self.dividends.get(&instrument_id.to_string());
                let sessions = sessions
                    .iter()
                    .map(|session| {
                        let date = Timestamp::from_nanosecond(i128::from(session.open.as_u64()))?
                            .to_zoned(timezone.clone())
                            .date()
                            .to_string();
                        Ok(IntradayMomentumSession {
                            dividend: dividends
                                .and_then(|values| values.get(&date))
                                .copied()
                                .unwrap_or_default(),
                            ..*session
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let mut config = self.strategy.clone();
                config.instrument_id = instrument_id;
                config.sessions = sessions;
                config.bars_are_final = false;
                config.base.strategy_id = Some(StrategyId::from(format!(
                    "INTRADAY-MOMENTUM-{:03}",
                    index + 1,
                )));
                config.base.order_id_tag = Some(format!("{:03}", 805 + index));
                config.base.oms_type = Some(OmsType::Netting);
                config.base.external_order_claims = Some(vec![instrument_id]);
                config.base.market_exit_reduce_only = false;
                config.validate()?;
                Ok(config)
            })
            .collect()
    }
}

async fn warmup(
    context: &QuoteContext,
    app: &AppConfig,
    config: &IntradayMomentumConfig,
    now: UnixNanos,
) -> anyhow::Result<Vec<Bar>> {
    let instrument_id = config.instrument_id;
    let price_increment: Price = app.instrument_price_increments[&instrument_id.to_string()]
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid price increment: {e}"))?;
    let bar_type: BarType = format!("{instrument_id}-1-MINUTE-LAST-EXTERNAL").parse()?;
    let mut bars = Vec::new();
    let mut before = None;
    for _ in 0..app.warmup_batches {
        let rows = history_api_call_with_retry(|| {
            context.history_candlesticks_by_offset(
                instrument_id.symbol.as_str(),
                Period::OneMinute,
                AdjustType::NoAdjust,
                false,
                before,
                1_000,
                TradeSessions::Intraday,
            )
        })
        .await?;
        if rows.is_empty() {
            break;
        }
        let oldest = rows
            .iter()
            .map(|row| row.timestamp)
            .min()
            .context("Longbridge history page contained no timestamp")?;
        let local = Timestamp::from_nanosecond(oldest.unix_timestamp_nanos())?
            .to_zoned(get_timezone(US_TIMEZONE)?)
            .checked_sub(jiff::SignedDuration::from_mins(1))?;
        before = Some(
            Date::from_calendar_date(
                i32::from(local.year()),
                Month::try_from(u8::try_from(local.month())?)?,
                u8::try_from(local.day())?,
            )?
            .with_time(time::Time::from_hms(
                u8::try_from(local.hour())?,
                u8::try_from(local.minute())?,
                u8::try_from(local.second())?,
            )?),
        );
        let observed: UnixNanos = Timestamp::now().into();
        bars.extend(
            rows.into_iter()
                .map(|row| {
                    parse_bar_with_price_precision(
                        bar_type,
                        row,
                        observed,
                        price_increment.precision,
                    )
                    .map(|raw| {
                        let end = UnixNanos::from(raw.ts_event.as_u64() + MINUTE);
                        Bar::new(
                            raw.bar_type,
                            raw.open,
                            raw.high,
                            raw.low,
                            raw.close,
                            raw.volume,
                            end,
                            observed,
                        )
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
    }
    bars.sort_unstable_by_key(|bar| bar.ts_event);
    bars.dedup_by_key(|bar| bar.ts_event);

    let mut complete = Vec::new();
    let required = config.lookback_days + 1;
    let mut sessions = config
        .sessions
        .iter()
        .filter(|session| session.close < now)
        .rev()
        .take(required)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        sessions.len() == required,
        "calendar returned {} completed sessions; need {required}",
        sessions.len(),
    );
    sessions.reverse();
    for session in sessions {
        let session_bars = bars
            .iter()
            .copied()
            .filter(|bar| session.open < bar.ts_event && bar.ts_event <= session.close)
            .collect::<Vec<_>>();
        let is_complete = session_bars
            .first()
            .is_some_and(|bar| bar.ts_event.as_u64() == session.open.as_u64() + MINUTE)
            && session_bars
                .last()
                .is_some_and(|bar| bar.ts_event == session.close);
        anyhow::ensure!(
            is_complete,
            "Longbridge warmup is incomplete for session {}",
            session.open,
        );
        complete.extend(session_bars);
    }
    if let Some(session) = config
        .sessions
        .iter()
        .find(|session| session.open < now && now < session.close)
    {
        complete.extend(
            bars.iter()
                .copied()
                .filter(|bar| session.open < bar.ts_event && bar.ts_event <= now),
        );
    }
    Ok(complete)
}

fn schedule_stop(node: &LiveNode, close: UnixNanos) -> anyhow::Result<()> {
    let close = Timestamp::from_nanosecond(i128::from(close.as_u64()))?;
    let delay =
        Duration::try_from(Timestamp::now().duration_until(close))? + Duration::from_secs(5);
    let handle = node.handle();
    get_runtime().spawn(async move {
        tokio::time::sleep(delay).await;
        handle.stop();
    });
    Ok(())
}

async fn run(path: &Path, live_acknowledged: bool) -> anyhow::Result<()> {
    let app = AppConfig::load(path)?;
    anyhow::ensure!(
        app.papertrading || live_acknowledged,
        "papertrading=false routes real capital; rerun with --live after reviewing the configuration",
    );
    anyhow::ensure!(
        !app.papertrading || !live_acknowledged,
        "--live was supplied while papertrading=true; choose one routing mode explicitly",
    );
    let data_config = app.data_config();
    let (context, mut receiver) = QuoteContext::new(data_config.sdk_config().await?);
    let now = Timestamp::now();
    let sessions = calendar(&context, now, 100).await?;
    let now_ns: UnixNanos = now.into();
    let current = sessions
        .iter()
        .copied()
        .find(|session| now_ns < session.close)
        .context("no current or upcoming US regular session in the calendar")?;
    let configs = app.strategy_configs(&sessions)?;
    let mut strategies = Vec::with_capacity(configs.len());
    let mut reports = Vec::with_capacity(configs.len());
    for config in configs {
        let history = warmup(&context, &app, &config, now_ns)
            .await
            .with_context(|| format!("failed warming {}", config.instrument_id))?;
        let mut strategy = IntradayMomentumStrategy::new(config.clone())?;
        reports.push((config.instrument_id, strategy.report_handle()));
        strategy.warmup(history)?;
        strategies.push(strategy);
    }
    drop(context);
    tokio::time::timeout(Duration::from_secs(10), async {
        while receiver.recv().await.is_some() {}
    })
    .await
    .context("warmup quote connection did not shut down; refusing a second context")?;

    let trader_id = TraderId::from(app.trader_id.as_str());
    let account_id = AccountId::from(app.account_id.as_str());
    let environment = if app.papertrading {
        Environment::Sandbox
    } else {
        Environment::Live
    };
    let instrument_ids = app
        .instruments
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let order_rate = format!("{}/00:00:01", app.instruments.len().max(4));
    let mut node = LiveNode::builder(trader_id, environment)?
        .with_name(app.node_name.clone())
        .with_load_state(false)
        .with_save_state(false)
        .with_reconciliation(true)
        .with_exec_engine_config(LiveExecEngineConfig {
            reconciliation_lookback_mins: Some(60 * 24),
            reconciliation_instrument_ids: Some(instrument_ids.clone()),
            open_check_interval_secs: Some(10.0),
            position_check_interval_secs: Some(30.0),
            ..Default::default()
        })
        .with_risk_engine_config(LiveRiskEngineConfig {
            bypass: false,
            max_order_submit_rate: order_rate.clone(),
            max_order_modify_rate: order_rate,
            max_notional_per_order: instrument_ids
                .iter()
                .map(|instrument_id| {
                    (
                        instrument_id.clone(),
                        app.max_notional_per_order.to_string(),
                    )
                })
                .collect(),
            ..Default::default()
        })
        .with_delay_post_stop_secs(10)
        .add_data_client(
            None,
            Box::new(LongbridgeDataClientFactory::new()),
            Box::new(data_config),
        )?
        .add_exec_client(
            None,
            Box::new(LongbridgeExecutionClientFactory::new(trader_id, account_id)),
            Box::new(LongbridgeExecClientConfig {
                account_type: AccountType::Margin,
                papertrading: app.papertrading,
                outside_rth: false,
                ..Default::default()
            }),
        )?
        .build()?;
    for strategy in strategies {
        node.add_strategy(strategy)?;
    }
    schedule_stop(&node, current.close)?;
    node.run().await?;

    let report_path = path.with_extension(format!("{}.report.json", Timestamp::now().as_second()));
    let reports = reports
        .into_iter()
        .map(|(instrument_id, report)| {
            let report = report
                .lock()
                .map_err(|_| anyhow::anyhow!("intraday momentum report lock poisoned"))?
                .clone();
            Ok((instrument_id.to_string(), report))
        })
        .collect::<anyhow::Result<BTreeMap<String, IntradayMomentumReport>>>()?;
    fs::write(&report_path, serde_json::to_vec_pretty(&reports)?)?;
    println!("Intraday momentum report: {}", report_path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nautilus_common::logging::ensure_logging_initialized();
    let args = env::args().skip(1).collect::<Vec<_>>();
    let live_acknowledged = args.iter().any(|arg| arg == "--live");
    let path = args
        .iter()
        .find(|arg| arg.as_str() != "--live")
        .map_or_else(|| Path::new(DEFAULT_CONFIG_PATH), Path::new);
    run(path, live_acknowledged).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    fn example_configuration_loads_relative_volume_extension() {
        let config: AppConfig =
            toml::from_str(include_str!("intraday_momentum_live.toml")).unwrap();

        assert!(!config.papertrading);
        assert_eq!(
            config.instruments,
            ["SPY", "TTWO", "APP", "NVDA", "MCHP", "BA", "U", "NFLX"]
                .map(|symbol| InstrumentId::from(format!("{symbol}.US.LONGBRIDGE"))),
        );
        assert_eq!(config.strategy.lookback_days, 14);
        assert_eq!(
            config.strategy.relative_volume_threshold,
            Some(Decimal::ONE)
        );
        assert_eq!(config.strategy.capital_fraction, Decimal::new(125, 3));
        assert_eq!(config.strategy.flatten_before_close_minutes, 1);

        let strategies = config
            .strategy_configs(&[IntradayMomentumSession {
                open: UnixNanos::from(1_000_000_000_000),
                close: UnixNanos::from(1_000_000_000_000 + 390 * MINUTE),
                dividend: Decimal::ZERO,
            }])
            .unwrap();
        assert_eq!(strategies.len(), 8);
        assert_eq!(
            strategies[7].instrument_id,
            InstrumentId::from("NFLX.US.LONGBRIDGE")
        );
    }
}
