use chrono::TimeZone;
use chrono::Utc;
use rusty_poly_streak_rsi::config::{
    Config, ExecutionMode, LimitPriceHighGuard, LimitPriceReference, MarketOrderType,
    PolymarketSlugFormat,
};
use rusty_poly_streak_rsi::polymarket::{
    calculate_available_shares_up_to_price, calculate_limit_order_quote, parse_best_ask_body,
    parse_best_bid_body, parse_book_reference_price_body, parse_gamma_market_body,
    parse_market_ws_best_ask_message, parse_market_ws_reference_price_message,
    parse_order_execution_details_body, parse_order_status_body, validate_sufficient_usdc_balance,
    MarketInfo, PolymarketClient,
};
use rusty_poly_streak_rsi::strategy::{Prediction, Signal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn make_config(mode: ExecutionMode) -> Config {
    Config {
        binance_ws_url: "wss://stream.binance.com:9443/ws".to_string(),
        symbol: "btcusdt".to_string(),
        interval: "5m".to_string(),
        execution_mode: mode,
        trade_amount_usdc: 10.0,
        polymarket_api_key: String::new(),
        polymarket_api_secret: String::new(),
        polymarket_api_url: "https://clob-v2.polymarket.com".to_string(),
        logs_dir: "logs".to_string(),
        evm_private_key: None,
        polymarket_funder: None,
        polymarket_signature_type: None,
        strategy: "three_candle_rsi7_reversal".to_string(),
        rsi_overbought: 65.0,
        rsi_oversold: 35.0,
        polymarket_slug_prefix: "btc-updown-5m".to_string(),
        polymarket_slug_format: PolymarketSlugFormat::Timestamp,
        polymarket_slug_asset: "bitcoin".to_string(),
        martingale_multiplier: 1.0,
        martingale_max_amount: 0.0,
        trade_amount_pct: 0.0,
        excluded_days: Vec::new(),
        excluded_hours: Vec::new(),
        ensemble_min_votes: 1,
        limit_price_reference: LimitPriceReference::BestAsk,
        limit_price_offset: 0.01,
        limit_price_high_guard: disabled_high_guard(),
        market_order_type: MarketOrderType::Fok,
    }
}

fn disabled_high_guard() -> LimitPriceHighGuard {
    LimitPriceHighGuard {
        enabled: false,
        threshold: 0.60,
        price: 0.55,
    }
}

fn make_signal(prediction: Prediction) -> Signal {
    Signal {
        prediction,
        signal_candle_close_time: Utc::now(),
        rsi: 72.0,
        strategy_name: "test".to_string(),
    }
}

fn make_market() -> MarketInfo {
    MarketInfo {
        condition_id: "cond_123".to_string(),
        up_token_id: "up_token".to_string(),
        down_token_id: "down_token".to_string(),
        slug: "btc-updown-5m-20240309".to_string(),
        order_min_size: 5.0,
    }
}

// --- build_slug ---

#[test]
fn test_build_slug_known_timestamp() {
    // 1710000000000 ms → Unix 1710000000 s
    let slug = PolymarketClient::build_slug("btc-updown-5m", 1710000000000);
    assert_eq!(slug, "btc-updown-5m-1710000000");
}

#[test]
fn test_build_slug_format_prefix() {
    let slug = PolymarketClient::build_slug("btc-updown-5m", 1710000000000);
    assert!(slug.starts_with("btc-updown-5m-"));
}

#[test]
fn test_build_slug_suffix_is_unix_seconds() {
    // Le suffixe est le timestamp en secondes (pas YYYYMMDD)
    let slug = PolymarketClient::build_slug("btc-updown-5m", 1710000000000);
    let suffix = slug.strip_prefix("btc-updown-5m-").unwrap();
    assert_eq!(suffix, "1710000000");
    assert!(suffix.chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn test_build_slug_ms_to_seconds_truncation() {
    // 1704067200000 ms → 1704067200 s
    let slug = PolymarketClient::build_slug("btc-updown-5m", 1704067200000);
    assert_eq!(slug, "btc-updown-5m-1704067200");
}

#[test]
fn test_build_slug_different_candles_produce_different_slugs() {
    // Deux bougies 5m consécutives (300 000 ms d'écart)
    let slug1 = PolymarketClient::build_slug("btc-updown-5m", 1710000000000);
    let slug2 = PolymarketClient::build_slug("btc-updown-5m", 1710000300000);
    assert_ne!(slug1, slug2);
}

#[test]
fn test_build_hourly_et_slug_formats_polymarket_hourly_market() {
    let open_time_ms = Utc
        .with_ymd_and_hms(2026, 5, 21, 17, 0, 0)
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        PolymarketClient::build_hourly_et_slug("solana", open_time_ms),
        Some("solana-up-or-down-may-21-2026-1pm-et".to_string())
    );
}

#[test]
fn test_build_hourly_et_slug_handles_morning_hour() {
    let open_time_ms = Utc
        .with_ymd_and_hms(2026, 5, 21, 15, 0, 0)
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        PolymarketClient::build_hourly_et_slug("bitcoin", open_time_ms),
        Some("bitcoin-up-or-down-may-21-2026-11am-et".to_string())
    );
}

#[test]
fn test_client_uses_configured_clob_api_url_without_trailing_slash() {
    let mut config = make_config(ExecutionMode::DryRun);
    config.polymarket_api_url = "https://example.test/".to_string();
    let client = PolymarketClient::new(config);
    assert_eq!(client.clob_api_base(), "https://example.test");
}

#[test]
fn test_client_uses_configured_api_bases_without_trailing_slash() {
    let config = make_config(ExecutionMode::DryRun);
    let client = PolymarketClient::new_with_api_bases(
        config,
        "http://127.0.0.1:1234/",
        "http://127.0.0.1:5678/",
    );
    assert_eq!(client.gamma_api_base(), "http://127.0.0.1:1234");
    assert_eq!(client.clob_api_base(), "http://127.0.0.1:5678");
}

#[test]
fn test_parse_gamma_market_body_maps_reversed_outcomes() {
    let body = r#"[{
        "conditionId":"cond",
        "outcomes":"[\"Down\",\"Up\"]",
        "clobTokenIds":"[\"down-token\",\"up-token\"]",
        "orderMinSize":7
    }]"#;

    let market = parse_gamma_market_body("btc-updown-5m-1710000000", body).unwrap();

    assert_eq!(market.condition_id, "cond");
    assert_eq!(market.up_token_id, "up-token");
    assert_eq!(market.down_token_id, "down-token");
    assert_eq!(market.order_min_size, 7.0);
}

#[test]
fn test_parse_gamma_market_body_rejects_missing_up() {
    let body = r#"[{
        "conditionId":"cond",
        "outcomes":"[\"Down\",\"Flat\"]",
        "clobTokenIds":"[\"down-token\",\"flat-token\"]"
    }]"#;

    let err = parse_gamma_market_body("slug", body).unwrap_err();
    assert!(err.to_string().contains("Outcome 'Up'"));
}

#[test]
fn test_parse_best_ask_body_returns_first_ask_price() {
    let body = r#"{"asks":[{"price":"0.41"},{"price":"0.42"}]}"#;

    assert_eq!(parse_best_ask_body(body), Some(0.41));
}

#[test]
fn test_parse_best_ask_body_returns_lowest_ask_price() {
    let body = r#"{"asks":[{"price":"0.99"},{"price":"0.52"},{"price":"0.51"}]}"#;

    assert_eq!(parse_best_ask_body(body), Some(0.51));
}

#[test]
fn test_parse_best_ask_body_returns_none_for_empty_book() {
    assert_eq!(parse_best_ask_body(r#"{"asks":[]}"#), None);
}

#[test]
fn test_parse_best_bid_body_returns_highest_bid_price() {
    let body = r#"{"bids":[{"price":"0.30"},{"price":"0.33"},{"price":"0.31"}]}"#;

    assert_eq!(parse_best_bid_body(body), Some(0.33));
}

#[test]
fn test_parse_book_reference_price_body_supports_best_bid() {
    let body = r#"{
        "asks":[{"price":"0.35"},{"price":"0.34"}],
        "bids":[{"price":"0.31"},{"price":"0.33"}]
    }"#;

    assert_eq!(
        parse_book_reference_price_body(body, LimitPriceReference::BestBid),
        Some(0.33)
    );
}

#[test]
fn test_parse_market_ws_best_ask_message_supports_best_bid_ask() {
    let body = r#"{
        "event_type":"best_bid_ask",
        "asset_id":"token-up",
        "best_bid":"0.50",
        "best_ask":"0.53"
    }"#;

    assert_eq!(
        parse_market_ws_best_ask_message("token-up", body),
        Some(0.53)
    );
}

#[test]
fn test_parse_market_ws_reference_price_message_supports_best_bid() {
    let body = r#"{
        "event_type":"best_bid_ask",
        "asset_id":"token-up",
        "best_bid":"0.50",
        "best_ask":"0.53"
    }"#;

    assert_eq!(
        parse_market_ws_reference_price_message("token-up", body, LimitPriceReference::BestBid),
        Some(0.50)
    );
}

#[test]
fn test_parse_market_ws_best_ask_message_supports_book_snapshot() {
    let body = r#"{
        "event_type":"book",
        "asset_id":"token-up",
        "asks":[{"price":"0.55"},{"price":"0.52"}]
    }"#;

    assert_eq!(
        parse_market_ws_best_ask_message("token-up", body),
        Some(0.52)
    );
}

#[test]
fn test_parse_market_ws_best_ask_message_supports_price_change() {
    let body = r#"{
        "event_type":"price_change",
        "price_changes":[
            {"asset_id":"token-down","best_ask":"0.60"},
            {"asset_id":"token-up","best_ask":"0.51"}
        ]
    }"#;

    assert_eq!(
        parse_market_ws_best_ask_message("token-up", body),
        Some(0.51)
    );
}

#[test]
fn test_parse_order_status_body_supports_nested_and_array_shapes() {
    assert_eq!(
        parse_order_status_body(r#"{"status":"matched"}"#).unwrap(),
        "matched"
    );
    assert_eq!(
        parse_order_status_body(r#"{"order":{"status":"filled"}}"#).unwrap(),
        "filled"
    );
    assert_eq!(
        parse_order_status_body(r#"[{"status":"cancelled"}]"#).unwrap(),
        "cancelled"
    );
}

#[test]
fn test_parse_order_execution_details_body_extracts_price_fields() {
    let details = parse_order_execution_details_body(
        r#"{"order":{"status":"matched","price":"0.52","average_price":"0.51","size_matched":"5"}}"#,
    )
    .unwrap();

    assert_eq!(details.status, "matched");
    assert_eq!(details.order_price, Some(0.52));
    assert_eq!(details.average_price, Some(0.51));
    assert_eq!(details.size_matched, Some(5.0));
}

#[test]
fn test_parse_order_status_body_rejects_missing_status_with_context() {
    let err = parse_order_status_body(r#"{"id":"order"}"#).unwrap_err();

    assert!(err.to_string().contains("order status absent"));
}

#[test]
fn test_calculate_limit_order_quote_uses_offset_and_caps_price() {
    let quote = calculate_limit_order_quote(10.0, 5.0, Some(0.985), 0.02, disabled_high_guard());

    assert_eq!(quote.limit_price, 0.99);
    assert!(!quote.adjusted_to_min_size);
    assert_eq!(quote.effective_usdc, 10.0);
}

#[test]
fn test_calculate_limit_order_quote_adjusts_to_min_size() {
    let quote = calculate_limit_order_quote(1.0, 5.0, Some(0.4), 0.01, disabled_high_guard());

    assert_eq!(quote.limit_price, 0.41000000000000003);
    assert_eq!(quote.effective_usdc, 2.06);
    assert!(quote.adjusted_to_min_size);
}

#[test]
fn test_calculate_limit_order_quote_accepts_negative_offset() {
    let quote = calculate_limit_order_quote(10.0, 5.0, Some(0.55), -0.02, disabled_high_guard());

    assert_eq!(quote.limit_price, 0.53);
}

#[test]
fn test_calculate_limit_order_quote_clamps_negative_offset_to_min_price() {
    let quote = calculate_limit_order_quote(10.0, 5.0, Some(0.02), -0.05, disabled_high_guard());

    assert_eq!(quote.limit_price, 0.01);
}

#[test]
fn test_calculate_limit_order_quote_applies_high_price_guard() {
    let quote = calculate_limit_order_quote(
        10.0,
        5.0,
        Some(0.62),
        0.01,
        LimitPriceHighGuard {
            enabled: true,
            threshold: 0.60,
            price: 0.55,
        },
    );

    assert_eq!(quote.uncapped_limit_price, 0.63);
    assert_eq!(quote.limit_price, 0.55);
    assert!(quote.high_guard_applied);
}

#[test]
fn test_calculate_limit_order_quote_does_not_apply_high_price_guard_at_threshold() {
    let quote = calculate_limit_order_quote(
        10.0,
        5.0,
        Some(0.59),
        0.01,
        LimitPriceHighGuard {
            enabled: true,
            threshold: 0.60,
            price: 0.55,
        },
    );

    assert_eq!(quote.limit_price, 0.6);
    assert!(!quote.high_guard_applied);
}

#[test]
fn test_calculate_available_shares_up_to_price_sums_fillable_depth() {
    let body = r#"{
        "asks":[
            {"price":"0.50","size":"2.5"},
            {"price":"0.51","size":"3.0"},
            {"price":"0.55","size":"10.0"}
        ]
    }"#;

    assert_eq!(
        calculate_available_shares_up_to_price(body, 0.51),
        Some(5.5)
    );
}

#[test]
fn test_validate_sufficient_usdc_balance_rejects_min_order_above_balance() {
    let err = validate_sufficient_usdc_balance(4.95, 2.27).unwrap_err();

    assert!(err.to_string().contains("solde USDC insuffisant"));
}

#[test]
fn test_validate_sufficient_usdc_balance_accepts_available_balance() {
    assert!(validate_sufficient_usdc_balance(4.95, 4.95).is_ok());
}

// --- place_order ---

#[tokio::test]
async fn test_place_order_dryrun_returns_ok() {
    let client = PolymarketClient::new(make_config(ExecutionMode::DryRun));
    let signal = make_signal(Prediction::Up);
    let market = make_market();

    let result = client.place_order(&signal, &market, 10.0).await;
    assert!(result.is_ok(), "DryRun doit retourner Ok");

    let order = result.unwrap();
    assert_eq!(order.status, "DRY_RUN");
    assert!(order.order_id.starts_with("dry-run-"));
}

#[tokio::test]
async fn test_place_order_dryrun_down_signal() {
    let client = PolymarketClient::new(make_config(ExecutionMode::DryRun));
    let signal = make_signal(Prediction::Down);
    let market = make_market();

    let result = client.place_order(&signal, &market, 10.0).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().status, "DRY_RUN");
}

/// P3 : en mode Market, place_order doit retourner Err (non implémenté en V2)
#[tokio::test]
async fn test_place_order_market_mode_returns_err() {
    let client = PolymarketClient::new(make_config(ExecutionMode::Market));
    let signal = make_signal(Prediction::Up);
    let market = make_market();

    let result = client.place_order(&signal, &market, 10.0).await;
    assert!(
        result.is_err(),
        "Mode Market non implémenté doit retourner Err"
    );
}

/// P3 : en mode Limit, place_order doit retourner Err (non implémenté en V2)
#[tokio::test]
async fn test_place_order_limit_mode_returns_err() {
    let client = PolymarketClient::new(make_config(ExecutionMode::Limit));
    let signal = make_signal(Prediction::Down);
    let market = make_market();

    let result = client.place_order(&signal, &market, 10.0).await;
    assert!(
        result.is_err(),
        "Mode Limit non implémenté doit retourner Err"
    );
}

/// Vérifie que ack_at >= submitted_at (pas de latence négative dans le dry-run)
#[tokio::test]
async fn test_place_order_dryrun_timestamps_ordered() {
    let client = PolymarketClient::new(make_config(ExecutionMode::DryRun));
    let signal = make_signal(Prediction::Up);
    let market = make_market();

    let before = Utc::now();
    let order = client.place_order(&signal, &market, 10.0).await.unwrap();
    assert!(
        order.ack_at >= before,
        "ack_at doit être >= au timestamp avant l'appel"
    );
    assert!(
        order.ack_at >= order.submitted_at,
        "ack_at doit être >= submitted_at"
    );
}

#[tokio::test]
async fn test_resolve_market_uses_configured_gamma_base_and_caches_result() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = r#"[{
        "conditionId":"cond-live",
        "outcomes":"[\"Up\",\"Down\"]",
        "clobTokenIds":"[\"up-live\",\"down-live\"]",
        "orderMinSize":5
    }]"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 1024];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]);
        assert!(request.contains("GET /markets?slug=btc-updown-5m-1710000000 "));
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let config = make_config(ExecutionMode::DryRun);
    let client = PolymarketClient::new_with_api_bases(config, format!("http://{}", addr), "");

    let first = client
        .resolve_market("btc-updown-5m-1710000000")
        .await
        .unwrap();
    let second = client
        .resolve_market("btc-updown-5m-1710000000")
        .await
        .unwrap();

    assert_eq!(first.condition_id, "cond-live");
    assert_eq!(first.up_token_id, "up-live");
    assert_eq!(second.down_token_id, "down-live");
    server.await.unwrap();
}
