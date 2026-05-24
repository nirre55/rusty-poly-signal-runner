use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusty_poly_streak_rsi::binance::Candle;
use rusty_poly_streak_rsi::config::{
    Config, ExecutionMode, LimitPriceHighGuard, LimitPriceReference, MarketOrderType,
    PolymarketSlugFormat,
};
use rusty_poly_streak_rsi::logger::TradeLogger;
use rusty_poly_streak_rsi::money::MoneyManager;
use rusty_poly_streak_rsi::polymarket::{MarketInfo, OrderResult};
use rusty_poly_streak_rsi::runtime_metrics::RuntimeMetrics;
use rusty_poly_streak_rsi::strategy::{Prediction, Signal, Strategy};
use rusty_poly_streak_rsi::tracker::{PolymarketReadClient, PositionTracker};
use rusty_poly_streak_rsi::trading_runtime::{
    process_closed_candle, ClosedCandleAction, PolymarketTradingClient, RuntimeState,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

fn tmp_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "rusty_poly_streak_rsi_runtime_test_{}_{}",
        label,
        uuid::Uuid::new_v4()
    ))
}

fn make_config(logs_dir: &str) -> Config {
    Config {
        binance_ws_url: "wss://stream.binance.com:9443/ws".to_string(),
        symbol: "btcusdt".to_string(),
        interval: "5m".to_string(),
        execution_mode: ExecutionMode::DryRun,
        trade_amount_usdc: 10.0,
        polymarket_api_key: String::new(),
        polymarket_api_secret: String::new(),
        polymarket_api_url: "https://clob.polymarket.com".to_string(),
        logs_dir: logs_dir.to_string(),
        evm_private_key: None,
        polymarket_funder: None,
        polymarket_signature_type: None,
        strategy: "fixed_test_strategy".to_string(),
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
        limit_price_high_guard: LimitPriceHighGuard {
            enabled: false,
            threshold: 0.60,
            price: 0.55,
        },
        market_order_type: MarketOrderType::Fok,
    }
}

fn make_candle(close_time: DateTime<Utc>) -> Candle {
    Candle {
        open_time: close_time - Duration::minutes(5),
        close_time,
        open: 100.0,
        high: 103.0,
        low: 99.0,
        close: 102.0,
        volume: 1_000.0,
        is_closed: true,
    }
}

struct FixedSignalStrategy {
    emit: bool,
}

impl Strategy for FixedSignalStrategy {
    fn name(&self) -> &str {
        "fixed_test_strategy"
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        if !self.emit {
            return None;
        }
        self.emit = false;
        Some(Signal {
            prediction: Prediction::Up,
            signal_candle_close_time: candle.close_time,
            rsi: 72.0,
            strategy_name: self.name().to_string(),
        })
    }

    fn warmup(&mut self, _candle: &Candle) {}
    fn current_rsi(&self) -> Option<f64> {
        Some(72.0)
    }
    fn current_series(&self) -> Option<bool> {
        Some(true)
    }
    fn current_atr(&self) -> Option<f64> {
        Some(1.0)
    }
    fn candle_log_extras(&self) -> String {
        "test=true".to_string()
    }
}

struct MockRuntimePolymarketClient;

impl MockRuntimePolymarketClient {
    fn market(slug: &str) -> MarketInfo {
        MarketInfo {
            condition_id: "condition".to_string(),
            up_token_id: "111".to_string(),
            down_token_id: "222".to_string(),
            slug: slug.to_string(),
            order_min_size: 5.0,
        }
    }
}

impl PolymarketTradingClient for MockRuntimePolymarketClient {
    fn resolve_market<'a>(
        &'a self,
        slug: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<MarketInfo>> + Send + 'a>> {
        Box::pin(async move { Ok(Self::market(slug)) })
    }

    fn place_order<'a>(
        &'a self,
        _signal: &'a Signal,
        _market: &'a MarketInfo,
        amount_usdc: f64,
    ) -> Pin<Box<dyn Future<Output = Result<OrderResult>> + Send + 'a>> {
        Box::pin(async move {
            Ok(OrderResult {
                order_id: "dry-run-test-order".to_string(),
                status: "DRY_RUN".to_string(),
                amount_usdc,
                limit_price: None,
                execution_price: None,
                execution_price_source: None,
                size_matched: None,
                submitted_at: Utc::now(),
                ack_at: Utc::now(),
            })
        })
    }

    fn warm_sdk_caches<'a>(
        &'a self,
        _market: &'a MarketInfo,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }
}

impl PolymarketReadClient for MockRuntimePolymarketClient {
    fn get_order_status<'a>(
        &'a self,
        _order_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move { Ok("DRY_RUN".to_string()) })
    }

    fn get_usdc_balance<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<f64>> + Send + 'a>> {
        Box::pin(async move { Ok(10.0) })
    }
}

#[tokio::test]
async fn dry_run_closed_candle_flow_writes_trade_and_skips_tracker_pending() {
    let dir = tmp_dir("dryrun_flow");
    std::fs::create_dir_all(&dir).unwrap();

    let config = make_config(dir.to_str().unwrap());
    let logger = Arc::new(TradeLogger::new(dir.to_str().unwrap()).unwrap());
    let mock_client = Arc::new(MockRuntimePolymarketClient);
    let trading_client: Arc<dyn PolymarketTradingClient> = mock_client.clone();
    let tracker_client: Arc<dyn PolymarketReadClient> = mock_client;
    let money = Arc::new(tokio::sync::Mutex::new(MoneyManager::new(
        10.0,
        1.0,
        0.0,
        dir.to_str().unwrap(),
    )));
    let tracker = Arc::new(PositionTracker::new(
        tracker_client,
        logger.clone(),
        money.clone(),
        dir.to_str().unwrap(),
        0.0,
    ));
    let state = RuntimeState {
        trade_logger: logger,
        poly_client: trading_client,
        money_manager: money,
        tracker: tracker.clone(),
        metrics: Arc::new(RuntimeMetrics::default()),
    };
    let close_time = Utc.with_ymd_and_hms(2026, 1, 1, 0, 5, 0).unwrap();
    let candle = make_candle(close_time);
    let mut strategy = FixedSignalStrategy { emit: true };

    let action = process_closed_candle(
        &config,
        Duration::minutes(5),
        &mut strategy,
        &state,
        &candle,
    )
    .await;

    assert!(matches!(action, ClosedCandleAction::OrderPlaced { .. }));
    assert_eq!(state.metrics.snapshot().order_placed, 1);
    assert_eq!(tracker.pending_count().await, 0);

    let csv = std::fs::read_to_string(dir.join("trades.csv")).unwrap();
    assert!(csv.contains("fixed_test_strategy"));
    assert!(csv.contains("DRY_RUN"));
    assert!(csv.contains("PENDING"));

    std::fs::remove_dir_all(&dir).ok();
}
