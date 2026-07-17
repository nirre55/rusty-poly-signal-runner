//! Append-only audit journal for causal microstructure decisions.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use crate::binance::Candle;
use crate::microstructure::{Feature, MicrostructureSnapshot};
use crate::strategies::ethusd_perp_coinm_15m_microstructure_mixed_13::EthUsdPerpMicrostructureMixed13;
use crate::strategy::{MicrostructureDecisionSummary, Prediction, Strategy};

pub const AUDIT_FILE_NAME: &str = "microstructure_decisions.jsonl";
const AUDIT_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AuditRecordStatus {
    Decision,
    CollectionError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionOutcome {
    Up,
    Down,
    Skip,
}

impl DecisionOutcome {
    fn from_prediction(prediction: Option<&Prediction>) -> Self {
        match prediction {
            Some(Prediction::Up) => Self::Up,
            Some(Prediction::Down) => Self::Down,
            None => Self::Skip,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditCandle {
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

impl From<&Candle> for AuditCandle {
    fn from(candle: &Candle) -> Self {
        Self {
            open_time: candle.open_time,
            close_time: candle.close_time,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
        }
    }
}

impl AuditCandle {
    fn to_candle(&self) -> Candle {
        Candle {
            open_time: self.open_time,
            close_time: self.close_time,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: 0.0,
            is_closed: true,
        }
    }
}

/// One durable record in the microstructure audit journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicrostructureAuditRecord {
    pub schema_version: u8,
    pub status: AuditRecordStatus,
    pub recorded_at: DateTime<Utc>,
    pub observed_at: Option<DateTime<Utc>>,
    pub strategy_name: String,
    pub candle: Option<AuditCandle>,
    pub features: BTreeMap<String, f64>,
    pub feature_source_times: BTreeMap<String, DateTime<Utc>>,
    pub outcome: Option<DecisionOutcome>,
    pub green_votes: Option<u32>,
    pub red_votes: Option<u32>,
    pub active_rules: Vec<String>,
    pub polymarket_slug: Option<String>,
    pub error: Option<String>,
    pub previous_hash: Option<String>,
    pub record_hash: String,
}

#[derive(Serialize)]
struct HashPayload<'a> {
    schema_version: u8,
    status: AuditRecordStatus,
    recorded_at: DateTime<Utc>,
    observed_at: Option<DateTime<Utc>>,
    strategy_name: &'a str,
    candle: &'a Option<AuditCandle>,
    features: &'a BTreeMap<String, f64>,
    feature_source_times: &'a BTreeMap<String, DateTime<Utc>>,
    outcome: Option<DecisionOutcome>,
    green_votes: Option<u32>,
    red_votes: Option<u32>,
    active_rules: &'a [String],
    polymarket_slug: &'a Option<String>,
    error: &'a Option<String>,
    previous_hash: &'a Option<String>,
}

impl MicrostructureAuditRecord {
    pub fn decision(
        snapshot: &MicrostructureSnapshot,
        strategy_name: &str,
        summary: &MicrostructureDecisionSummary,
        polymarket_slug: String,
    ) -> Result<Self> {
        snapshot.ensure_audit_complete()?;
        let features = snapshot
            .values()
            .iter()
            .map(|(feature, value)| (feature.as_str().to_string(), *value))
            .collect();
        let feature_source_times = snapshot
            .feature_source_times()
            .iter()
            .map(|(feature, source_time)| (feature.as_str().to_string(), *source_time))
            .collect();

        Ok(Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            status: AuditRecordStatus::Decision,
            recorded_at: Utc::now(),
            observed_at: Some(snapshot.observed_at()),
            strategy_name: strategy_name.to_string(),
            candle: Some(AuditCandle::from(snapshot.candle())),
            features,
            feature_source_times,
            outcome: Some(DecisionOutcome::from_prediction(
                summary.prediction.as_ref(),
            )),
            green_votes: Some(summary.green_votes),
            red_votes: Some(summary.red_votes),
            active_rules: summary.active_rules.clone(),
            polymarket_slug: Some(polymarket_slug),
            error: None,
            previous_hash: None,
            record_hash: String::new(),
        })
    }

    pub fn collection_error(strategy_name: &str, error: &str) -> Self {
        Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            status: AuditRecordStatus::CollectionError,
            recorded_at: Utc::now(),
            observed_at: None,
            strategy_name: strategy_name.to_string(),
            candle: None,
            features: BTreeMap::new(),
            feature_source_times: BTreeMap::new(),
            outcome: None,
            green_votes: None,
            red_votes: None,
            active_rules: Vec::new(),
            polymarket_slug: None,
            error: Some(sanitize_error(error)),
            previous_hash: None,
            record_hash: String::new(),
        }
    }

    fn calculate_hash(&self) -> Result<String> {
        let payload = HashPayload {
            schema_version: self.schema_version,
            status: self.status,
            recorded_at: self.recorded_at,
            observed_at: self.observed_at,
            strategy_name: &self.strategy_name,
            candle: &self.candle,
            features: &self.features,
            feature_source_times: &self.feature_source_times,
            outcome: self.outcome,
            green_votes: self.green_votes,
            red_votes: self.red_votes,
            active_rules: &self.active_rules,
            polymarket_slug: &self.polymarket_slug,
            error: &self.error,
            previous_hash: &self.previous_hash,
        };
        let bytes = serde_json::to_vec(&payload).context("serialisation hash audit")?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

/// Synchronously appends audit records and calls `sync_data` before returning.
pub struct MicrostructureAuditLogger {
    path: PathBuf,
    lock: Mutex<()>,
}

impl MicrostructureAuditLogger {
    pub fn new(logs_dir: &str) -> Result<Self> {
        fs::create_dir_all(logs_dir)?;
        let path = PathBuf::from(logs_dir).join(AUDIT_FILE_NAME);
        let existing = read_audit_records(&path)?;
        let integrity_errors = verify_hash_chain(&existing);
        if !integrity_errors.is_empty() {
            bail!(
                "journal d'audit existant invalide: {}",
                integrity_errors.join("; ")
            );
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &mut MicrostructureAuditRecord) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| anyhow!("audit lock poisoned: {error}"))?;
        let records = read_audit_records(&self.path)?;
        let integrity_errors = verify_hash_chain(&records);
        if !integrity_errors.is_empty() {
            bail!(
                "refus d'ajouter a un journal d'audit invalide: {}",
                integrity_errors.join("; ")
            );
        }
        record.previous_hash = records.last().map(|last| last.record_hash.clone());
        record.record_hash = record.calculate_hash()?;
        let serialized = serde_json::to_string(record).context("serialisation JSONL audit")?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("ouverture journal audit {}", self.path.display()))?;
        writeln!(file, "{serialized}")?;
        file.sync_data()
            .with_context(|| format!("synchronisation journal audit {}", self.path.display()))?;
        Ok(())
    }
}

pub fn read_audit_records(path: &Path) -> Result<Vec<MicrostructureAuditRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("lecture journal audit {}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("JSON audit invalide ligne {}", index + 1))
        })
        .collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuditVerificationReport {
    pub total_records: usize,
    pub decisions: usize,
    pub up: usize,
    pub down: usize,
    pub skip: usize,
    pub collection_errors: usize,
    pub failures: Vec<String>,
}

impl AuditVerificationReport {
    pub fn is_valid(&self) -> bool {
        self.failures.is_empty()
    }
}

pub fn verify_audit_file(path: &Path) -> Result<AuditVerificationReport> {
    let records = read_audit_records(path)?;
    let mut report = AuditVerificationReport {
        total_records: records.len(),
        failures: verify_hash_chain(&records),
        ..AuditVerificationReport::default()
    };

    for (index, record) in records.iter().enumerate() {
        let line = index + 1;
        match record.status {
            AuditRecordStatus::CollectionError => {
                report.collection_errors += 1;
                validate_collection_error(record, line, &mut report.failures);
            }
            AuditRecordStatus::Decision => {
                report.decisions += 1;
                validate_decision_record(record, line, &mut report);
            }
        }
    }
    Ok(report)
}

fn verify_hash_chain(records: &[MicrostructureAuditRecord]) -> Vec<String> {
    let mut failures = Vec::new();
    let mut previous_hash = None;
    for (index, record) in records.iter().enumerate() {
        let line = index + 1;
        if record.previous_hash != previous_hash {
            failures.push(format!("ligne {line}: previous_hash invalide"));
        }
        match record.calculate_hash() {
            Ok(expected) if expected == record.record_hash => {}
            Ok(_) => failures.push(format!("ligne {line}: record_hash invalide")),
            Err(error) => failures.push(format!("ligne {line}: hash non calculable: {error}")),
        }
        previous_hash = Some(record.record_hash.clone());
    }
    failures
}

fn validate_collection_error(
    record: &MicrostructureAuditRecord,
    line: usize,
    failures: &mut Vec<String>,
) {
    if record.schema_version != AUDIT_SCHEMA_VERSION {
        failures.push(format!("ligne {line}: version audit inconnue"));
    }
    if record.error.as_deref().unwrap_or_default().is_empty() {
        failures.push(format!("ligne {line}: collection error sans message"));
    }
    if record.candle.is_some()
        || !record.features.is_empty()
        || !record.feature_source_times.is_empty()
        || record.outcome.is_some()
    {
        failures.push(format!(
            "ligne {line}: collection error contient une decision"
        ));
    }
}

fn validate_decision_record(
    record: &MicrostructureAuditRecord,
    line: usize,
    report: &mut AuditVerificationReport,
) {
    if record.schema_version != AUDIT_SCHEMA_VERSION {
        report
            .failures
            .push(format!("ligne {line}: version audit inconnue"));
        return;
    }
    let (Some(observed_at), Some(candle), Some(outcome), Some(green_votes), Some(red_votes)) = (
        record.observed_at,
        record.candle.as_ref(),
        record.outcome,
        record.green_votes,
        record.red_votes,
    ) else {
        report
            .failures
            .push(format!("ligne {line}: decision incomplete"));
        return;
    };
    match outcome {
        DecisionOutcome::Up => report.up += 1,
        DecisionOutcome::Down => report.down += 1,
        DecisionOutcome::Skip => report.skip += 1,
    }
    if observed_at < candle.close_time {
        report.failures.push(format!(
            "ligne {line}: observed_at precede la cloture de decision"
        ));
    }

    let mut values = BTreeMap::new();
    for feature in Feature::ALL {
        let name = feature.as_str();
        let Some(value) = record.features.get(name) else {
            report
                .failures
                .push(format!("ligne {line}: feature absente {name}"));
            continue;
        };
        if !value.is_finite() {
            report
                .failures
                .push(format!("ligne {line}: feature non finie {name}"));
            continue;
        }
        let Some(source_time) = record.feature_source_times.get(name) else {
            report
                .failures
                .push(format!("ligne {line}: source absente {name}"));
            continue;
        };
        if *source_time > candle.close_time {
            report.failures.push(format!(
                "ligne {line}: source future {name} ({source_time} > {})",
                candle.close_time
            ));
            continue;
        }
        values.insert(*feature, *value);
    }
    if record.features.len() != Feature::ALL.len()
        || record.feature_source_times.len() != Feature::ALL.len()
    {
        report
            .failures
            .push(format!("ligne {line}: nombre de features invalide"));
    }
    for name in record.features.keys() {
        if Feature::from_str(name).is_err() {
            report
                .failures
                .push(format!("ligne {line}: feature inconnue {name}"));
        }
    }
    if values.len() != Feature::ALL.len() {
        return;
    }

    let source_times = record
        .feature_source_times
        .iter()
        .filter_map(|(name, source_time)| Feature::from_str(name).ok().map(|f| (f, *source_time)))
        .collect();
    let snapshot = MicrostructureSnapshot::with_metadata(
        candle.to_candle(),
        values,
        observed_at,
        source_times,
    );
    let mut strategy = EthUsdPerpMicrostructureMixed13::new();
    let _ = strategy.on_microstructure_snapshot(&snapshot);
    let Some(summary) = strategy.last_microstructure_decision_summary() else {
        report
            .failures
            .push(format!("ligne {line}: strategie sans resume audit"));
        return;
    };
    let expected_outcome = DecisionOutcome::from_prediction(summary.prediction.as_ref());
    if outcome != expected_outcome
        || green_votes != summary.green_votes
        || red_votes != summary.red_votes
        || record.active_rules != summary.active_rules
    {
        report
            .failures
            .push(format!("ligne {line}: decision re-evaluee differente"));
    }
}

fn sanitize_error(error: &str) -> String {
    let mut sanitized = error
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    sanitized.truncate(500);
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rusty_poly_signal_runner_audit_{label}_{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn snapshot(source_time: DateTime<Utc>) -> MicrostructureSnapshot {
        let candle = Candle {
            open_time: source_time - Duration::minutes(15),
            close_time: source_time,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1.0,
            is_closed: true,
        };
        let values = Feature::ALL
            .iter()
            .copied()
            .map(|feature| (feature, 0.0))
            .collect();
        let source_times = Feature::ALL
            .iter()
            .copied()
            .map(|feature| (feature, source_time))
            .collect();
        MicrostructureSnapshot::with_metadata(candle, values, source_time, source_times)
    }

    fn decision_record(source_time: DateTime<Utc>) -> MicrostructureAuditRecord {
        let snapshot = snapshot(source_time);
        let mut strategy = EthUsdPerpMicrostructureMixed13::new();
        let _ = strategy.on_microstructure_snapshot(&snapshot);
        let summary = strategy.last_microstructure_decision_summary().unwrap();
        MicrostructureAuditRecord::decision(&snapshot, strategy.name(), &summary, "eth-test".into())
            .unwrap()
    }

    #[test]
    fn appends_hashed_records_and_verifies_them() {
        let dir = temp_dir("append");
        let logger = MicrostructureAuditLogger::new(dir.to_str().unwrap()).unwrap();
        let source_time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut decision = decision_record(source_time);
        logger.append(&mut decision).unwrap();
        let mut failure =
            MicrostructureAuditRecord::collection_error("mixed_13", "network\nfailed");
        logger.append(&mut failure).unwrap();

        let report = verify_audit_file(logger.path()).unwrap();
        assert!(report.is_valid(), "{:?}", report.failures);
        assert_eq!(report.total_records, 2);
        assert_eq!(report.decisions, 1);
        assert_eq!(report.collection_errors, 1);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn detects_a_future_feature_source_time() {
        let dir = temp_dir("future_source");
        let logger = MicrostructureAuditLogger::new(dir.to_str().unwrap()).unwrap();
        let source_time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut decision = decision_record(source_time);
        decision.feature_source_times.insert(
            Feature::SignalReturn6.as_str().to_string(),
            source_time + Duration::milliseconds(1),
        );
        logger.append(&mut decision).unwrap();

        let report = verify_audit_file(logger.path()).unwrap();
        assert!(!report.is_valid());
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("source future")));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn detects_a_tampered_record_hash() {
        let dir = temp_dir("tampered");
        let logger = MicrostructureAuditLogger::new(dir.to_str().unwrap()).unwrap();
        let source_time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut decision = decision_record(source_time);
        logger.append(&mut decision).unwrap();

        let path = logger.path().to_path_buf();
        let content = fs::read_to_string(&path).unwrap();
        fs::write(&path, content.replacen("eth-test", "eth-tampered", 1)).unwrap();

        let report = verify_audit_file(&path).unwrap();
        assert!(!report.is_valid());
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("record_hash invalide")));
        fs::remove_dir_all(dir).ok();
    }
}
