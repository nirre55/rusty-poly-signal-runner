use chrono::Utc;
use rusty_poly_signal_runner::binance::{parse_klines, Candle};
use serde_json::json;

fn make_kline_row(
    open_ms: i64,
    close_ms: i64,
    o: &str,
    h: &str,
    l: &str,
    c: &str,
    v: &str,
) -> serde_json::Value {
    json!([open_ms, o, h, l, c, v, close_ms, "0", 0, "0", "0", "0"])
}

fn make_candle(open: f64, close: f64) -> Candle {
    Candle {
        open_time: Utc::now(),
        close_time: Utc::now(),
        open,
        high: open.max(close) + 1.0,
        low: open.min(close) - 1.0,
        close,
        volume: 1.0,
        is_closed: true,
    }
}

// --- Candle ---

#[test]
fn test_candle_green() {
    let c = make_candle(100.0, 110.0);
    assert!(c.is_green());
    assert!(!c.is_red());
}

#[test]
fn test_candle_red() {
    let c = make_candle(110.0, 100.0);
    assert!(c.is_red());
    assert!(!c.is_green());
}

#[test]
fn test_candle_doji_counted_as_green() {
    // close == open → is_green=true, is_red=false (comportement documenté)
    let doji = make_candle(100.0, 100.0);
    assert!(doji.is_green());
    assert!(!doji.is_red());
}

// --- parse_klines ---

#[test]
fn test_parse_klines_valid() {
    let rows = vec![
        make_kline_row(
            1_000_000, 1_299_999, "100.0", "110.0", "90.0", "105.0", "500.0",
        ),
        make_kline_row(
            1_300_000, 1_599_999, "105.0", "115.0", "95.0", "95.0", "600.0",
        ),
    ];
    let candles = parse_klines(rows);
    assert_eq!(candles.len(), 2);

    assert_eq!(candles[0].open, 100.0);
    assert_eq!(candles[0].high, 110.0);
    assert_eq!(candles[0].low, 90.0);
    assert_eq!(candles[0].close, 105.0);
    assert_eq!(candles[0].volume, 500.0);
    assert!(candles[0].is_closed);

    assert!(candles[0].is_green()); // close(105) >= open(100)
    assert!(candles[1].is_red()); // close(95) < open(105)
}

#[test]
fn test_parse_klines_ignores_malformed_rows() {
    let rows = vec![
        json!("not an array"),
        json!(null),
        json!([1_000_000]), // tableau trop court
        make_kline_row(
            1_000_000, 1_299_999, "100.0", "110.0", "90.0", "105.0", "500.0",
        ),
    ];
    let candles = parse_klines(rows);
    assert_eq!(candles.len(), 1, "Seule la ligne valide doit être parsée");
}

#[test]
fn test_parse_klines_empty() {
    assert!(parse_klines(vec![]).is_empty());
}

#[test]
fn test_parse_klines_timestamps() {
    let open_ms = 1_710_000_000_000i64;
    let close_ms = 1_710_000_299_999i64;
    let rows = vec![make_kline_row(
        open_ms, close_ms, "50000.0", "51000.0", "49000.0", "50500.0", "1.5",
    )];
    let candles = parse_klines(rows);
    assert_eq!(candles[0].open_time.timestamp_millis(), open_ms);
    assert_eq!(candles[0].close_time.timestamp_millis(), close_ms);
}

/// P2 : un timestamp invalide doit éliminer la bougie (pas d'epoch 1970 silencieuse)
#[test]
fn test_parse_klines_invalid_timestamp_skipped() {
    let bad_ts = i64::MAX; // hors plage DateTime
    let rows = vec![
        json!([
            bad_ts,
            "100.0",
            "110.0",
            "90.0",
            "105.0",
            "500.0",
            1_299_999i64,
            "0",
            0,
            "0",
            "0",
            "0"
        ]),
        make_kline_row(
            1_000_000, 1_299_999, "100.0", "110.0", "90.0", "105.0", "500.0",
        ),
    ];
    let candles = parse_klines(rows);
    assert_eq!(
        candles.len(),
        1,
        "La bougie avec timestamp invalide doit être ignorée"
    );
    assert_ne!(
        candles[0].open_time.timestamp_millis(),
        0,
        "Aucune bougie epoch 1970 ne doit être produite"
    );
}
