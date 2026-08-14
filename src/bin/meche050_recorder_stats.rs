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
    SessionAnalyzer, SessionMetricContext, SessionMetricsRecord,
};

const FIXED_LIMIT_PRICE: f64 = 0.50;
const MINIMUM_SHARES: f64 = 5.0;

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
    resolution: Option<Resolution>,
    raw_stream_path: Option<String>,
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

fn main() -> Result<()> {
    let cli = parse_cli()?;
    match cli.command.as_str() {
        "backfill" => backfill(&cli.logs_dir),
        "report" => report(&cli.logs_dir),
        "purge" => purge(&cli.logs_dir, cli.confirm),
        "verify" => verify(&cli.logs_dir),
        "all" => {
            backfill(&cli.logs_dir)?;
            report(&cli.logs_dir)?;
            purge(&cli.logs_dir, cli.confirm)?;
            verify(&cli.logs_dir)
        }
        command => Err(anyhow!(
            "commande inconnue '{command}'; utilisez backfill, report, purge, verify ou all"
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
    let mut generated = 0_u64;
    let mut skipped_missing_stream = 0_u64;

    for session in sessions.values() {
        if completed.contains(&session.session_id) {
            continue;
        }
        let Some(stream_value) = session.raw_stream_path.as_deref() else {
            skipped_missing_stream += 1;
            continue;
        };
        let stream_path = resolve_runtime_path(stream_value);
        if !stream_path.exists() {
            skipped_missing_stream += 1;
            continue;
        }
        let session_signals = signals_by_session
            .get(&session.session_id)
            .cloned()
            .unwrap_or_default();
        if session_signals.is_empty() {
            skipped_missing_stream += 1;
            continue;
        }
        let metric = backfill_session(session, &session_signals, &candidate_amounts, &stream_path)
            .with_context(|| format!("backfill session {}", session.session_id))?;
        let analysis_complete = metric.analysis_complete;
        append_jsonl(&metrics_path, &metric)?;
        if analysis_complete {
            completed.insert(session.session_id.clone());
        }
        generated += 1;
        println!(
            "BACKFILLED\t{}\t{}\t{}",
            session.session_id, session.market_slot, stream_value
        );
    }

    println!(
        "BACKFILL_SUMMARY\tgenerated={}\ttotal_metrics={}\tmissing_stream={}",
        generated,
        completed.len(),
        skipped_missing_stream
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

    let file = fs::File::open(stream_path)
        .with_context(|| format!("lecture stream {}", stream_path.display()))?;
    let mut activated_predictions = BTreeSet::new();
    let mut saw_compact_quotes = false;
    for line in BufReader::new(file).lines() {
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
    Ok(())
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
    let streams_root = fs::canonicalize(logs_dir.join("streams"))
        .with_context(|| format!("dossier streams absent dans {}", logs_dir.display()))?;
    let mut candidates = Vec::new();
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
        let bytes = fs::metadata(&canonical)?.len();
        candidates.push((session.session_id.clone(), canonical, bytes));
    }
    let total_bytes = candidates.iter().map(|(_, _, bytes)| *bytes).sum::<u64>();
    println!(
        "PURGE_PLAN\tfiles={}\tbytes={}\tgib={:.3}\tconfirmed={}",
        candidates.len(),
        total_bytes,
        total_bytes as f64 / 1_073_741_824.0,
        confirm
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
    println!("VERIFY\tsessions={}\tmetrics={}\tincomplete_metrics={}\tactive={}\traw_files={}\traw_gib={:.3}\tfinalized_without_metrics={}", sessions.len(), metric_ids.len(), incomplete_metrics, active.len(), raw_files, raw_bytes as f64 / 1_073_741_824.0, finalized_without_metrics);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::Aggregate;

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
}
