use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rusty_poly_signal_runner::recorder_metrics::{
    OutcomeMetrics, SessionAnalyzer, SessionMetricContext, SessionMetricsRecord,
};
use rusty_poly_signal_runner::trajectory::{
    finalize_trajectory_sync, load_trajectory_index, open_trajectory_reader,
    recover_trajectory_index_record, trajectory_path, upsert_trajectory_index, verify_trajectory,
    TrajectoryIndexRecord, TrajectoryMetadata,
};
use rusty_poly_signal_runner::trajectory_analysis::ExitCostAssumptions;
use rusty_poly_signal_runner::trajectory_reports::{
    build_trajectory_reports, ReportOutcome, ReportSession, ReportSettings, ReportSignal,
};

const FIXED_LIMIT_PRICE: f64 = 0.50;
const MINIMUM_SHARES: f64 = 5.0;
const STRATEGY_NAMES: [&str; 4] = ["boll_fade", "streak_rsi", "trio_vote2", "reversal_pro"];

#[derive(Debug)]
struct Cli {
    logs_dir: PathBuf,
    command: String,
    confirm: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionSummary {
    session_id: String,
    market_slot: String,
    entry_time_ms: i64,
    slug: String,
    up_token_id: String,
    down_token_id: String,
    #[serde(default)]
    signal_ids: Vec<String>,
    resolution: Option<Resolution>,
    raw_stream_path: Option<String>,
    #[serde(default)]
    trajectory_path: Option<String>,
    #[serde(default)]
    trajectory_sha256: Option<String>,
    #[serde(default)]
    trajectory_observation_count: Option<u64>,
    #[serde(default)]
    trajectory_compressed_bytes: Option<u64>,
    #[serde(default)]
    trajectory_uncompressed_bytes: Option<u64>,
    #[serde(default)]
    trajectory_finalized_at: Option<DateTime<Utc>>,
    #[serde(default)]
    completion_status: String,
    #[serde(default)]
    gap_count: u64,
    #[serde(default)]
    reconnect_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Resolution {
    winning_asset_id: Option<String>,
    winning_outcome: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SignalRecord {
    signal_id: String,
    session_id: Option<String>,
    strategy: String,
    market_slot: String,
    prediction: String,
    detected_at_local: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
struct SizingRecord {
    signal_id: String,
    disposition: String,
    details: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamEnvelope {
    received_at_unix_ms: i64,
    event_type: String,
    payload: Value,
}

#[derive(Debug, Default, Clone, Serialize)]
struct Aggregate {
    signals: u64,
    order_candidates: u64,
    immediate_fak_fills: u64,
    resting_limit_fills: u64,
    not_filled: u64,
    wins: u64,
    losses: u64,
    pending: u64,
    data_gap_records: u64,
    pnl_usdc: f64,
    fill_delay_ms_total: i64,
    fill_delay_count: u64,
}

impl Aggregate {
    fn add_outcome(
        &mut self,
        outcome: &rusty_poly_signal_runner::recorder_metrics::OutcomeMetrics,
        has_data_gaps: bool,
    ) {
        self.signals += 1;
        if has_data_gaps {
            self.data_gap_records += 1;
        }
        let Some(candidate) = outcome.order_candidate.as_ref() else {
            return;
        };
        self.order_candidates += 1;
        if candidate.immediate_fak_fillable {
            self.immediate_fak_fills += 1;
        }
        if let Some(fill) = candidate.first_fully_fillable.as_ref() {
            self.resting_limit_fills += 1;
            self.fill_delay_ms_total += fill.elapsed_from_signal_ms;
            self.fill_delay_count += 1;
        } else {
            self.not_filled += 1;
        }
        match outcome.order_fill_result.as_str() {
            "WIN" => self.wins += 1,
            "LOSS" => self.losses += 1,
            "PENDING" => self.pending += 1,
            _ => {}
        }
        self.pnl_usdc += outcome.order_fill_pnl_usdc.unwrap_or(0.0);
    }

    fn win_rate_pct(&self) -> f64 {
        let resolved = self.wins + self.losses;
        if resolved == 0 {
            0.0
        } else {
            self.wins as f64 * 100.0 / resolved as f64
        }
    }

    fn fill_rate_pct(&self) -> f64 {
        if self.order_candidates == 0 {
            0.0
        } else {
            self.resting_limit_fills as f64 * 100.0 / self.order_candidates as f64
        }
    }

    fn immediate_rate_pct(&self) -> f64 {
        if self.order_candidates == 0 {
            0.0
        } else {
            self.immediate_fak_fills as f64 * 100.0 / self.order_candidates as f64
        }
    }

    fn ev_usdc(&self) -> f64 {
        if self.order_candidates == 0 {
            0.0
        } else {
            self.pnl_usdc / self.order_candidates as f64
        }
    }

    fn average_fill_seconds(&self) -> f64 {
        if self.fill_delay_count == 0 {
            0.0
        } else {
            self.fill_delay_ms_total as f64 / self.fill_delay_count as f64 / 1_000.0
        }
    }
}

#[derive(Debug, Serialize)]
struct ReportRow {
    scope: String,
    strategy: String,
    market: String,
    #[serde(flatten)]
    aggregate: Aggregate,
    fill_rate_pct: f64,
    immediate_rate_pct: f64,
    win_rate_pct: f64,
    ev_usdc_per_candidate: f64,
    average_fill_seconds: f64,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
struct MinimalStats {
    total_signals: u64,
    trades_below_0_50: u64,
    wins_below_0_50: u64,
    losses_below_0_50: u64,
    missed_wins_no_below_0_50: u64,
    missed_losses_no_below_0_50: u64,
}

impl MinimalStats {
    fn add_outcome(&mut self, outcome: &OutcomeMetrics) {
        self.total_signals += 1;
        let minimum_best_ask = outcome.min_best_ask.as_ref().map(|minimum| minimum.value);
        let crossed_below = is_strictly_below_0_50(minimum_best_ask);
        if crossed_below {
            self.trades_below_0_50 += 1;
        }
        match (crossed_below, outcome.winning_outcome) {
            (true, Some(true)) => self.wins_below_0_50 += 1,
            (true, Some(false)) => self.losses_below_0_50 += 1,
            (false, Some(true)) => self.missed_wins_no_below_0_50 += 1,
            (false, Some(false)) => self.missed_losses_no_below_0_50 += 1,
            (_, None) => {}
        }
    }
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
struct MajorityStats {
    #[serde(flatten)]
    stats: MinimalStats,
    trades_ignored_tie: u64,
}

struct MinimalReports {
    global_all_signals: MinimalStats,
    global_majority: MajorityStats,
    by_strategy: BTreeMap<String, MinimalStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MajorityVote {
    Up,
    Down,
    Tie,
    NoSignals,
}

fn is_strictly_below_0_50(minimum_best_ask: Option<f64>) -> bool {
    minimum_best_ask.is_some_and(|price| price < FIXED_LIMIT_PRICE)
}

fn majority_vote(up_votes: usize, down_votes: usize) -> MajorityVote {
    if up_votes + down_votes == 0 {
        return MajorityVote::NoSignals;
    }
    match up_votes.cmp(&down_votes) {
        std::cmp::Ordering::Greater => MajorityVote::Up,
        std::cmp::Ordering::Less => MajorityVote::Down,
        std::cmp::Ordering::Equal => MajorityVote::Tie,
    }
}

fn main() -> Result<()> {
    let cli = parse_cli()?;
    match cli.command.as_str() {
        "backfill" => backfill(&cli.logs_dir),
        "report" => report(&cli.logs_dir),
        "purge" => purge(&cli.logs_dir, cli.confirm),
        "verify" => verify(&cli.logs_dir),
        "repair-index" => repair_trajectory_index(&cli.logs_dir),
        "all" => {
            backfill(&cli.logs_dir)?;
            if parse_env_bool("PORTFOLIO_RECORDER_PRESERVE_TRAJECTORIES", false)? {
                repair_trajectory_index(&cli.logs_dir)?;
            }
            report(&cli.logs_dir)?;
            purge(&cli.logs_dir, cli.confirm)?;
            verify(&cli.logs_dir)
        }
        command => Err(anyhow!(
            "commande inconnue '{command}'; utilisez backfill, report, purge, verify, repair-index ou all"
        )),
    }
}

fn parse_cli() -> Result<Cli> {
    let mut logs_dir = PathBuf::from("logs/meche050-forward");
    let mut command = None;
    let mut confirm = false;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--logs-dir" => {
                logs_dir = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--logs-dir requiert un chemin"))?,
                );
            }
            "--confirm" => confirm = true,
            value if value.starts_with('-') => {
                return Err(anyhow!("option inconnue: {value}"));
            }
            value if command.is_none() => command = Some(value.to_string()),
            value => return Err(anyhow!("argument inattendu: {value}")),
        }
    }
    Ok(Cli {
        logs_dir,
        command: command.unwrap_or_else(|| "report".to_string()),
        confirm,
    })
}

fn backfill(logs_dir: &Path) -> Result<()> {
    let sessions = load_sessions(&logs_dir.join("sessions.jsonl"))?;
    let signals = load_signals(&logs_dir.join("signals.jsonl"))?;
    let sizing = load_sizing(&logs_dir.join("signal_sizing.jsonl"))?;
    let metrics_path = logs_dir.join("session_metrics.jsonl");
    let mut completed = load_metrics(&metrics_path)?
        .into_iter()
        .filter(|metric| metric.analysis_complete)
        .map(|metric| metric.session_id)
        .collect::<BTreeSet<_>>();
    let signals_by_session = group_signals_by_session(signals);
    let candidate_amounts = candidate_amounts_by_signal(sizing);
    let trajectory_index = load_trajectory_index(&logs_dir.join("trajectory_index.jsonl"))?
        .into_iter()
        .map(|record| (record.session_id.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let mut generated = 0_u64;
    let mut skipped_missing_source = 0_u64;

    for session in sessions.values() {
        if completed.contains(&session.session_id) {
            continue;
        }
        let source = session
            .raw_stream_path
            .as_deref()
            .map(resolve_runtime_path)
            .filter(|path| path.exists())
            .or_else(|| {
                session
                    .trajectory_path
                    .as_deref()
                    .map(resolve_runtime_path)
                    .filter(|path| path.exists())
            })
            .or_else(|| {
                trajectory_index
                    .get(&session.session_id)
                    .map(|record| resolve_runtime_path(&record.path))
                    .filter(|path| path.exists())
            });
        let Some(source_path) = source else {
            skipped_missing_source += 1;
            continue;
        };
        let session_signals = signals_by_session
            .get(&session.session_id)
            .cloned()
            .unwrap_or_default();
        if session_signals.is_empty() {
            skipped_missing_source += 1;
            continue;
        }
        let metric = backfill_session(session, &session_signals, &candidate_amounts, &source_path)
            .with_context(|| format!("backfill session {}", session.session_id))?;
        let analysis_complete = metric.analysis_complete;
        append_jsonl(&metrics_path, &metric)?;
        if analysis_complete {
            completed.insert(session.session_id.clone());
        }
        generated += 1;
        println!(
            "BACKFILLED\t{}\t{}\t{}",
            session.session_id,
            session.market_slot,
            source_path.display()
        );
    }

    println!(
        "BACKFILL_SUMMARY\tgenerated={}\ttotal_metrics={}\tmissing_source={}",
        generated,
        completed.len(),
        skipped_missing_source
    );
    Ok(())
}

fn backfill_session(
    session: &SessionSummary,
    signals: &[SignalRecord],
    candidate_amounts: &BTreeMap<String, f64>,
    stream_path: &Path,
) -> Result<SessionMetricsRecord> {
    let mut analyzer = SessionAnalyzer::new(
        &session.up_token_id,
        &session.down_token_id,
        FIXED_LIMIT_PRICE,
        MINIMUM_SHARES,
    );
    let groups = group_activation_signals(signals);
    for (prediction, signal_ids) in &groups {
        let amount = signal_ids
            .iter()
            .filter_map(|signal_id| candidate_amounts.get(signal_id).copied())
            .max_by(f64::total_cmp);
        if let Some(amount) = amount {
            analyzer.set_order_candidate(prediction, amount);
        }
    }

    let mut activated_predictions = BTreeSet::new();
    let mut saw_compact_quotes = false;
    for line in open_trajectory_reader(stream_path)?.lines() {
        let line = line?;
        let Ok(envelope) = serde_json::from_str::<StreamEnvelope>(&line) else {
            continue;
        };
        match envelope.event_type.as_str() {
            "signal_snapshot" => {
                saw_compact_quotes = true;
                let Some(prediction) = envelope.payload.get("outcome").and_then(Value::as_str)
                else {
                    continue;
                };
                let signal_ids = groups
                    .get(&prediction.to_ascii_uppercase())
                    .cloned()
                    .unwrap_or_default();
                if analyzer.activate_from_compact_snapshot(
                    prediction,
                    signal_ids,
                    envelope.received_at_unix_ms,
                    &envelope.payload,
                ) {
                    activated_predictions.insert(prediction.to_ascii_uppercase());
                }
            }
            "quote" => {
                saw_compact_quotes = true;
                analyzer.process_compact_quote(&envelope.payload, envelope.received_at_unix_ms);
            }
            "order_candidate" => {
                if let (Some(prediction), Some(amount_usdc)) = (
                    envelope.payload.get("prediction").and_then(Value::as_str),
                    envelope.payload.get("amount_usdc").and_then(Value::as_f64),
                ) {
                    analyzer.set_order_candidate(prediction, amount_usdc);
                }
            }
            "signal_activated" => {
                for (prediction, signal_ids) in &groups {
                    if activated_predictions.contains(prediction) {
                        continue;
                    }
                    analyzer.activate(
                        prediction,
                        signal_ids.iter().cloned(),
                        envelope.received_at_unix_ms,
                    );
                    activated_predictions.insert(prediction.clone());
                }
            }
            _ => {
                analyzer.process_payload(&envelope.payload, envelope.received_at_unix_ms);
            }
        }
    }
    let replay_complete = activated_predictions.len() == groups.len();
    if activated_predictions.len() < groups.len() {
        for (prediction, signal_ids) in &groups {
            if activated_predictions.contains(prediction) {
                continue;
            }
            let detected_at = signals
                .iter()
                .filter(|signal| signal.prediction.eq_ignore_ascii_case(prediction))
                .map(|signal| signal.detected_at_local.timestamp_millis())
                .min()
                .unwrap_or(session.entry_time_ms);
            analyzer.activate(prediction, signal_ids.iter().cloned(), detected_at);
        }
    }

    Ok(analyzer.finish(SessionMetricContext {
        source_format: if saw_compact_quotes {
            "backfill_compact_v2".to_string()
        } else {
            "backfill_raw_v1".to_string()
        },
        analysis_complete: replay_complete,
        session_id: session.session_id.clone(),
        market_slot: session.market_slot.clone(),
        entry_time_ms: session.entry_time_ms,
        slug: session.slug.clone(),
        winning_asset_id: session
            .resolution
            .as_ref()
            .and_then(|resolution| resolution.winning_asset_id.clone()),
        winning_outcome: session
            .resolution
            .as_ref()
            .and_then(|resolution| resolution.winning_outcome.clone()),
        raw_stream_path: session.raw_stream_path.clone(),
        completion_status: session.completion_status.clone(),
        gap_count: session.gap_count,
        reconnect_count: session.reconnect_count,
    }))
}

fn report(logs_dir: &Path) -> Result<()> {
    let metrics = load_metrics(&logs_dir.join("session_metrics.jsonl"))?
        .into_iter()
        .filter(|metric| metric.analysis_complete)
        .collect::<Vec<_>>();
    let signals = load_signals(&logs_dir.join("signals.jsonl"))?;
    let signal_index = signals
        .into_iter()
        .map(|signal| (signal.signal_id.clone(), signal))
        .collect::<BTreeMap<_, _>>();
    let mut rows = BTreeMap::<(String, String, String), Aggregate>::new();
    let mut unique_orders = Aggregate::default();

    for metric in &metrics {
        for outcome in &metric.outcomes {
            if outcome.signal_ids.is_empty() {
                continue;
            }
            let has_data_gaps = metric.gap_count > 0;
            unique_orders.add_outcome(outcome, has_data_gaps);
            for signal_id in &outcome.signal_ids {
                let Some(signal) = signal_index.get(signal_id) else {
                    continue;
                };
                for key in [
                    (
                        "strategy_market".to_string(),
                        signal.strategy.clone(),
                        signal.market_slot.clone(),
                    ),
                    (
                        "strategy".to_string(),
                        signal.strategy.clone(),
                        "ALL".to_string(),
                    ),
                    (
                        "market".to_string(),
                        "ALL".to_string(),
                        signal.market_slot.clone(),
                    ),
                    (
                        "all_signals".to_string(),
                        "ALL".to_string(),
                        "ALL".to_string(),
                    ),
                ] {
                    rows.entry(key)
                        .or_default()
                        .add_outcome(outcome, has_data_gaps);
                }
            }
        }
    }

    println!("scope\tstrategy\tmarket\tsignals\tcandidates\timmediate_fak\tresting_fills\tnot_filled\twins\tlosses\tpending\tdata_gap_records\tfill_rate_pct\timmediate_rate_pct\twin_rate_pct\tpnl_usdc\tev_usdc\tavg_fill_seconds");
    let mut report_rows = Vec::new();
    report_rows.push(to_report_row("unique_orders", "ALL", "ALL", unique_orders));
    report_rows.extend(
        rows.into_iter()
            .map(|((scope, strategy, market), aggregate)| {
                to_report_row(&scope, &strategy, &market, aggregate)
            }),
    );
    for row in &report_rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.4}\t{:.3}",
            row.scope,
            row.strategy,
            row.market,
            row.aggregate.signals,
            row.aggregate.order_candidates,
            row.aggregate.immediate_fak_fills,
            row.aggregate.resting_limit_fills,
            row.aggregate.not_filled,
            row.aggregate.wins,
            row.aggregate.losses,
            row.aggregate.pending,
            row.aggregate.data_gap_records,
            row.fill_rate_pct,
            row.immediate_rate_pct,
            row.win_rate_pct,
            row.aggregate.pnl_usdc,
            row.ev_usdc_per_candidate,
            row.average_fill_seconds,
        );
    }
    atomic_write_json(
        &logs_dir.join("stats_summary.json"),
        &json!({
            "schema_version": 1,
            "generated_at": Utc::now(),
            "session_metrics_count": metrics.len(),
            "rows": report_rows,
        }),
    )?;
    write_minimal_reports(logs_dir, &metrics, &signal_index)?;
    write_trajectory_reports(logs_dir, &metrics, &signal_index)?;
    Ok(())
}

fn write_trajectory_reports(
    logs_dir: &Path,
    metrics: &[SessionMetricsRecord],
    signal_index: &BTreeMap<String, SignalRecord>,
) -> Result<()> {
    let sessions = load_sessions(&logs_dir.join("sessions.jsonl"))?;
    let trajectory_index = load_trajectory_index(&logs_dir.join("trajectory_index.jsonl"))?
        .into_iter()
        .map(|record| (record.session_id.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let inputs = build_report_sessions(metrics, signal_index, &sessions, &trajectory_index);
    let settings = ReportSettings {
        costs: ExitCostAssumptions {
            fee_bps: parse_env_f64("PORTFOLIO_STATS_EXIT_FEE_BPS", 0.0)?,
            slippage_bps: parse_env_f64("PORTFOLIO_STATS_EXIT_SLIPPAGE_BPS", 0.0)?,
        },
        minimum_sample_size: parse_env_u64("PORTFOLIO_STATS_MIN_SAMPLE_SIZE", 30)?,
    };
    let reports = build_trajectory_reports(&inputs, &STRATEGY_NAMES, settings);
    for (name, report) in reports.temporal {
        atomic_write_json(
            &logs_dir
                .join("stats")
                .join("temporal")
                .join(format!("{name}.json")),
            &report,
        )?;
    }
    for (name, report) in reports.risk {
        atomic_write_json(
            &logs_dir
                .join("stats")
                .join("risk")
                .join(format!("{name}.json")),
            &report,
        )?;
    }
    Ok(())
}

fn build_report_sessions(
    metrics: &[SessionMetricsRecord],
    signal_index: &BTreeMap<String, SignalRecord>,
    sessions: &BTreeMap<String, SessionSummary>,
    trajectory_index: &BTreeMap<String, TrajectoryIndexRecord>,
) -> Vec<ReportSession> {
    metrics
        .iter()
        .map(|metric| {
            let summary = sessions.get(&metric.session_id);
            let trajectory_path = summary
                .and_then(|session| session.trajectory_path.as_deref())
                .or_else(|| {
                    trajectory_index
                        .get(&metric.session_id)
                        .map(|record| record.path.as_str())
                })
                .map(resolve_runtime_path);
            let outcomes = metric
                .outcomes
                .iter()
                .filter(|outcome| !outcome.signal_ids.is_empty())
                .map(|outcome| {
                    let signals = outcome
                        .signal_ids
                        .iter()
                        .filter_map(|signal_id| signal_index.get(signal_id))
                        .map(|signal| ReportSignal {
                            signal_id: signal.signal_id.clone(),
                            strategy: signal.strategy.clone(),
                            prediction: signal.prediction.clone(),
                        })
                        .collect::<Vec<_>>();
                    let signal_at_unix_ms = outcome.signal_at_unix_ms.unwrap_or_else(|| {
                        outcome
                            .signal_ids
                            .iter()
                            .filter_map(|signal_id| signal_index.get(signal_id))
                            .map(|signal| signal.detected_at_local.timestamp_millis())
                            .min()
                            .unwrap_or(metric.entry_time_ms)
                    });
                    ReportOutcome {
                        outcome: outcome.outcome.clone(),
                        signal_at_unix_ms,
                        winning_outcome: outcome.winning_outcome,
                        signals,
                    }
                })
                .collect();
            ReportSession {
                session_id: metric.session_id.clone(),
                market_slot: metric.market_slot.clone(),
                target_close_time_ms: metric.entry_time_ms + interval_millis(&metric.market_slot)
                    - 1,
                completion_status: metric.completion_status.clone(),
                gap_count: metric.gap_count,
                trajectory_path,
                outcomes,
            }
        })
        .collect()
}

fn interval_millis(market_slot: &str) -> i64 {
    if market_slot.ends_with("_15m") {
        15 * 60 * 1_000
    } else {
        5 * 60 * 1_000
    }
}

fn write_minimal_reports(
    logs_dir: &Path,
    metrics: &[SessionMetricsRecord],
    signal_index: &BTreeMap<String, SignalRecord>,
) -> Result<()> {
    let reports = build_minimal_reports(metrics, signal_index);
    let stats_dir = logs_dir.join("stats");
    atomic_write_json(
        &stats_dir.join("global_all_signals.json"),
        &reports.global_all_signals,
    )?;
    atomic_write_json(
        &stats_dir.join("global_majority.json"),
        &reports.global_majority,
    )?;
    for strategy in STRATEGY_NAMES {
        let stats = reports
            .by_strategy
            .get(strategy)
            .cloned()
            .unwrap_or_default();
        atomic_write_json(&stats_dir.join(format!("{strategy}.json")), &stats)?;
    }
    Ok(())
}

fn build_minimal_reports(
    metrics: &[SessionMetricsRecord],
    signal_index: &BTreeMap<String, SignalRecord>,
) -> MinimalReports {
    let mut reports = MinimalReports {
        global_all_signals: MinimalStats::default(),
        global_majority: MajorityStats::default(),
        by_strategy: STRATEGY_NAMES
            .iter()
            .map(|strategy| ((*strategy).to_string(), MinimalStats::default()))
            .collect(),
    };

    for metric in metrics {
        for outcome in &metric.outcomes {
            for signal_id in &outcome.signal_ids {
                let Some(signal) = signal_index.get(signal_id) else {
                    continue;
                };
                reports.global_all_signals.add_outcome(outcome);
                if let Some(stats) = reports.by_strategy.get_mut(&signal.strategy) {
                    stats.add_outcome(outcome);
                }
            }
        }

        let up = metric
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome.eq_ignore_ascii_case("UP"));
        let down = metric
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome.eq_ignore_ascii_case("DOWN"));
        let up_votes = up.map_or(0, |outcome| outcome.signal_ids.len());
        let down_votes = down.map_or(0, |outcome| outcome.signal_ids.len());
        match majority_vote(up_votes, down_votes) {
            MajorityVote::Up => {
                if let Some(outcome) = up {
                    reports.global_majority.stats.add_outcome(outcome);
                }
            }
            MajorityVote::Down => {
                if let Some(outcome) = down {
                    reports.global_majority.stats.add_outcome(outcome);
                }
            }
            MajorityVote::Tie => reports.global_majority.trades_ignored_tie += 1,
            MajorityVote::NoSignals => {}
        }
    }
    reports
}

fn purge(logs_dir: &Path, confirm: bool) -> Result<()> {
    let sessions = load_sessions(&logs_dir.join("sessions.jsonl"))?;
    let metrics = load_metrics(&logs_dir.join("session_metrics.jsonl"))?;
    let completed = metrics
        .iter()
        .filter(|metric| metric.analysis_complete)
        .map(|metric| metric.session_id.as_str())
        .collect::<BTreeSet<_>>();
    let active = load_active_session_ids(&logs_dir.join("recorder_state.json"))?;
    let require_trajectory = parse_env_bool("PORTFOLIO_RECORDER_PRESERVE_TRAJECTORIES", false)?;
    let trajectory_index = load_trajectory_index(&logs_dir.join("trajectory_index.jsonl"))?
        .into_iter()
        .map(|record| (record.session_id.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let streams_root = fs::canonicalize(logs_dir.join("streams"))
        .with_context(|| format!("dossier streams absent dans {}", logs_dir.display()))?;
    let mut candidates = Vec::new();
    let mut skipped_without_trajectory = 0_u64;
    for session in sessions.values() {
        if !completed.contains(session.session_id.as_str()) || active.contains(&session.session_id)
        {
            continue;
        }
        let Some(raw_path) = session.raw_stream_path.as_deref() else {
            continue;
        };
        let path = resolve_runtime_path(raw_path);
        if !path.exists() {
            continue;
        }
        let canonical = fs::canonicalize(&path)?;
        if !canonical.starts_with(&streams_root) {
            return Err(anyhow!(
                "refus de purger un fichier hors streams: {}",
                canonical.display()
            ));
        }
        if require_trajectory {
            let Some(record) = trajectory_index.get(&session.session_id) else {
                skipped_without_trajectory += 1;
                continue;
            };
            let mut resolved = record.clone();
            resolved.path = resolve_runtime_path(&record.path)
                .to_string_lossy()
                .into_owned();
            if let Err(error) = verify_trajectory(&resolved) {
                skipped_without_trajectory += 1;
                eprintln!(
                    "PURGE_SKIPPED_INVALID_TRAJECTORY\t{}\t{error:#}",
                    session.session_id
                );
                continue;
            }
        }
        let bytes = fs::metadata(&canonical)?.len();
        candidates.push((session.session_id.clone(), canonical, bytes));
    }
    let total_bytes = candidates.iter().map(|(_, _, bytes)| *bytes).sum::<u64>();
    println!(
        "PURGE_PLAN\tfiles={}\tbytes={}\tgib={:.3}\tconfirmed={}\tskipped_without_trajectory={}",
        candidates.len(),
        total_bytes,
        total_bytes as f64 / 1_073_741_824.0,
        confirm,
        skipped_without_trajectory
    );
    if !confirm {
        println!("DRY_RUN_ONLY\trelancez avec --confirm après avoir vérifié le rapport");
        return Ok(());
    }
    for (session_id, path, bytes) in candidates {
        fs::remove_file(&path).with_context(|| format!("suppression {}", path.display()))?;
        append_jsonl(
            &logs_dir.join("stream_cleanup.jsonl"),
            &json!({
                "schema_version": 1,
                "record_type": "STREAM_DELETED_AFTER_METRICS",
                "deleted_at": Utc::now(),
                "session_id": session_id,
                "path": path,
                "bytes_reclaimed": bytes,
            }),
        )?;
        println!("PURGED\t{}\t{}\t{}", session_id, bytes, path.display());
    }
    Ok(())
}

fn verify(logs_dir: &Path) -> Result<()> {
    let sessions = load_sessions(&logs_dir.join("sessions.jsonl"))?;
    let metrics = load_metrics(&logs_dir.join("session_metrics.jsonl"))?;
    let active = load_active_session_ids(&logs_dir.join("recorder_state.json"))?;
    let raw_files = count_files(&logs_dir.join("streams"), "jsonl")?;
    let raw_bytes = directory_size(&logs_dir.join("streams"))?;
    let trajectory_files = count_files(&logs_dir.join("trajectories"), "zst")?;
    let trajectory_bytes = directory_size(&logs_dir.join("trajectories"))?;
    let trajectory_index = load_trajectory_index(&logs_dir.join("trajectory_index.jsonl"))?;
    let indexed_ids = trajectory_index
        .iter()
        .map(|record| record.session_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut invalid_trajectories = 0_u64;
    for record in &trajectory_index {
        let mut resolved = record.clone();
        resolved.path = resolve_runtime_path(&record.path)
            .to_string_lossy()
            .into_owned();
        if let Err(error) = verify_trajectory(&resolved) {
            invalid_trajectories += 1;
            eprintln!("TRAJECTORY_INVALID\t{}\t{error:#}", record.session_id);
        }
    }
    let mut reconstructable_index_entries = 0_u64;
    for session in sessions
        .values()
        .filter(|session| !indexed_ids.contains(session.session_id.as_str()))
    {
        match recoverable_index_record(logs_dir, session) {
            Ok(Some(_)) => reconstructable_index_entries += 1,
            Ok(None) => {}
            Err(error) => {
                invalid_trajectories += 1;
                eprintln!(
                    "TRAJECTORY_UNINDEXED_INVALID\t{}\t{error:#}",
                    session.session_id
                );
            }
        }
    }
    let metric_ids = metrics
        .iter()
        .filter(|metric| metric.analysis_complete)
        .map(|metric| metric.session_id.as_str())
        .collect::<BTreeSet<_>>();
    let incomplete_metrics = metrics
        .iter()
        .filter(|metric| !metric.analysis_complete)
        .count();
    let finalized_without_metrics = sessions
        .keys()
        .filter(|session_id| !metric_ids.contains(session_id.as_str()))
        .count();
    println!("VERIFY\tsessions={}\tmetrics={}\tincomplete_metrics={}\tactive={}\traw_files={}\traw_gib={:.3}\ttrajectory_files={}\ttrajectory_gib={:.3}\tindexed_trajectories={}\treconstructable_index_entries={}\tinvalid_trajectories={}\tfinalized_without_metrics={}", sessions.len(), metric_ids.len(), incomplete_metrics, active.len(), raw_files, raw_bytes as f64 / 1_073_741_824.0, trajectory_files, trajectory_bytes as f64 / 1_073_741_824.0, trajectory_index.len(), reconstructable_index_entries, invalid_trajectories, finalized_without_metrics);
    if invalid_trajectories > 0 {
        return Err(anyhow!(
            "{} trajectoire(s) invalide(s)",
            invalid_trajectories
        ));
    }
    Ok(())
}

fn repair_trajectory_index(logs_dir: &Path) -> Result<()> {
    let sessions = load_sessions(&logs_dir.join("sessions.jsonl"))?;
    let indexed_ids = load_trajectory_index(&logs_dir.join("trajectory_index.jsonl"))?
        .into_iter()
        .map(|record| record.session_id)
        .collect::<BTreeSet<_>>();
    let mut recoverable = Vec::new();
    let mut incomplete_metadata = 0_u64;

    for session in sessions.values() {
        if indexed_ids.contains(&session.session_id) {
            continue;
        }
        let record = match recoverable_index_record(logs_dir, session)? {
            Some(record) => record,
            None => {
                let Some(source) = session
                    .raw_stream_path
                    .as_deref()
                    .map(resolve_runtime_path)
                    .filter(|path| path.exists())
                else {
                    incomplete_metadata += 1;
                    continue;
                };
                let destination =
                    trajectory_path(logs_dir, session.entry_time_ms, &session.session_id);
                finalize_trajectory_sync(&source, &destination, trajectory_metadata(session))
                    .with_context(|| {
                        format!(
                            "finalisation réparatrice de la session {}",
                            session.session_id
                        )
                    })?
            }
        };
        recoverable.push(record);
    }

    for record in &recoverable {
        upsert_trajectory_index(logs_dir, record.clone())?;
        println!("INDEX_REPAIRED\t{}\t{}", record.session_id, record.path);
    }
    println!(
        "REPAIR_INDEX_SUMMARY\trepaired={}\tincomplete_metadata={}",
        recoverable.len(),
        incomplete_metadata
    );
    Ok(())
}

fn recoverable_index_record(
    logs_dir: &Path,
    session: &SessionSummary,
) -> Result<Option<TrajectoryIndexRecord>> {
    if let Some(record) = reconstruct_index_record(session) {
        let mut resolved = record.clone();
        resolved.path = resolve_runtime_path(&record.path)
            .to_string_lossy()
            .into_owned();
        verify_trajectory(&resolved).with_context(|| {
            format!(
                "validation des métadonnées de la session {}",
                session.session_id
            )
        })?;
        return Ok(Some(record));
    }
    let path = session
        .trajectory_path
        .as_deref()
        .map(resolve_runtime_path)
        .unwrap_or_else(|| trajectory_path(logs_dir, session.entry_time_ms, &session.session_id));
    if !path.exists() {
        return Ok(None);
    }
    recover_trajectory_index_record(&path, trajectory_metadata(session))
        .with_context(|| {
            format!(
                "reconstruction des métadonnées de la session {}",
                session.session_id
            )
        })
        .map(Some)
}

fn trajectory_metadata(session: &SessionSummary) -> TrajectoryMetadata {
    TrajectoryMetadata {
        session_id: session.session_id.clone(),
        market_slot: session.market_slot.clone(),
        entry_time_ms: session.entry_time_ms,
        slug: session.slug.clone(),
        signal_ids: session.signal_ids.clone(),
        completion_status: session.completion_status.clone(),
        gap_count: session.gap_count,
    }
}

fn reconstruct_index_record(session: &SessionSummary) -> Option<TrajectoryIndexRecord> {
    Some(TrajectoryIndexRecord {
        schema_version: 1,
        session_id: session.session_id.clone(),
        market_slot: session.market_slot.clone(),
        entry_time_ms: session.entry_time_ms,
        slug: session.slug.clone(),
        signal_ids: session.signal_ids.clone(),
        completion_status: session.completion_status.clone(),
        gap_count: session.gap_count,
        path: session.trajectory_path.clone()?,
        sha256: session.trajectory_sha256.clone()?,
        observation_count: session.trajectory_observation_count?,
        uncompressed_bytes: session.trajectory_uncompressed_bytes?,
        compressed_bytes: session.trajectory_compressed_bytes?,
        finalized_at: session.trajectory_finalized_at?,
    })
}

fn to_report_row(scope: &str, strategy: &str, market: &str, aggregate: Aggregate) -> ReportRow {
    ReportRow {
        scope: scope.to_string(),
        strategy: strategy.to_string(),
        market: market.to_string(),
        fill_rate_pct: aggregate.fill_rate_pct(),
        immediate_rate_pct: aggregate.immediate_rate_pct(),
        win_rate_pct: aggregate.win_rate_pct(),
        ev_usdc_per_candidate: aggregate.ev_usdc(),
        average_fill_seconds: aggregate.average_fill_seconds(),
        aggregate,
    }
}

fn load_sessions(path: &Path) -> Result<BTreeMap<String, SessionSummary>> {
    Ok(read_jsonl::<SessionSummary>(path)?
        .into_iter()
        .map(|session| (session.session_id.clone(), session))
        .collect())
}

fn load_signals(path: &Path) -> Result<Vec<SignalRecord>> {
    read_jsonl(path)
}

fn load_sizing(path: &Path) -> Result<Vec<SizingRecord>> {
    read_jsonl(path)
}

fn load_metrics(path: &Path) -> Result<Vec<SessionMetricsRecord>> {
    let mut by_session = BTreeMap::new();
    for metric in read_jsonl::<SessionMetricsRecord>(path)? {
        by_session.insert(metric.session_id.clone(), metric);
    }
    Ok(by_session.into_values().collect())
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path)?;
    let mut values = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(&line)
            .with_context(|| format!("JSONL invalide {} ligne {}", path.display(), index + 1))?;
        values.push(value);
    }
    Ok(values)
}

fn group_signals_by_session(signals: Vec<SignalRecord>) -> BTreeMap<String, Vec<SignalRecord>> {
    let mut grouped = BTreeMap::<String, Vec<SignalRecord>>::new();
    for signal in signals {
        if let Some(session_id) = signal.session_id.clone() {
            grouped.entry(session_id).or_default().push(signal);
        }
    }
    grouped
}

fn group_activation_signals(signals: &[SignalRecord]) -> BTreeMap<String, Vec<String>> {
    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for signal in signals {
        grouped
            .entry(signal.prediction.to_ascii_uppercase())
            .or_default()
            .push(signal.signal_id.clone());
    }
    grouped
}

fn candidate_amounts_by_signal(sizing: Vec<SizingRecord>) -> BTreeMap<String, f64> {
    sizing
        .into_iter()
        .filter(|record| record.disposition == "DRY_RUN_ORDER_CANDIDATE")
        .filter_map(|record| {
            let amount = record.details.get("combined_amount_usdc")?.as_f64()?;
            Some((record.signal_id, amount))
        })
        .collect()
}

fn load_active_session_ids(path: &Path) -> Result<BTreeSet<String>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(value
        .get("active_sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|session| session.get("session_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

fn resolve_runtime_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("fichier sans dossier parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("fichier sans dossier parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn count_files(path: &Path, extension: &str) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            count += count_files(&entry_path, extension)?;
        } else if entry_path.extension().and_then(|value| value.to_str()) == Some(extension) {
            count += 1;
        }
    }
    Ok(count)
}

fn directory_size(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut bytes = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            bytes += directory_size(&entry.path())?;
        } else {
            bytes += metadata.len();
        }
    }
    Ok(bytes)
}

fn parse_env_f64(key: &str, default: f64) -> Result<f64> {
    match env::var(key) {
        Ok(value) => value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("{key} doit être un nombre positif"))
            .and_then(|parsed| {
                (parsed >= 0.0 && parsed.is_finite())
                    .then_some(parsed)
                    .ok_or_else(|| anyhow!("{key} doit être un nombre positif"))
            }),
        Err(_) => Ok(default),
    }
}

fn parse_env_u64(key: &str, default: u64) -> Result<u64> {
    match env::var(key) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .with_context(|| format!("{key} doit être un entier positif")),
        Err(_) => Ok(default),
    }
}

fn parse_env_bool(key: &str, default: bool) -> Result<bool> {
    match env::var(key) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(anyhow!("{key} doit être true ou false")),
        },
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_strictly_below_0_50, majority_vote, Aggregate, MajorityVote};

    #[test]
    fn aggregate_rates_include_non_fills_in_ev_denominator() {
        let aggregate = Aggregate {
            order_candidates: 4,
            resting_limit_fills: 2,
            immediate_fak_fills: 1,
            wins: 1,
            losses: 1,
            pnl_usdc: 1.0,
            ..Aggregate::default()
        };
        assert_eq!(aggregate.fill_rate_pct(), 50.0);
        assert_eq!(aggregate.immediate_rate_pct(), 25.0);
        assert_eq!(aggregate.win_rate_pct(), 50.0);
        assert_eq!(aggregate.ev_usdc(), 0.25);
    }

    #[test]
    fn strict_crossing_accepts_price_below_half() {
        assert!(is_strictly_below_0_50(Some(0.49)));
    }

    #[test]
    fn strict_crossing_rejects_price_equal_to_half() {
        assert!(!is_strictly_below_0_50(Some(0.50)));
    }

    #[test]
    fn majority_vote_selects_up_for_two_up_and_one_down() {
        assert_eq!(majority_vote(2, 1), MajorityVote::Up);
    }

    #[test]
    fn majority_vote_ignores_equal_votes() {
        assert_eq!(majority_vote(2, 2), MajorityVote::Tie);
    }
}
