use anyhow::{anyhow, Result};
use chrono::{Duration, TimeZone, Utc};
use rusty_poly_signal_runner::binance::Candle;
use rusty_poly_signal_runner::config::Config;
use rusty_poly_signal_runner::logger::TradeLogger;
use rusty_poly_signal_runner::microstructure::{Feature, MicrostructureSnapshot};
use rusty_poly_signal_runner::money::MoneyManager;
use rusty_poly_signal_runner::polymarket::{MarketInfo, OrderResult};
use rusty_poly_signal_runner::runtime_metrics::RuntimeMetrics;
use rusty_poly_signal_runner::strategy::{
    MicrostructureDecisionSummary, Prediction, Signal, Strategy,
};
use rusty_poly_signal_runner::tracker::{PolymarketReadClient, PositionTracker};
use rusty_poly_signal_runner::trading_runtime::{
    process_microstructure_snapshot, ClosedCandleAction, PolymarketTradingClient, RuntimeState,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty_poly_signal_runner_microstructure_audit_runtime_{}",
        uuid::Uuid::new_v4()
    ))
}

static CONFIG_ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

struct EnvironmentRestore {
    original_values: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvironmentRestore {
    fn set(variables: &[(&'static str, &str)]) -> Self {
        let original_values = variables
            .iter()
            .map(|(key, value)| {
                let original = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, original)
            })
            .collect();
        Self { original_values }
    }
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        for (key, original) in &self.original_values {
            match original {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn config(logs_dir: &str) -> Config {
    let _guard = CONFIG_ENV_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _restore = EnvironmentRestore::set(&[
        ("EXECUTION_MODE", "dry-run"),
        ("TRADE_AMOUNT_USDC", "10"),
        ("TRADE_AMOUNT_PCT", "0"),
        ("SYMBOL", "ethusdt"),
        ("INTERVAL", "15m"),
        ("LOGS_DIR", logs_dir),
        ("STRATEGY", "audit_signal_strategy"),
        ("POLYMARKET_SLUG_PREFIX", "eth-updown-15m"),
        ("POLYMARKET_SLUG_FORMAT", "timestamp"),
        ("POLYMARKET_SLUG_ASSET", "ethereum"),
        ("EXCLUDED_DAYS", ""),
        ("EXCLUDED_HOURS", ""),
        ("STRATEGY_CONFIG", ""),
    ]);

    Config::from_env().unwrap()
}

struct AuditSignalStrategy {
    summary: Option<MicrostructureDecisionSummary>,
}

impl Strategy for AuditSignalStrategy {
    fn name(&self) -> &str {
        "audit_signal_strategy"
    }

    fn on_closed_candle(&mut self, _candle: &Candle) -> Option<Signal> {
        None
    }

    fn on_microstructure_snapshot(&mut self, snapshot: &MicrostructureSnapshot) -> Option<Signal> {
        self.summary = Some(MicrostructureDecisionSummary {
            prediction: Some(Prediction::Up),
            green_votes: 1,
            red_votes: 0,
            active_rules: vec!["test_rule".to_string()],
        });
        Some(Signal {
            prediction: Prediction::Up,
            signal_candle_close_time: snapshot.candle().close_time,
            rsi: 100.0,
            strategy_name: self.name().to_string(),
        })
    }

    fn last_microstructure_decision_summary(&self) -> Option<MicrostructureDecisionSummary> {
        self.summary.clone()
    }

    fn warmup(&mut self, _candle: &Candle) {}

    fn current_rsi(&self) -> Option<f64> {
        None
    }

    fn current_series(&self) -> Option<bool> {
        None
    }

    fn current_atr(&self) -> Option<f64> {
        None
    }

    fn candle_log_extras(&self) -> String {
        "audit=true".to_string()
    }
}

struct AuditOrderClient {
    audit_path: PathBuf,
    resolution_saw_audit: Mutex<bool>,
    resolution_calls: Mutex<u32>,
}

impl AuditOrderClient {
    fn market(slug: &str) -> MarketInfo {
        MarketInfo {
            condition_id: "condition".to_string(),
            up_token_id: "up".to_string(),
            down_token_id: "down".to_string(),
            slug: slug.to_string(),
            order_min_size: 5.0,
        }
    }
}

type TradingFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

impl PolymarketTradingClient for AuditOrderClient {
    fn resolve_market<'a>(&'a self, slug: &'a str) -> TradingFuture<'a, MarketInfo> {
        *self.resolution_calls.lock().unwrap() += 1;
        let audit_exists = std::fs::read_to_string(&self.audit_path)
            .map(|content| content.contains("\"status\":\"DECISION\""))
            .unwrap_or(false);
        *self.resolution_saw_audit.lock().unwrap() = audit_exists;
        if !audit_exists {
            return Box::pin(async { Err(anyhow!("audit absent avant resolution")) });
        }
        Box::pin(async move { Ok(Self::market(slug)) })
    }

    fn get_usdc_balance<'a>(&'a self) -> TradingFuture<'a, f64> {
        Box::pin(async { Ok(10.0) })
    }

    fn place_order<'a>(
        &'a self,
        _signal: &'a Signal,
        _market: &'a MarketInfo,
        amount_usdc: f64,
    ) -> TradingFuture<'a, OrderResult> {
        Box::pin(async move {
            Ok(OrderResult {
                order_id: "audit-order".to_string(),
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

    fn warm_sdk_caches<'a>(&'a self, _market: &'a MarketInfo) -> TradingFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

impl PolymarketReadClient for AuditOrderClient {
    fn get_order_status<'a>(&'a self, _order_id: &'a str) -> TradingFuture<'a, String> {
        Box::pin(async { Ok("DRY_RUN".to_string()) })
    }

    fn get_usdc_balance<'a>(&'a self) -> TradingFuture<'a, f64> {
        Box::pin(async { Ok(10.0) })
    }
}

fn complete_snapshot() -> MicrostructureSnapshot {
    let close_time = Utc.with_ymd_and_hms(2026, 7, 17, 0, 0, 0).unwrap();
    let candle = Candle {
        open_time: close_time - Duration::minutes(15),
        close_time,
        open: 100.0,
        high: 101.0,
        low: 99.0,
        close: 100.5,
        volume: 1.0,
        is_closed: true,
    };
    let values: BTreeMap<_, _> = Feature::ALL
        .iter()
        .copied()
        .map(|feature| (feature, 0.0))
        .collect();
    MicrostructureSnapshot::new(candle, values)
}

fn runtime_state(logs_dir: &str) -> (RuntimeState, Arc<AuditOrderClient>) {
    let logger = Arc::new(TradeLogger::new(logs_dir).unwrap());
    let client = Arc::new(AuditOrderClient {
        audit_path: logger.microstructure_audit_path().to_path_buf(),
        resolution_saw_audit: Mutex::new(false),
        resolution_calls: Mutex::new(0),
    });
    let trading_client: Arc<dyn PolymarketTradingClient> = client.clone();
    let tracker_client: Arc<dyn PolymarketReadClient> = client.clone();
    let money = Arc::new(tokio::sync::Mutex::new(MoneyManager::new(
        10.0, 1.0, 0.0, logs_dir,
    )));
    let tracker = Arc::new(PositionTracker::new(
        tracker_client,
        logger.clone(),
        money.clone(),
        logs_dir,
        0.0,
    ));
    (
        RuntimeState {
            trade_logger: logger,
            poly_client: trading_client,
            money_manager: money,
            tracker,
            metrics: Arc::new(RuntimeMetrics::default()),
        },
        client,
    )
}

#[tokio::test]
async fn persists_audit_record_before_market_resolution() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = config(dir.to_str().unwrap());
    let (state, client) = runtime_state(dir.to_str().unwrap());
    let mut strategy = AuditSignalStrategy { summary: None };

    let action = process_microstructure_snapshot(
        &config,
        Duration::minutes(15),
        &mut strategy,
        &state,
        &complete_snapshot(),
    )
    .await;

    assert!(matches!(action, ClosedCandleAction::OrderPlaced { .. }));
    assert!(*client.resolution_saw_audit.lock().unwrap());
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn rejects_a_snapshot_observed_before_close_without_resolving_a_market() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = config(dir.to_str().unwrap());
    let (state, client) = runtime_state(dir.to_str().unwrap());
    let complete = complete_snapshot();
    let snapshot = MicrostructureSnapshot::with_metadata(
        complete.candle().clone(),
        complete.values().clone(),
        complete.candle().close_time - Duration::milliseconds(1),
        complete.feature_source_times().clone(),
    );
    let mut strategy = AuditSignalStrategy { summary: None };

    let action = process_microstructure_snapshot(
        &config,
        Duration::minutes(15),
        &mut strategy,
        &state,
        &snapshot,
    )
    .await;

    assert_eq!(action, ClosedCandleAction::AuditFailed);
    assert_eq!(*client.resolution_calls.lock().unwrap(), 0);
    assert_eq!(state.metrics.snapshot().audit_failed, 1);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn refuses_execution_when_the_audit_journal_becomes_corrupted() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = config(dir.to_str().unwrap());
    let (state, client) = runtime_state(dir.to_str().unwrap());
    std::fs::write(
        state.trade_logger.microstructure_audit_path(),
        "{\"invalid\":true}\n",
    )
    .unwrap();
    let mut strategy = AuditSignalStrategy { summary: None };

    let action = process_microstructure_snapshot(
        &config,
        Duration::minutes(15),
        &mut strategy,
        &state,
        &complete_snapshot(),
    )
    .await;

    assert_eq!(action, ClosedCandleAction::AuditFailed);
    assert_eq!(*client.resolution_calls.lock().unwrap(), 0);
    assert_eq!(state.metrics.snapshot().audit_failed, 1);
    std::fs::remove_dir_all(dir).ok();
}
