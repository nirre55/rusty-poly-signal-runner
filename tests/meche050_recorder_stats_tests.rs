use std::fs;
use std::process::Command;

use chrono::Utc;
use serde_json::{json, Value};

#[test]
fn backfill_report_and_confirmed_purge_preserve_metrics() {
    let root = std::env::temp_dir().join(format!(
        "meche050-recorder-stats-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let logs = root.join("logs");
    let stream = logs
        .join("streams")
        .join("2026-08-12")
        .join("btc_5m")
        .join("market.jsonl");
    fs::create_dir_all(stream.parent().unwrap()).unwrap();

    write_jsonl(
        &logs.join("sessions.jsonl"),
        &[json!({
            "session_id":"session-1",
            "market_slot":"btc_5m",
            "entry_time_ms":1_000,
            "slug":"btc-updown-5m-1",
            "up_token_id":"up-token",
            "down_token_id":"down-token",
            "resolution":{
                "winning_asset_id":"up-token",
                "winning_outcome":"Up"
            },
            "raw_stream_path":stream,
        })],
    );
    write_jsonl(
        &logs.join("signals.jsonl"),
        &[json!({
            "signal_id":"btc_5m:1000:boll_fade:up",
            "session_id":"session-1",
            "strategy":"boll_fade",
            "market_slot":"btc_5m",
            "prediction":"UP",
            "detected_at_local":"1970-01-01T00:00:01Z"
        })],
    );
    write_jsonl(
        &logs.join("signal_sizing.jsonl"),
        &[json!({
            "signal_id":"btc_5m:1000:boll_fade:up",
            "disposition":"DRY_RUN_ORDER_CANDIDATE",
            "details":{"combined_amount_usdc":2.5}
        })],
    );
    write_jsonl(
        &stream,
        &[
            envelope(
                900,
                "book",
                json!({
                    "event_type":"book",
                    "asset_id":"up-token",
                    "bids":[{"price":"0.48","size":"10"}],
                    "asks":[{"price":"0.50","size":"4"}]
                }),
            ),
            envelope(1_000, "signal_activated", json!({"signal_ids":[]})),
            envelope(
                1_250,
                "price_change",
                json!({
                    "event_type":"price_change",
                    "price_changes":[{
                        "asset_id":"up-token",
                        "price":"0.50",
                        "size":"6",
                        "side":"SELL",
                        "best_bid":"0.48",
                        "best_ask":"0.50"
                    }]
                }),
            ),
        ],
    );
    fs::write(
        logs.join("recorder_state.json"),
        br#"{"schema_version":1,"active_sessions":[]}"#,
    )
    .unwrap();

    run(&logs, &["backfill"]);
    let metrics: Value = serde_json::from_str(
        fs::read_to_string(logs.join("session_metrics.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(
        metrics["outcomes"][1]["first_minimum_fillable"]["elapsed_from_signal_ms"],
        250
    );
    assert_eq!(metrics["outcomes"][1]["order_fill_result"], "WIN");

    run(&logs, &["report"]);
    assert!(logs.join("stats_summary.json").exists());
    run(&logs, &["purge"]);
    assert!(stream.exists());
    run(&logs, &["purge", "--confirm"]);
    assert!(!stream.exists());
    assert!(logs.join("stream_cleanup.jsonl").exists());
    assert!(logs.join("session_metrics.jsonl").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn incomplete_resume_metrics_are_rebuilt_from_compact_stream() {
    let root = std::env::temp_dir().join(format!(
        "meche050-recorder-resume-stats-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let logs = root.join("logs");
    let stream = logs.join("streams").join("compact.jsonl");
    fs::create_dir_all(stream.parent().unwrap()).unwrap();

    write_jsonl(
        &logs.join("sessions.jsonl"),
        &[json!({
            "session_id":"session-compact",
            "market_slot":"btc_5m",
            "entry_time_ms":1_000,
            "slug":"btc-updown-5m-1",
            "up_token_id":"up-token",
            "down_token_id":"down-token",
            "completion_status":"RESOLVED_WITH_GAPS",
            "gap_count":1,
            "resolution":{"winning_asset_id":"up-token","winning_outcome":"Up"},
            "raw_stream_path":stream,
        })],
    );
    write_jsonl(
        &logs.join("signals.jsonl"),
        &[json!({
            "signal_id":"btc_5m:1000:boll_fade:up",
            "session_id":"session-compact",
            "strategy":"boll_fade",
            "market_slot":"btc_5m",
            "prediction":"UP",
            "detected_at_local":"1970-01-01T00:00:01Z"
        })],
    );
    write_jsonl(
        &logs.join("signal_sizing.jsonl"),
        &[json!({
            "signal_id":"btc_5m:1000:boll_fade:up",
            "disposition":"DRY_RUN_ORDER_CANDIDATE",
            "details":{"combined_amount_usdc":2.5}
        })],
    );
    write_jsonl(
        &logs.join("session_metrics.jsonl"),
        &[json!({
            "schema_version":2,
            "record_type":"SESSION_METRICS",
            "generated_at":"1970-01-01T00:00:02Z",
            "source_format":"runtime_compact_v2_resumed",
            "analysis_complete":false,
            "session_id":"session-compact",
            "market_slot":"btc_5m",
            "entry_time_ms":1_000,
            "slug":"btc-updown-5m-1",
            "limit_price":0.5,
            "minimum_shares":5.0,
            "completion_status":"RESOLVED_WITH_GAPS",
            "gap_count":1,
            "reconnect_count":0,
            "resolution_winning_asset_id":"up-token",
            "resolution_winning_outcome":"Up",
            "outcomes":[],
            "raw_stream_path":stream,
        })],
    );
    write_jsonl(
        &stream,
        &[
            envelope(
                1_000,
                "signal_snapshot",
                json!({
                    "outcome":"UP",
                    "asset_id":"up-token",
                    "best_bid":0.48,
                    "best_ask":0.50,
                    "ask_shares_at_or_below_limit":4.0
                }),
            ),
            envelope(
                1_250,
                "quote",
                json!({
                    "outcome":"UP",
                    "asset_id":"up-token",
                    "best_bid":0.49,
                    "best_ask":0.50,
                    "ask_shares_at_or_below_limit":6.0
                }),
            ),
        ],
    );
    fs::write(
        logs.join("recorder_state.json"),
        br#"{"schema_version":1,"active_sessions":[]}"#,
    )
    .unwrap();

    run(&logs, &["backfill"]);
    let metrics = fs::read_to_string(logs.join("session_metrics.jsonl")).unwrap();
    let records = metrics
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(records.len(), 2);
    let rebuilt = records.last().unwrap();
    assert_eq!(rebuilt["analysis_complete"], true);
    assert_eq!(rebuilt["source_format"], "backfill_compact_v2");
    assert_eq!(
        rebuilt["outcomes"][1]["order_candidate"]["first_fully_fillable"]["elapsed_from_signal_ms"],
        250
    );

    run(&logs, &["report"]);
    let report: Value =
        serde_json::from_slice(&fs::read(logs.join("stats_summary.json")).unwrap()).unwrap();
    assert_eq!(report["session_metrics_count"], 1);
    run(&logs, &["purge", "--confirm"]);
    assert!(!stream.exists());

    fs::remove_dir_all(root).unwrap();
}

fn envelope(received_at_unix_ms: i64, event_type: &str, payload: Value) -> Value {
    json!({
        "received_at_unix_ms":received_at_unix_ms,
        "event_type":event_type,
        "payload":payload,
    })
}

fn write_jsonl(path: &std::path::Path, values: &[Value]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let body = values
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    fs::write(path, format!("{body}\n")).unwrap();
}

fn run(logs: &std::path::Path, arguments: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_meche050_recorder_stats"))
        .arg("--logs-dir")
        .arg(logs)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
