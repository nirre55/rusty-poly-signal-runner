use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use csv::{StringRecord, WriterBuilder};
use reqwest::Client;
use rusty_poly_signal_runner::config::Config;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: String,
    closed: bool,
}

#[derive(Debug, Deserialize)]
struct ClobMarket {
    tokens: Vec<ClobToken>,
}

#[derive(Debug, Deserialize)]
struct ClobToken {
    outcome: String,
    winner: bool,
}

#[derive(Debug)]
struct TradeRow {
    trade_id: String,
    signal_key: String,
    prediction: String,
    outcome: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let logs_dir = PathBuf::from(&config.logs_dir);
    let trades_path = logs_dir.join("trades.csv");
    let report_path = logs_dir.join("reconciliation_report.csv");

    let trades = read_trades(&trades_path)?;
    let client = Client::new();
    let mut rows = Vec::new();

    for trade in trades {
        let Some(slug) = extract_updown_slug(&trade.signal_key) else {
            continue;
        };

        let checked_at = Utc::now().to_rfc3339();
        let row = match fetch_official_winner(&client, &slug).await {
            Ok(Some(winner)) => {
                let official_outcome = if winner.eq_ignore_ascii_case(&trade.prediction) {
                    "WIN"
                } else {
                    "LOSS"
                };
                let reconciliation = if official_outcome == trade.outcome {
                    "MATCH"
                } else {
                    "MISMATCH"
                };
                vec![
                    checked_at,
                    trade.trade_id,
                    trade.signal_key,
                    slug,
                    trade.prediction,
                    trade.outcome,
                    winner,
                    official_outcome.to_string(),
                    reconciliation.to_string(),
                    String::new(),
                ]
            }
            Ok(None) => vec![
                checked_at,
                trade.trade_id,
                trade.signal_key,
                slug,
                trade.prediction,
                trade.outcome,
                String::new(),
                String::new(),
                "PENDING".to_string(),
                "market not closed or winner unavailable".to_string(),
            ],
            Err(e) => vec![
                checked_at,
                trade.trade_id,
                trade.signal_key,
                slug,
                trade.prediction,
                trade.outcome,
                String::new(),
                String::new(),
                "ERROR".to_string(),
                e.to_string(),
            ],
        };
        rows.push(row);
    }

    write_report(&report_path, &rows)?;

    let mismatches = rows
        .iter()
        .filter(|row| row.get(8).map(String::as_str) == Some("MISMATCH"))
        .count();
    let pending = rows
        .iter()
        .filter(|row| row.get(8).map(String::as_str) == Some("PENDING"))
        .count();
    let errors = rows
        .iter()
        .filter(|row| row.get(8).map(String::as_str) == Some("ERROR"))
        .count();

    println!(
        "Reconciliation complete | checked={} mismatches={} pending={} errors={} report={}",
        rows.len(),
        mismatches,
        pending,
        errors,
        report_path.display()
    );

    Ok(())
}

fn read_trades(path: &Path) -> Result<Vec<TradeRow>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(content.as_bytes());
    let headers = rdr.headers()?.clone();
    let trade_id_col = required_col(&headers, "trade_id")?;
    let signal_key_col = required_col(&headers, "signal_key")?;
    let prediction_col = required_col(&headers, "prediction")?;
    let outcome_col = required_col(&headers, "outcome")?;

    let mut trades = Vec::new();
    for record in rdr.records() {
        let record = record?;
        trades.push(TradeRow {
            trade_id: field(&record, trade_id_col),
            signal_key: field(&record, signal_key_col),
            prediction: field(&record, prediction_col).to_ascii_uppercase(),
            outcome: field(&record, outcome_col).to_ascii_uppercase(),
        });
    }
    Ok(trades)
}

fn required_col(headers: &StringRecord, name: &str) -> Result<usize> {
    headers
        .iter()
        .position(|header| header == name)
        .ok_or_else(|| anyhow!("missing '{}' column", name))
}

fn field(record: &StringRecord, index: usize) -> String {
    record.get(index).unwrap_or_default().trim().to_string()
}

fn extract_updown_slug(signal_key: &str) -> Option<String> {
    let slug = signal_key.split(':').nth(1)?.trim().to_ascii_lowercase();
    if slug.contains("btc-updown-") || slug.contains("eth-updown-") {
        Some(slug)
    } else {
        None
    }
}

async fn fetch_official_winner(client: &Client, slug: &str) -> Result<Option<String>> {
    let gamma_url = format!("https://gamma-api.polymarket.com/markets/slug/{slug}");
    let gamma = client
        .get(gamma_url)
        .send()
        .await?
        .error_for_status()?
        .json::<GammaMarket>()
        .await?;

    if !gamma.closed {
        return Ok(None);
    }

    let clob_url = format!("https://clob.polymarket.com/markets/{}", gamma.condition_id);
    let clob = client
        .get(clob_url)
        .send()
        .await?
        .error_for_status()?
        .json::<ClobMarket>()
        .await?;

    Ok(clob
        .tokens
        .into_iter()
        .find(|token| token.winner)
        .map(|token| token.outcome.to_ascii_uppercase()))
}

fn write_report(path: &Path, rows: &[Vec<String>]) -> Result<()> {
    let file_exists = path.exists() && fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut wtr = WriterBuilder::new().has_headers(false).from_writer(file);
    if !file_exists {
        wtr.write_record([
            "checked_at_utc",
            "trade_id",
            "signal_key",
            "slug",
            "prediction",
            "binance_outcome",
            "official_winner",
            "official_outcome",
            "reconciliation",
            "note",
        ])?;
    }
    for row in rows {
        wtr.write_record(row)?;
    }
    wtr.flush()?;
    Ok(())
}
