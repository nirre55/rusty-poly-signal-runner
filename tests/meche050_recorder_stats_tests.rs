use std::fs;
use std::process::Command;

use chrono::Utc;
use rusty_poly_signal_runner::trajectory::{
    finalize_trajectory, upsert_trajectory_index, TrajectoryMetadata,
};
use serde_json::{json, Value};

#[tokio::test]
async fn compressed_shared_trajectory_generates_temporal_and_risk_reports() {
    let root = temporary_root("trajectory-reports");
    let logs = root.join("logs");
    let entry_time_ms = 1_700_000_000_000_i64;
    let signal_ids = vec![
        "btc_5m:1700000000000:boll_fade:up".to_string(),
        "btc_5m:1700000000000:trio_vote2:up".to_string(),
        "btc_5m:1700000000000:streak_rsi:down".to_string(),
    ];
    let source = logs.join("streams").join("trajectory-source.jsonl");
    write_jsonl(
        &source,
        &[
            envelope(
                entry_time_ms,
                "signal_snapshot",
                quote_payload("UP", entry_time_ms, 0.49, 0.50, 0.49, 8.0),
            ),
            envelope(
                entry_time_ms + 10_000,
                "quote",
                quote_payload("UP", entry_time_ms + 10_000, 0.48, 0.49, 0.47, 8.0),
            ),
            envelope(
                entry_time_ms + 25_000,
                "binance_quote",
                json!({"open":40_000.0,"close":40_100.0,"is_closed":false}),
            ),
            envelope(
                entry_time_ms + 25_000,
                "quote",
                quote_payload("UP", entry_time_ms + 25_000, 0.40, 0.42, 0.39, 20.0),
            ),
            envelope(
                entry_time_ms + 30_000,
                "quote",
                quote_payload("UP", entry_time_ms + 30_000, 0.52, 0.53, 0.51, 20.0),
            ),
        ],
    );
    let trajectory = finalize_trajectory(
        source,
        logs.join("trajectories/2023-11-14/session-shared.jsonl.zst"),
        TrajectoryMetadata {
            session_id: "session-shared".to_string(),
            market_slot: "btc_5m".to_string(),
            entry_time_ms,
            slug: "btc-updown-5m-shared".to_string(),
            signal_ids: signal_ids.clone(),
            completion_status: "RESOLVED_COMPLETE".to_string(),
            gap_count: 0,
        },
    )
    .await
    .unwrap();
    upsert_trajectory_index(&logs, trajectory).unwrap();

    write_jsonl(
        &logs.join("sessions.jsonl"),
        &[json!({
            "session_id":"session-shared",
            "market_slot":"btc_5m",
            "entry_time_ms":entry_time_ms,
            "slug":"btc-updown-5m-shared",
            "up_token_id":"up-token",
            "down_token_id":"down-token",
            "signal_ids":signal_ids,
            "completion_status":"RESOLVED_COMPLETE",
            "gap_count":0,
            "resolution":{"winning_asset_id":"up-token","winning_outcome":"UP"},
            "raw_stream_path":null
        })],
    );
    write_jsonl(
        &logs.join("signals.jsonl"),
        &[
            signal_record_at(
                "session-shared",
                "btc_5m:1700000000000:boll_fade:up",
                entry_time_ms,
            ),
            signal_record_at(
                "session-shared",
                "btc_5m:1700000000000:trio_vote2:up",
                entry_time_ms,
            ),
            signal_record_at(
                "session-shared",
                "btc_5m:1700000000000:streak_rsi:down",
                entry_time_ms,
            ),
        ],
    );
    write_jsonl(
        &logs.join("session_metrics.jsonl"),
        &[trajectory_metric(entry_time_ms)],
    );

    run(&logs, &["report"]);

    let temporal = read_json(&logs.join("stats/temporal/global_majority.json"));
    assert_eq!(temporal["overall"]["total_signals"], 1);
    assert_eq!(temporal["overall"]["crossed_below_0_50"], 1);
    assert_eq!(temporal["overall"]["time_to_cross_seconds"]["median"], 10.0);
    let boll = read_json(&logs.join("stats/temporal/boll_fade.json"));
    let trio = read_json(&logs.join("stats/temporal/trio_vote2.json"));
    assert_eq!(boll["overall"]["crossed_below_0_50"], 1);
    assert_eq!(trio["overall"]["crossed_below_0_50"], 1);

    let risk = read_json(&logs.join("stats/risk/global_majority.json"));
    assert_eq!(risk["schema_version"], 2);
    assert_approx(
        risk["overall"]["maximum_drawdown"]["maximum"]
            .as_f64()
            .unwrap(),
        0.08,
    );
    assert_approx(
        risk["overall"]["horizons"]["t15s"]["spread"]["median"]
            .as_f64()
            .unwrap(),
        0.02,
    );
    let winner_adverse = &risk["overall"]["winning_trade_adverse_excursion"];
    assert_approx(
        winner_adverse["lowest_best_bid"]["mean"].as_f64().unwrap(),
        0.40,
    );
    assert_approx(
        winner_adverse["drop_from_0_50"]["median"].as_f64().unwrap(),
        0.10,
    );
    assert_approx(
        winner_adverse["drop_pct_from_0_50"]["mean"]
            .as_f64()
            .unwrap(),
        20.0,
    );
    assert_approx(
        winner_adverse["unrealized_pnl_5_shares_usdc"]["median"]
            .as_f64()
            .unwrap(),
        -0.50,
    );
    assert_approx(
        winner_adverse["time_to_low_seconds"]["mean"]
            .as_f64()
            .unwrap(),
        15.0,
    );

    let all_signals = read_json(&logs.join("stats/risk/global_all_signals.json"));
    let boll_risk = read_json(&logs.join("stats/risk/boll_fade.json"));
    let trio_risk = read_json(&logs.join("stats/risk/trio_vote2.json"));
    let streak_risk = read_json(&logs.join("stats/risk/streak_rsi.json"));
    assert_eq!(
        (
            all_signals["overall"]["winning_trade_adverse_excursion"]["winning_trades"].as_u64(),
            boll_risk["overall"]["winning_trade_adverse_excursion"]["winning_trades"].as_u64(),
            boll_risk["by_market"]["btc_5m"]["winning_trade_adverse_excursion"]["winning_trades"]
                .as_u64(),
            trio_risk["overall"]["winning_trade_adverse_excursion"]["winning_trades"].as_u64(),
            streak_risk["overall"]["winning_trade_adverse_excursion"]["winning_trades"].as_u64(),
        ),
        (Some(2), Some(1), Some(1), Some(1), Some(0))
    );

    for scope in [
        "global_all_signals",
        "global_majority",
        "boll_fade",
        "streak_rsi",
        "trio_vote2",
        "reversal_pro",
    ] {
        assert!(logs.join(format!("stats/temporal/{scope}.json")).exists());
        assert!(logs.join(format!("stats/risk/{scope}.json")).exists());
    }

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn repair_index_and_backfill_work_from_compressed_trajectory_only() {
    let root = temporary_root("repair-index");
    let logs = root.join("logs");
    let entry_time_ms = 1_700_000_300_000_i64;
    let signal_id = "btc_5m:1700000300000:boll_fade:up";
    let source = logs.join("streams/source.jsonl");
    write_jsonl(
        &source,
        &[
            envelope(
                entry_time_ms,
                "signal_snapshot",
                quote_payload("UP", entry_time_ms, 0.48, 0.50, 0.47, 4.0),
            ),
            envelope(
                entry_time_ms + 250,
                "quote",
                quote_payload("UP", entry_time_ms + 250, 0.48, 0.49, 0.47, 6.0),
            ),
        ],
    );
    let _record = finalize_trajectory(
        source.clone(),
        logs.join("trajectories/2023-11-14/session-repair.jsonl.zst"),
        TrajectoryMetadata {
            session_id: "session-repair".to_string(),
            market_slot: "btc_5m".to_string(),
            entry_time_ms,
            slug: "btc-updown-5m-repair".to_string(),
            signal_ids: vec![signal_id.to_string()],
            completion_status: "RESOLVED_COMPLETE".to_string(),
            gap_count: 0,
        },
    )
    .await
    .unwrap();
    fs::remove_file(source).unwrap();
    write_jsonl(
        &logs.join("sessions.jsonl"),
        &[json!({
            "session_id":"session-repair",
            "market_slot":"btc_5m",
            "entry_time_ms":entry_time_ms,
            "slug":"btc-updown-5m-repair",
            "up_token_id":"up-token",
            "down_token_id":"down-token",
            "signal_ids":[signal_id],
            "resolution":{"winning_asset_id":"up-token","winning_outcome":"UP"},
            "completion_status":"RESOLVED_COMPLETE",
            "gap_count":0,
            "raw_stream_path":null
        })],
    );
    write_jsonl(
        &logs.join("signals.jsonl"),
        &[signal_record_at("session-repair", signal_id, entry_time_ms)],
    );
    write_jsonl(
        &logs.join("signal_sizing.jsonl"),
        &[json!({
            "signal_id":signal_id,
            "disposition":"DRY_RUN_ORDER_CANDIDATE",
            "details":{"combined_amount_usdc":2.5}
        })],
    );
    fs::write(
        logs.join("recorder_state.json"),
        br#"{"schema_version":1,"active_sessions":[]}"#,
    )
    .unwrap();

    run(&logs, &["repair-index"]);
    run(&logs, &["verify"]);
    run(&logs, &["backfill"]);

    let index = fs::read_to_string(logs.join("trajectory_index.jsonl")).unwrap();
    let indexed: Value = serde_json::from_str(index.trim()).unwrap();
    assert_eq!(indexed["signal_ids"], json!([signal_id]));
    let metrics: Value = serde_json::from_str(
        fs::read_to_string(logs.join("session_metrics.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(metrics["source_format"], "backfill_compact_v2");
    assert_eq!(metrics["outcomes"][1]["order_fill_result"], "WIN");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repair_index_finalizes_a_retained_raw_stream_after_interrupted_compression() {
    let root = temporary_root("repair-raw");
    let logs = root.join("logs");
    let entry_time_ms = 1_700_000_600_000_i64;
    let source = logs.join("streams/session-repair-raw.jsonl");
    write_jsonl(
        &source,
        &[envelope(
            entry_time_ms,
            "signal_snapshot",
            quote_payload("UP", entry_time_ms, 0.48, 0.49, 0.47, 6.0),
        )],
    );
    write_jsonl(
        &logs.join("sessions.jsonl"),
        &[json!({
            "session_id":"session-repair-raw",
            "market_slot":"btc_5m",
            "entry_time_ms":entry_time_ms,
            "slug":"btc-updown-5m-repair-raw",
            "up_token_id":"up-token",
            "down_token_id":"down-token",
            "signal_ids":["btc_5m:1700000600000:boll_fade:up"],
            "completion_status":"RESOLVED_COMPLETE",
            "gap_count":0,
            "raw_stream_path":source
        })],
    );

    run(&logs, &["repair-index"]);
    run(&logs, &["verify"]);

    assert!(source.exists());
    assert!(logs
        .join("trajectories/2023-11-14/session-repair-raw.jsonl.zst")
        .exists());
    assert!(logs.join("trajectory_index.jsonl").exists());

    fs::remove_dir_all(root).unwrap();
}

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
fn confirmed_purge_keeps_raw_stream_when_required_trajectory_is_missing() {
    let root = temporary_root("protected-purge");
    let logs = root.join("logs");
    let stream = logs.join("streams/session-protected.jsonl");
    write_jsonl(&stream, &[envelope(1_000, "signal_activated", json!({}))]);
    write_jsonl(
        &logs.join("sessions.jsonl"),
        &[json!({
            "session_id":"session-protected",
            "market_slot":"btc_5m",
            "entry_time_ms":1_000,
            "slug":"btc-updown-5m-protected",
            "up_token_id":"up-token",
            "down_token_id":"down-token",
            "raw_stream_path":stream
        })],
    );
    write_jsonl(
        &logs.join("session_metrics.jsonl"),
        &[metric_record(
            "session-protected",
            1_000,
            &["btc_5m:1000:boll_fade:up"],
            &[],
            0.49,
            0.60,
        )],
    );

    run_with_preserved_trajectories(&logs, &["purge", "--confirm"]);

    assert!(stream.exists());
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

#[test]
fn report_writes_strict_crossing_strategy_and_majority_files() {
    let root = std::env::temp_dir().join(format!(
        "meche050-minimal-stats-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let logs = root.join("logs");
    let tie_up = ["btc_5m:1000:boll_fade:up", "btc_5m:1000:trio_vote2:up"];
    let tie_down = [
        "btc_5m:1000:streak_rsi:down",
        "btc_5m:1000:reversal_pro:down",
    ];
    let majority_up = ["eth_5m:2000:boll_fade:up", "eth_5m:2000:trio_vote2:up"];
    let minority_down = ["eth_5m:2000:streak_rsi:down"];

    write_jsonl(
        &logs.join("session_metrics.jsonl"),
        &[
            metric_record("session-tie", 1_000, &tie_up, &tie_down, 0.49, 0.49),
            metric_record(
                "session-majority",
                2_000,
                &majority_up,
                &minority_down,
                0.49,
                0.50,
            ),
        ],
    );
    let signals = tie_up
        .iter()
        .chain(&tie_down)
        .map(|signal_id| signal_record("session-tie", signal_id))
        .chain(
            majority_up
                .iter()
                .chain(&minority_down)
                .map(|signal_id| signal_record("session-majority", signal_id)),
        )
        .collect::<Vec<_>>();
    write_jsonl(&logs.join("signals.jsonl"), &signals);

    run(&logs, &["report"]);

    assert_eq!(
        read_json(&logs.join("stats/global_all_signals.json")),
        json!({
            "total_signals":7,
            "trades_below_0_50":6,
            "wins_below_0_50":4,
            "losses_below_0_50":2,
            "missed_wins_no_below_0_50":0,
            "missed_losses_no_below_0_50":1
        })
    );
    assert_eq!(
        read_json(&logs.join("stats/global_majority.json")),
        json!({
            "total_signals":1,
            "trades_below_0_50":1,
            "wins_below_0_50":1,
            "losses_below_0_50":0,
            "missed_wins_no_below_0_50":0,
            "missed_losses_no_below_0_50":0,
            "trades_ignored_tie":1
        })
    );
    assert_eq!(
        read_json(&logs.join("stats/streak_rsi.json")),
        json!({
            "total_signals":2,
            "trades_below_0_50":1,
            "wins_below_0_50":0,
            "losses_below_0_50":1,
            "missed_wins_no_below_0_50":0,
            "missed_losses_no_below_0_50":1
        })
    );

    fs::remove_dir_all(root).unwrap();
}

fn metric_record(
    session_id: &str,
    entry_time_ms: i64,
    up_signal_ids: &[&str],
    down_signal_ids: &[&str],
    up_minimum_ask: f64,
    down_minimum_ask: f64,
) -> Value {
    json!({
        "schema_version":2,
        "record_type":"SESSION_METRICS",
        "generated_at":"1970-01-01T00:00:03Z",
        "source_format":"test",
        "analysis_complete":true,
        "session_id":session_id,
        "market_slot":"btc_5m",
        "entry_time_ms":entry_time_ms,
        "slug":session_id,
        "limit_price":0.5,
        "minimum_shares":5.0,
        "completion_status":"RESOLVED_COMPLETE",
        "gap_count":0,
        "reconnect_count":0,
        "resolution_winning_asset_id":"up-token",
        "resolution_winning_outcome":"UP",
        "outcomes":[
            outcome_record("DOWN", "down-token", down_signal_ids, down_minimum_ask, false),
            outcome_record("UP", "up-token", up_signal_ids, up_minimum_ask, true)
        ],
        "raw_stream_path":null
    })
}

fn outcome_record(
    outcome: &str,
    token_id: &str,
    signal_ids: &[&str],
    minimum_ask: f64,
    winning_outcome: bool,
) -> Value {
    json!({
        "outcome":outcome,
        "token_id":token_id,
        "signal_ids":signal_ids,
        "signal_at_unix_ms":1_000,
        "quote_at_signal":null,
        "quote_observation_count":1,
        "first_limit_touch":null,
        "first_minimum_fillable":null,
        "immediate_limit_touch":false,
        "immediate_minimum_fillable":false,
        "order_candidate":null,
        "min_best_ask":{
            "observed_at_unix_ms":1_100,
            "elapsed_from_signal_ms":100,
            "value":minimum_ask
        },
        "max_best_ask":null,
        "min_best_bid":null,
        "max_best_bid":null,
        "max_fillable_shares_at_limit":null,
        "checkpoints":{},
        "last_quote":null,
        "winning_outcome":winning_outcome,
        "minimum_fill_result":"NOT_FILLED",
        "minimum_fill_pnl_usdc":0.0,
        "order_fill_result":"NO_ORDER_CANDIDATE",
        "order_fill_pnl_usdc":null
    })
}

fn signal_record(session_id: &str, signal_id: &str) -> Value {
    signal_record_at(session_id, signal_id, 1_000)
}

fn signal_record_at(session_id: &str, signal_id: &str, detected_at_ms: i64) -> Value {
    let mut parts = signal_id.rsplit(':');
    let prediction = parts.next().unwrap().to_ascii_uppercase();
    let strategy = parts.next().unwrap();
    json!({
        "signal_id":signal_id,
        "session_id":session_id,
        "strategy":strategy,
        "market_slot":"btc_5m",
        "prediction":prediction,
        "detected_at_local":chrono::DateTime::<Utc>::from_timestamp_millis(detected_at_ms)
            .unwrap()
            .to_rfc3339()
    })
}

fn trajectory_metric(entry_time_ms: i64) -> Value {
    let up_signals = [
        "btc_5m:1700000000000:boll_fade:up",
        "btc_5m:1700000000000:trio_vote2:up",
    ];
    let down_signals = ["btc_5m:1700000000000:streak_rsi:down"];
    json!({
        "schema_version":2,
        "record_type":"SESSION_METRICS",
        "generated_at":"2023-11-14T22:13:21Z",
        "source_format":"runtime_compact_v2",
        "analysis_complete":true,
        "session_id":"session-shared",
        "market_slot":"btc_5m",
        "entry_time_ms":entry_time_ms,
        "slug":"btc-updown-5m-shared",
        "limit_price":0.5,
        "minimum_shares":5.0,
        "completion_status":"RESOLVED_COMPLETE",
        "gap_count":0,
        "reconnect_count":0,
        "resolution_winning_asset_id":"up-token",
        "resolution_winning_outcome":"UP",
        "outcomes":[
            outcome_record_at("DOWN", "down-token", &down_signals, entry_time_ms, 0.60, false),
            outcome_record_at("UP", "up-token", &up_signals, entry_time_ms, 0.49, true)
        ],
        "raw_stream_path":null
    })
}

fn outcome_record_at(
    outcome: &str,
    token_id: &str,
    signal_ids: &[&str],
    signal_at_ms: i64,
    minimum_ask: f64,
    winning_outcome: bool,
) -> Value {
    let mut value = outcome_record(outcome, token_id, signal_ids, minimum_ask, winning_outcome);
    value["signal_at_unix_ms"] = json!(signal_at_ms);
    value["min_best_ask"]["observed_at_unix_ms"] = json!(signal_at_ms + 10_000);
    value
}

fn quote_payload(
    outcome: &str,
    observed_at_ms: i64,
    bid: f64,
    ask: f64,
    sell_vwap_5: f64,
    ask_depth: f64,
) -> Value {
    json!({
        "outcome":outcome,
        "asset_id":"up-token",
        "observed_at_unix_ms":observed_at_ms,
        "best_bid":bid,
        "best_bid_size":10.0,
        "bid_shares_available":20.0,
        "best_ask":ask,
        "best_ask_size":ask_depth,
        "ask_shares_at_or_below_limit":ask_depth,
        "sell_vwap_5":sell_vwap_5,
        "sell_vwap_candidate":sell_vwap_5,
        "candidate_shares":5.0,
        "last_trade_price":(bid + ask) / 2.0
    })
}

fn temporary_root(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "meche050-{name}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ))
}

fn assert_approx(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
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
    run_with_trajectory_setting(logs, arguments, false);
}

fn run_with_preserved_trajectories(logs: &std::path::Path, arguments: &[&str]) {
    run_with_trajectory_setting(logs, arguments, true);
}

fn run_with_trajectory_setting(
    logs: &std::path::Path,
    arguments: &[&str],
    preserve_trajectories: bool,
) {
    let output = Command::new(env!("CARGO_BIN_EXE_meche050_recorder_stats"))
        .arg("--logs-dir")
        .arg(logs)
        .args(arguments)
        .env(
            "PORTFOLIO_RECORDER_PRESERVE_TRAJECTORIES",
            preserve_trajectories.to_string(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
