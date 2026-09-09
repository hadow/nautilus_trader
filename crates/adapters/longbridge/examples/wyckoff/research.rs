// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautilustrader.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software distributed under the
//  License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND,
//  either express or implied. See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! 运行预先声明的 IS、Validation、OOS、成本压力和消融研究。

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use rust_decimal::Decimal;

use super::{
    backtest::{Sample, SimulationResult, SimulationSpec, simulate},
    data::{PreparedSymbol, ResearchConfig, prepare_data},
    structure::{EntryVariant, StudyLayer},
};

const DEFAULT_CONFIG_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/wyckoff_research.toml",
);

/// 完成第一阶段研究；只有 IS 和 Validation 同时通过门槛后才读取 OOS 绩效
pub(crate) async fn run() -> anyhow::Result<()> {
    let config_path = config_path_from_args()?;
    let config = ResearchConfig::load(&config_path)?;
    println!("Wyckoff research config: {}", config_path.display());
    let prepared = prepare_data(&config).await?;
    let in_sample = Sample {
        label: "IS",
        start: config.in_sample_start,
        end: config.validation_start,
    };
    let validation = Sample {
        label: "VALIDATION",
        start: config.validation_start,
        end: config.out_of_sample_start,
    };
    let out_of_sample = Sample {
        label: "OOS",
        start: config.out_of_sample_start,
        end: config.end,
    };
    let mut report = research_header(&config, &prepared);

    report.push("## Entry comparison".to_string());
    let mut is_results = Vec::new();
    for variant in EntryVariant::ALL {
        let result = simulate(
            &config,
            &prepared,
            in_sample,
            SimulationSpec {
                layer: StudyLayer::OpeningRange,
                variant,
                risk_reward: Decimal::from(2),
                cost_multiple: Decimal::ONE,
            },
        )?;
        record_result(&mut report, &result);
        is_results.push(result);
    }

    report.push("## Ablation study".to_string());
    for layer in StudyLayer::ALL {
        if layer == StudyLayer::OpeningRange {
            continue;
        }
        for variant in EntryVariant::ALL {
            let result = simulate(
                &config,
                &prepared,
                in_sample,
                SimulationSpec {
                    layer,
                    variant,
                    risk_reward: Decimal::from(2),
                    cost_multiple: Decimal::ONE,
                },
            )?;
            record_result(&mut report, &result);
            is_results.push(result);
        }
    }
    if let Some(diagnostic) = best_result(is_results.iter()) {
        record_breakdown(&mut report, "Best IS diagnostic breakdown", diagnostic);
    }
    let eligible_is = is_results
        .iter()
        .filter(|result| passes_development_gate(result, config.minimum_trades))
        .collect::<Vec<_>>();
    if eligible_is.is_empty() {
        report.push("## OOS result".to_string());
        report.push("未运行：没有 Entry 通过预先声明的 IS 样本量、Profit Factor、Expectancy 和 Sharpe 门槛。".to_string());
        report.extend(rejection_tail(
            "IS 阶段没有统计优势，停止增加过滤器或实现实盘 Strategy。",
        ));
        return write_report(report);
    }

    report.push("## Validation".to_string());
    let mut validation_results = Vec::new();
    for result in eligible_is {
        let validation_result = simulate(&config, &prepared, validation, result.spec)?;
        record_result(&mut report, &validation_result);
        validation_results.push(validation_result);
    }
    let minimum_validation_trades = config.minimum_trades.div_ceil(2);
    let Some(entry_winner) = best_result(
        validation_results
            .iter()
            .filter(|result| passes_development_gate(result, minimum_validation_trades)),
    ) else {
        report.push("## OOS result".to_string());
        report.push("未运行：IS 候选在独立 Validation 上全部失效。".to_string());
        report.extend(rejection_tail("Validation 未确认结构性 Alpha，停止开发。"));
        return write_report(report);
    };
    let selected_variant = entry_winner.spec.variant;

    report.push("## Fixed-R exit comparison".to_string());
    let mut reward_validation = Vec::new();
    for risk_reward in [Decimal::ONE, Decimal::from(2), Decimal::from(3)] {
        let spec = SimulationSpec {
            layer: entry_winner.spec.layer,
            variant: selected_variant,
            risk_reward,
            cost_multiple: Decimal::ONE,
        };
        let is_result = simulate(&config, &prepared, in_sample, spec)?;
        record_result(&mut report, &is_result);
        if passes_development_gate(&is_result, config.minimum_trades) {
            let validation_result = simulate(&config, &prepared, validation, spec)?;
            record_result(&mut report, &validation_result);
            reward_validation.push(validation_result);
        }
    }
    let Some(final_winner) = best_result(
        reward_validation
            .iter()
            .filter(|result| passes_development_gate(result, minimum_validation_trades)),
    ) else {
        report.push("## OOS result".to_string());
        report.push("未运行：没有 Fixed-R 退出同时通过 IS 与 Validation。".to_string());
        report.extend(rejection_tail("退出参数不稳定，停止开发。"));
        return write_report(report);
    };

    report.push("## OOS result".to_string());
    let mut oos_results = Vec::new();
    for cost_multiple in [Decimal::ONE, Decimal::from(2), Decimal::from(3)] {
        let result = simulate(
            &config,
            &prepared,
            out_of_sample,
            SimulationSpec {
                cost_multiple,
                ..final_winner.spec
            },
        )?;
        record_result(&mut report, &result);
        oos_results.push(result);
    }
    let normal_cost = &oos_results[0];
    record_breakdown(&mut report, "OOS breakdown at normal costs", normal_cost);

    let worth_continuing = passes_development_gate(normal_cost, config.minimum_trades)
        && oos_results[2].metrics.total_pnl > Decimal::ZERO;
    report.push("## Final verdict".to_string());
    if worth_continuing {
        report.push("值得进入模拟盘工程阶段：正常成本 OOS 通过门槛，且 3x 成本后仍保持正收益。正式实盘 Strategy 仍需独立实现和审查。".to_string());
    } else {
        report.push("不值得继续开发：OOS 未通过完整门槛，或 3x 成本压力后收益转负。按研究约束停止增加指标和实现实盘 Strategy。".to_string());
    }
    report.push("## Failure cases".to_string());
    report.push("- 5 分钟 OHLC 无法重建盘口队列、真实 spread、停牌后的流动性和盘中先后路径；同 Bar 双触发按止损处理。".to_string());
    report.push("- Longbridge Intraday 历史接口只提供常规交易时段，本轮无法验证 Premarket High/Low、盘前量和新闻跳空过滤。".to_string());
    report.push(
        "- 未复权历史数据可能受公司行动影响；本轮标的和时间段仍需人工核对拆股与异常 Bar。"
            .to_string(),
    );
    report.push("## Overfitting risk".to_string());
    report.push(
        "- 只选择 Entry 类型与 1R/2R/3R，未搜索连续小数阈值；OOS 在选择完成后才运行。".to_string(),
    );
    report.push(
        "- 五种 Entry、六层消融和三个目标仍构成多重比较；单次 OOS 通过也不能证明未来收益。"
            .to_string(),
    );
    write_report(report)
}

fn research_header(config: &ResearchConfig, prepared: &[PreparedSymbol]) -> Vec<String> {
    vec![
        "# Wyckoff intraday research report".to_string(),
        String::new(),
        "## Strategy thesis".to_string(),
        "供需失衡只有在价格离开平衡区、努力与结果匹配，并在回踩时出现供应/需求收缩后才可能延续；Spring 则要求越界后在同一根已完成 Bar 内收复。收益假设来自错误突破者止损与突破后剩余订单流，而不是形态名称。".to_string(),
        "## Feature definitions".to_string(),
        "- ATR：5 分钟 Wilder ATR(14)，只在当前 Bar close 后更新。".to_string(),
        "- Trading Range：当前 Bar 之前同日 12 根 Bar 的最高/最低；宽度 1.5-5 ATR，且上下边界各至少两次进入 0.2 ATR 邻域。".to_string(),
        "- RVOL：当前 5 分钟时槽成交量 / 过去最多 20 个交易日同一时槽成交量中位数，至少 5 个历史样本。".to_string(),
        "- Effort/Result：RVOL / max((High-Low)/ATR, 0.10)。吸收要求 RVOL>=1.25、Range<=0.8 ATR，并收在 Bar 顶部/底部 35%。".to_string(),
        "- VWAP：当日典型价格 (H+L+C)/3 的成交量加权均价，包含当前已完成 Bar。".to_string(),
        "- Regime：24 根 Bar 的 |净位移|/路径长度；>=0.35 为 Trending，<=0.20 为 Ranging。方向还需净位移 >=1 ATR 且位于 VWAP 同侧。".to_string(),
        "- Volatility：ATR/Close 相对过去 50 根自身中位数；>=1.5x 为 High，<=0.7x 为 Low。High 风险减半，Low 上限为 1.25 倍且完整过滤层禁止开仓。".to_string(),
        "## Signal definitions".to_string(),
        "- Spring immediate：Low 越过前序 Range Low 0.05-0.50 ATR，Close 收回 Range，收盘位置>=0.65；空头反向。".to_string(),
        "- Spring test：Spring 后 4 根内形成不创新低/高的测试，收回边界，完整过滤层要求成交量<=Spring 的 0.8。".to_string(),
        "- SOS breakout：Close 越过边界>=0.10 ATR，Range>=1 ATR，Body>=0.60 ATR，收盘位于 Bar 顺势末端 30%。".to_string(),
        "- SOS+LPS：SOS 后 6 根内回踩边界 0.40 ATR 邻域但未收回 Range，Bar<=1 ATR，完整过滤层要求成交量<=SOS 的 0.8。".to_string(),
        "- LPS confirmation：LPS 后 2 根内顺势突破 LPS 极端，Body>=0.25 ATR，收盘位于顺势末端 40%。".to_string(),
        "- 所有信号在 close 确认，下一根 5 分钟 Bar open 执行；不存在 pivot 右侧回写、未来 Range 或当根收盘价成交。".to_string(),
        "## Entry pseudocode".to_string(),
        "IF 完成 Bar 形成指定 Spring/SOS/LPS 结构 AND 当前研究层全部过滤成立 AND 10:00<=下一 Bar 时间<=15:00 AND 未触发日亏损/仓位/频次限制 THEN 下一根 Bar 开盘按市价入场。".to_string(),
        "## Exit rules".to_string(),
        "初始止损=结构极端外 0.10 ATR，且距入场至少 0.25 ATR；目标=入场价±N×每股风险；15:50 强平。同一 5 分钟 Bar 同时触及止损和目标时按止损先发生；本轮只用市价单，不假设限价单成交。".to_string(),
        "## Risk and execution".to_string(),
        format!(
            "单笔风险={} USD，日亏损停止线={} USD，最多同时持仓={}，每日最多交易={}，每标的每日最多={}；结构止损外加 {} ATR，并保证至少 0.25 ATR 距离。",
            config.risk_amount,
            config.daily_loss_limit,
            config.max_open_positions,
            config.max_trades_per_day,
            config.max_trades_per_symbol,
            config.stop_buffer_atr,
        ),
        format!(
            "历史标的={}，数据源=Longbridge 未复权 RTH 5m，IS={}..{}，Validation={}..{}，OOS={}..{}。",
            prepared.len(),
            config.in_sample_start,
            config.validation_start,
            config.validation_start,
            config.out_of_sample_start,
            config.out_of_sample_start,
            config.end,
        ),
        "## Market regime policy".to_string(),
        "Trending Bullish/Bearish 只允许同向 SOS/LPS；Ranging 允许 Spring；Transition 仅允许不逆强趋势的 Spring；Low Volatility、Range 不明确、VWAP/方向冲突、时间窗外、达到日亏损或仓位上限均不交易。".to_string(),
        "## Research scope".to_string(),
        "第一版只使用 RTH 5 分钟 Bar，避免在核心假设尚未证明前加入多周期、盘前、Volume Profile 或相关市场特征。Longbridge 交易日历中的半日市预先排除；数据断档发生后，该标的当日停止产生新信号。".to_string(),
        "Walk-forward 未预先启用：扣除 RVOL 预热后独立样本不足一年；只有 IS 与 Validation 均通过时才值得延长历史后新增滚动窗口。".to_string(),
    ]
}

fn record_result(report: &mut Vec<String>, result: &SimulationResult) {
    let line = result.summary();
    println!("{line}");
    report.push(format!("- {line}"));
}

fn record_breakdown(report: &mut Vec<String>, title: &str, result: &SimulationResult) {
    report.push(format!("### {title}"));
    for (month, pnl) in result.monthly_returns() {
        report.push(format!("- month={month}: net_pnl={pnl} USD"));
    }
    report.extend(
        result
            .cohort_lines()
            .into_iter()
            .map(|line| format!("- {line}")),
    );
}

fn passes_development_gate(result: &SimulationResult, minimum_trades: usize) -> bool {
    result.metrics.trades >= minimum_trades
        && result.metrics.total_pnl > Decimal::ZERO
        && result.metrics.expectancy.is_some_and(|value| value > 0.0)
        && result
            .metrics
            .profit_factor
            .is_some_and(|value| value > 1.0)
        && result.metrics.sharpe.is_some_and(|value| value > 0.0)
}

fn best_result<'a>(
    results: impl Iterator<Item = &'a SimulationResult>,
) -> Option<&'a SimulationResult> {
    results.max_by(|left, right| {
        left.metrics
            .sharpe
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&right.metrics.sharpe.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| left.metrics.total_pnl.cmp(&right.metrics.total_pnl))
    })
}

fn rejection_tail(reason: &str) -> Vec<String> {
    vec![
        "## Failure cases".to_string(),
        format!("- {reason}"),
        "- 未进入 OOS 不是缺失成功案例，而是防止在看见 OOS 后继续调整规则。".to_string(),
        "## Overfitting risk".to_string(),
        "- 若继续修改阈值来挽救本次样本，将把 Validation/OOS 变成训练集。".to_string(),
        "## Final verdict".to_string(),
        "不值得继续开发当前假设。".to_string(),
    ]
}

fn write_report(mut lines: Vec<String>) -> anyhow::Result<()> {
    lines.push(String::new());
    let output = lines.join("\n");
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/wyckoff-research-report.md");
    fs::write(&path, &output)
        .with_context(|| format!("failed to write research report {}", path.display()))?;
    println!("Wyckoff research report written to {}", path.display());
    Ok(())
}

fn config_path_from_args() -> anyhow::Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    anyhow::ensure!(args.next().is_none(), "expected at most one config path");
    Ok(path)
}
