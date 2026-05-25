use chrono::{Duration, TimeZone, Utc};
use rusty_poly_signal_runner::logger::{PendingBuyTradeRecord, TradeLogger, TradeRecord};
use rusty_poly_signal_runner::trade_timing::TradeLatencies;
use std::fs;

fn make_record(trade_id: &str, prediction: &str) -> TradeRecord {
    TradeRecord {
        trade_id: trade_id.to_string(),
        signal_key: format!("sig-{}", trade_id),
        symbol: "BTCUSDT".to_string(),
        interval: "5m".to_string(),
        signal_close_time_utc: "2024-01-01T00:00:00+00:00".to_string(),
        target_candle_open_time_utc: "2024-01-01T00:05:00+00:00".to_string(),
        prediction: prediction.to_string(),
        entry_side: "BUY".to_string(),
        entry_order_type: "DRY_RUN".to_string(),
        order_status: "DRY_RUN".to_string(),
        signal_to_submit_start_ms: 10,
        submit_start_to_ack_ms: 5,
        signal_to_ack_ms: 15,
        trade_open_to_order_ack_ms: 20,
        outcome: "PENDING".to_string(),
    }
}

fn tmp_dir(label: &str) -> std::path::PathBuf {
    // Dossier unique par test pour éviter les conflits entre tests parallèles
    let dir = std::env::temp_dir().join(format!(
        "rusty_poly_signal_runner_test_{}_{}",
        label,
        uuid::Uuid::new_v4()
    ));
    dir
}

#[test]
fn test_pending_buy_trade_record_factory_sets_csv_fields() {
    let signal_close = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 0).unwrap();
    let target_open = signal_close + Duration::minutes(5);
    let latencies = TradeLatencies {
        signal_to_submit_start_ms: 1,
        submit_start_to_ack_ms: 2,
        signal_to_ack_ms: 3,
        trade_open_to_order_ack_ms: 4,
    };

    let record = TradeRecord::pending_buy(PendingBuyTradeRecord {
        trade_id: "trade-1",
        signal_key: "strategy:slug:UP",
        symbol: "btcusdt",
        interval: "5m",
        signal_close_time_utc: &signal_close,
        target_candle_open_time_utc: &target_open,
        prediction: "UP",
        entry_order_type: "DRY_RUN",
        order_status: "DRY_RUN",
        latencies,
    });

    assert_eq!(record.trade_id, "trade-1");
    assert_eq!(record.entry_side, "BUY");
    assert_eq!(record.outcome, "PENDING");
    assert_eq!(record.signal_to_ack_ms, 3);
    assert_eq!(record.target_candle_open_time_utc, target_open.to_rfc3339());
}

// --- Création du CSV ---

#[test]
fn test_logger_creates_csv_file() {
    let dir = tmp_dir("creates_csv");
    let _logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    assert!(
        dir.join("trades.csv").exists(),
        "Le fichier trades.csv doit être créé"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_logger_csv_contains_headers() {
    let dir = tmp_dir("headers");
    let _logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    let content = fs::read_to_string(dir.join("trades.csv")).unwrap();

    for header in &[
        "trade_id",
        "signal_key",
        "symbol",
        "interval",
        "prediction",
        "order_status",
        "outcome",
        "signal_to_ack_ms",
    ] {
        assert!(content.contains(header), "Header manquant: {}", header);
    }
    fs::remove_dir_all(&dir).ok();
}

/// P7 : si le fichier existe mais est vide (crash pendant init), les headers doivent être écrits
#[test]
fn test_logger_writes_headers_on_empty_existing_file() {
    let dir = tmp_dir("empty_file");
    fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("trades.csv");

    // Créer un fichier vide (simule un crash pendant l'initialisation précédente)
    fs::write(&csv_path, "").unwrap();
    assert_eq!(fs::metadata(&csv_path).unwrap().len(), 0);

    let _logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    let content = fs::read_to_string(&csv_path).unwrap();
    assert!(
        content.contains("trade_id"),
        "Les headers doivent être écrits sur un fichier vide"
    );
    fs::remove_dir_all(&dir).ok();
}

/// Si le CSV existe déjà avec des données, le logger ne doit pas réécrire les headers
#[test]
fn test_logger_does_not_overwrite_existing_data() {
    let dir = tmp_dir("no_overwrite");
    let logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();

    logger.log_trade(&make_record("id-first", "UP")).unwrap();
    let content_before = fs::read_to_string(dir.join("trades.csv")).unwrap();
    let line_count_before = content_before.lines().count();

    // Recréer le logger sur le même dossier
    let logger2 = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    logger2
        .log_trade(&make_record("id-second", "DOWN"))
        .unwrap();

    let content_after = fs::read_to_string(dir.join("trades.csv")).unwrap();
    let line_count_after = content_after.lines().count();

    assert_eq!(
        line_count_after,
        line_count_before + 1,
        "Une seule ligne doit être ajoutée"
    );
    assert!(
        !content_after.contains("trade_id\ntrade_id"),
        "Les headers ne doivent pas être dupliqués"
    );
    fs::remove_dir_all(&dir).ok();
}

// --- log_trade ---

#[test]
fn test_log_trade_appends_record() {
    let dir = tmp_dir("append");
    let logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();

    logger.log_trade(&make_record("test-id-123", "UP")).unwrap();

    let content = fs::read_to_string(dir.join("trades.csv")).unwrap();
    assert!(content.contains("test-id-123"));
    assert!(content.contains("BTCUSDT"));
    assert!(content.contains("UP"));
    assert!(content.contains("PENDING"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_log_trade_multiple_records_all_present() {
    let dir = tmp_dir("multiple");
    let logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();

    logger.log_trade(&make_record("id-001", "UP")).unwrap();
    logger.log_trade(&make_record("id-002", "DOWN")).unwrap();
    logger.log_trade(&make_record("id-003", "UP")).unwrap();

    let content = fs::read_to_string(dir.join("trades.csv")).unwrap();
    assert!(content.contains("id-001"));
    assert!(content.contains("id-002"));
    assert!(content.contains("id-003"));

    // 1 header + 3 records = 4 lignes
    assert_eq!(content.lines().count(), 4);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_log_trade_latency_fields_written() {
    let dir = tmp_dir("latency");
    let logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();

    let mut record = make_record("lat-test", "UP");
    record.signal_to_submit_start_ms = 42;
    record.submit_start_to_ack_ms = 17;
    record.signal_to_ack_ms = 59;
    record.trade_open_to_order_ack_ms = 310;

    logger.log_trade(&record).unwrap();

    let content = fs::read_to_string(dir.join("trades.csv")).unwrap();
    assert!(content.contains("42"));
    assert!(content.contains("17"));
    assert!(content.contains("59"));
    assert!(content.contains("310"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_has_signal_key_finds_existing_signal() {
    let dir = tmp_dir("signal_key");
    let logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    let record = make_record("signal-key-id", "UP");

    logger.log_trade(&record).unwrap();

    assert!(logger.has_signal_key(&record.signal_key).unwrap());
    assert!(!logger.has_signal_key("sig-missing").unwrap());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_logger_migrates_old_csv_schema() {
    let dir = tmp_dir("migrate_old_schema");
    fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("trades.csv");
    fs::write(
        &csv_path,
        "trade_id,symbol,interval,signal_close_time_utc,target_candle_open_time_utc,prediction,entry_side,entry_order_type,order_status,signal_to_submit_start_ms,submit_start_to_ack_ms,signal_to_ack_ms,trade_open_to_order_ack_ms,outcome\n\
old-id,BTCUSDT,5m,2024-01-01T00:00:00+00:00,2024-01-01T00:05:00+00:00,UP,BUY,MARKET,Matched,10,11,21,30,MATCHED\n",
    )
    .unwrap();

    let _logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    let content = fs::read_to_string(&csv_path).unwrap();
    let mut lines = content.lines();
    let header = lines.next().unwrap();
    let row = lines.next().unwrap();

    assert!(header.contains("signal_key"));
    assert_eq!(header.split(',').count(), 15);
    assert_eq!(row.split(',').count(), 15);
    assert!(row.starts_with("old-id,"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_logger_migrates_mixed_old_and_new_rows() {
    let dir = tmp_dir("migrate_mixed_schema");
    fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("trades.csv");
    fs::write(
        &csv_path,
        "trade_id,symbol,interval,signal_close_time_utc,target_candle_open_time_utc,prediction,entry_side,entry_order_type,order_status,signal_to_submit_start_ms,submit_start_to_ack_ms,signal_to_ack_ms,trade_open_to_order_ack_ms,outcome\n\
old-id,BTCUSDT,5m,2024-01-01T00:00:00+00:00,2024-01-01T00:05:00+00:00,UP,BUY,MARKET,Matched,10,11,21,30,MATCHED\n\
new-id,sig-new,BTCUSDT,5m,2024-01-01T00:10:00+00:00,2024-01-01T00:15:00+00:00,DOWN,BUY,MARKET,Matched,12,13,25,40,PENDING\n",
    )
    .unwrap();

    let logger = TradeLogger::new(dir.to_str().unwrap()).unwrap();
    assert!(logger.has_signal_key("sig-new").unwrap());
    logger.update_outcome("new-id", "MATCHED").unwrap();

    let content = fs::read_to_string(&csv_path).unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines[0].split(',').count(), 15);
    assert_eq!(lines[1].split(',').count(), 15);
    assert_eq!(lines[2].split(',').count(), 15);
    assert!(lines[2].ends_with("MATCHED"));
    fs::remove_dir_all(&dir).ok();
}
