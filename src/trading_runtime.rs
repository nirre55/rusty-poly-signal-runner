use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::binance::Candle;
use crate::config::Config;
use crate::logger::{
    log_candle_close, log_order_ack, log_order_sent, log_signal_detected, CandleCloseLog,
    PendingBuyTradeRecord, TradeLogger, TradeRecord,
};
use crate::microstructure::MicrostructureSnapshot;
use crate::microstructure_audit::MicrostructureAuditRecord;
use crate::money::MoneyManager;
use crate::polymarket::{MarketInfo, OrderResult, PolymarketClient};
use crate::runtime_metrics::RuntimeMetrics;
use crate::strategy::{Prediction, Signal, Strategy};
use crate::tracker::{build_signal_key, PositionTracker};
use crate::trade_timing::TradeLatencies;
use crate::trading_filter::{trading_filter_reason, TradingFilterReason};

type PolymarketTradingFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait PolymarketTradingClient: Send + Sync {
    fn resolve_market<'a>(&'a self, slug: &'a str) -> PolymarketTradingFuture<'a, MarketInfo>;
    fn get_usdc_balance<'a>(&'a self) -> PolymarketTradingFuture<'a, f64>;

    fn place_order<'a>(
        &'a self,
        signal: &'a Signal,
        market: &'a MarketInfo,
        amount_usdc: f64,
    ) -> PolymarketTradingFuture<'a, OrderResult>;

    fn warm_sdk_caches<'a>(&'a self, market: &'a MarketInfo) -> PolymarketTradingFuture<'a, ()>;
}

impl PolymarketTradingClient for PolymarketClient {
    fn resolve_market<'a>(&'a self, slug: &'a str) -> PolymarketTradingFuture<'a, MarketInfo> {
        Box::pin(async move { PolymarketClient::resolve_market(self, slug).await })
    }

    fn get_usdc_balance<'a>(&'a self) -> PolymarketTradingFuture<'a, f64> {
        Box::pin(async move { PolymarketClient::get_usdc_balance(self).await })
    }

    fn place_order<'a>(
        &'a self,
        signal: &'a Signal,
        market: &'a MarketInfo,
        amount_usdc: f64,
    ) -> PolymarketTradingFuture<'a, OrderResult> {
        Box::pin(
            async move { PolymarketClient::place_order(self, signal, market, amount_usdc).await },
        )
    }

    fn warm_sdk_caches<'a>(&'a self, market: &'a MarketInfo) -> PolymarketTradingFuture<'a, ()> {
        Box::pin(async move {
            PolymarketClient::warm_sdk_caches(self, market).await;
            Ok(())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedCandleAction {
    NoSignal,
    AuditFailed,
    Filtered,
    DuplicateSignal,
    MarketResolveFailed,
    OrderFailed,
    OrderPlaced {
        trade_id: String,
        signal_key: String,
    },
}

pub struct RuntimeState {
    pub trade_logger: Arc<TradeLogger>,
    pub poly_client: Arc<dyn PolymarketTradingClient>,
    pub money_manager: Arc<tokio::sync::Mutex<MoneyManager>>,
    pub tracker: Arc<PositionTracker>,
    pub metrics: Arc<RuntimeMetrics>,
}

fn finish(state: &RuntimeState, action: ClosedCandleAction) -> ClosedCandleAction {
    state.metrics.record(&action);
    action
}

fn amount_from_balance_pct(balance: f64, pct: f64) -> f64 {
    let amount = (balance * pct / 100.0 * 100.0).floor() / 100.0;
    amount.max(1.0)
}

async fn resolve_trade_amount(config: &Config, state: &RuntimeState) -> f64 {
    if config.trade_amount_pct <= 0.0
        || matches!(config.execution_mode, crate::config::ExecutionMode::DryRun)
    {
        return state.money_manager.lock().await.current_amount();
    }

    match state.poly_client.get_usdc_balance().await {
        Ok(balance) => {
            let base_amount = amount_from_balance_pct(balance, config.trade_amount_pct);
            let mut money = state.money_manager.lock().await;
            money.set_base_amount(base_amount);
            let trade_amount = money.current_amount();
            info!(
                "[MONEY] Balance pre-order fraiche: {:.2}$ | {:.1}% = {:.2}$ | montant courant = {:.2}$",
                balance, config.trade_amount_pct, base_amount, trade_amount
            );
            trade_amount
        }
        Err(e) => {
            let fallback = state.money_manager.lock().await.current_amount();
            warn!(
                "[MONEY] Refresh balance pre-order echoue ({}), fallback montant courant = {:.2}$",
                e, fallback
            );
            fallback
        }
    }
}

pub fn spawn_prefetch_next_market(
    poly_client: &Arc<dyn PolymarketTradingClient>,
    close_time: DateTime<Utc>,
    interval_duration: Duration,
    config: &Config,
) {
    let poly = poly_client.clone();
    let config = config.clone();
    let future_open_ms =
        (close_time + interval_duration + chrono::Duration::milliseconds(1)).timestamp_millis();
    let future_slug = PolymarketClient::build_configured_slug(&config, future_open_ms);
    tokio::spawn(async move {
        if let Ok(market) = poly.resolve_market(&future_slug).await {
            let _ = poly.warm_sdk_caches(&market).await;
        }
    });
}

async fn validate_and_prefetch_next_market(
    state: &RuntimeState,
    config: &Config,
    candle: &Candle,
    interval_duration: Duration,
) {
    state
        .tracker
        .validate_with_closed_candle(candle.close_time, candle.is_green())
        .await;
    spawn_prefetch_next_market(
        &state.poly_client,
        candle.close_time,
        interval_duration,
        config,
    );
}

async fn should_skip_duplicate_signal(
    state: &RuntimeState,
    signal_key: &str,
    candle: &Candle,
) -> bool {
    if state.tracker.is_signal_active(signal_key).await {
        warn!(
            "Signal deja en cours de suivi - ordre ignore | signal_key={}",
            signal_key
        );
        state
            .tracker
            .validate_with_closed_candle(candle.close_time, candle.is_green())
            .await;
        return true;
    }

    match state.trade_logger.has_signal_key(signal_key) {
        Ok(true) => {
            warn!(
                "Signal deja execute precedemment - ordre ignore | signal_key={}",
                signal_key
            );
            state
                .tracker
                .validate_with_closed_candle(candle.close_time, candle.is_green())
                .await;
            true
        }
        Ok(false) => false,
        Err(e) => {
            warn!(
                "Impossible de verifier l'historique des signaux ({}), poursuite prudente",
                e
            );
            false
        }
    }
}

pub async fn process_closed_candle(
    config: &Config,
    interval_duration: Duration,
    strategy: &mut dyn Strategy,
    state: &RuntimeState,
    candle: &Candle,
) -> ClosedCandleAction {
    let signal = strategy.on_closed_candle(candle);
    process_signal_for_candle(config, interval_duration, strategy, state, candle, signal).await
}

/// Process a causally complete multi-source snapshot through the normal
/// Polymarket execution path without feeding it back into on_closed_candle.
pub async fn process_microstructure_snapshot(
    config: &Config,
    interval_duration: Duration,
    strategy: &mut dyn Strategy,
    state: &RuntimeState,
    snapshot: &MicrostructureSnapshot,
) -> ClosedCandleAction {
    let candle = snapshot.candle();
    let signal = strategy.on_microstructure_snapshot(snapshot);
    let next_open_ms = (candle.close_time + chrono::Duration::milliseconds(1)).timestamp_millis();
    let slug = PolymarketClient::build_configured_slug(config, next_open_ms);
    let Some(summary) = strategy.last_microstructure_decision_summary() else {
        error!(
            "[AUDIT] resume microstructure absent; aucun ordre ne sera envoye | strategy={}",
            strategy.name()
        );
        return finish(state, ClosedCandleAction::AuditFailed);
    };
    let mut audit_record = match MicrostructureAuditRecord::decision(
        snapshot,
        strategy.name(),
        &summary,
        slug,
    ) {
        Ok(record) => record,
        Err(error) => {
            error!(
                "[AUDIT] decision non serialisable; aucun ordre ne sera envoye | strategy={} error={}",
                strategy.name(),
                error
            );
            return finish(state, ClosedCandleAction::AuditFailed);
        }
    };
    if let Err(error) = state
        .trade_logger
        .log_microstructure_audit(&mut audit_record)
    {
        error!(
            "[AUDIT] ecriture durable echouee; aucun ordre ne sera envoye | strategy={} error={}",
            strategy.name(),
            error
        );
        return finish(state, ClosedCandleAction::AuditFailed);
    }
    process_signal_for_candle(config, interval_duration, strategy, state, candle, signal).await
}

async fn process_signal_for_candle(
    config: &Config,
    interval_duration: Duration,
    strategy: &mut dyn Strategy,
    state: &RuntimeState,
    candle: &Candle,
    signal: Option<Signal>,
) -> ClosedCandleAction {
    let signal_received_at = Utc::now();

    let color = if candle.is_green() { "VERT" } else { "ROUGE" };
    let candle_log_extras = strategy.candle_log_extras();
    log_candle_close(CandleCloseLog {
        symbol: &config.symbol,
        interval: &config.interval,
        candle_high: candle.high,
        candle_low: candle.low,
        candle_open: candle.open,
        close: candle.close,
        color,
        extras: &candle_log_extras,
        close_time: &candle.close_time,
    });

    let next_open_ms = (candle.close_time + chrono::Duration::milliseconds(1)).timestamp_millis();
    let slug = PolymarketClient::build_configured_slug(config, next_open_ms);

    let Some(signal) = signal else {
        validate_and_prefetch_next_market(state, config, candle, interval_duration).await;
        return finish(state, ClosedCandleAction::NoSignal);
    };

    log_signal_detected(
        &signal.strategy_name,
        &signal.prediction.to_string(),
        signal.rsi,
    );

    if let Some(reason) = trading_filter_reason(
        candle.close_time,
        &config.excluded_days,
        &config.excluded_hours,
    ) {
        match reason {
            TradingFilterReason::ExcludedDay(day) => {
                info!("[FILTRE JOUR] {} - trading desactive ce jour", day);
            }
            TradingFilterReason::ExcludedHour(hour) => {
                info!(
                    "[FILTRE HEURE] {}h UTC - trading desactive sur cette plage horaire",
                    hour
                );
            }
        }
        validate_and_prefetch_next_market(state, config, candle, interval_duration).await;
        return finish(state, ClosedCandleAction::Filtered);
    }

    let target_close_time = candle.close_time + interval_duration;
    let signal_key = build_signal_key(&signal.strategy_name, &slug, &signal.prediction);

    if should_skip_duplicate_signal(state, &signal_key, candle).await {
        return finish(state, ClosedCandleAction::DuplicateSignal);
    }

    let market = match state.poly_client.resolve_market(&slug).await {
        Ok(m) => m,
        Err(e) => {
            error!("Impossible de resoudre le marche Polymarket: {}", e);
            state
                .tracker
                .validate_with_closed_candle(candle.close_time, candle.is_green())
                .await;
            return finish(state, ClosedCandleAction::MarketResolveFailed);
        }
    };

    let trade_amount = resolve_trade_amount(config, state).await;
    let order_submit_started_at = Utc::now();

    let order_result = match state
        .poly_client
        .place_order(&signal, &market, trade_amount)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Erreur lors de l'envoi de l'ordre: {}", e);
            validate_and_prefetch_next_market(state, config, candle, interval_duration).await;
            return finish(state, ClosedCandleAction::OrderFailed);
        }
    };

    let latencies = TradeLatencies::from_times(
        signal_received_at,
        order_submit_started_at,
        order_result.ack_at,
        candle.close_time,
    );

    let token_id = match &signal.prediction {
        Prediction::Up => &market.up_token_id,
        Prediction::Down => &market.down_token_id,
    };

    log_order_sent(&order_result.order_id, token_id, order_result.amount_usdc);
    log_order_ack(
        &order_result.order_id,
        &order_result.status,
        latencies.signal_to_ack_ms,
    );

    let trade_id = Uuid::new_v4().to_string();
    let prediction = signal.prediction.to_string();
    let record = TradeRecord::pending_buy(PendingBuyTradeRecord {
        trade_id: &trade_id,
        signal_key: &signal_key,
        symbol: &config.symbol,
        interval: &config.interval,
        signal_close_time_utc: &signal.signal_candle_close_time,
        target_candle_open_time_utc: &candle.close_time,
        prediction: &prediction,
        entry_order_type: config.execution_mode.as_str(),
        order_status: &order_result.status,
        limit_price: order_result.limit_price,
        execution_price: order_result.execution_price,
        execution_price_source: order_result.execution_price_source.as_deref(),
        size_matched: order_result.size_matched,
        latencies,
    });

    if let Err(e) = state.trade_logger.log_trade(&record) {
        error!("Erreur lors de l'enregistrement du trade: {}", e);
    }

    state
        .tracker
        .track(
            trade_id.clone(),
            order_result.order_id,
            signal_key.clone(),
            signal.prediction.clone(),
            target_close_time,
            order_result.status.clone(),
        )
        .await;

    validate_and_prefetch_next_market(state, config, candle, interval_duration).await;

    finish(
        state,
        ClosedCandleAction::OrderPlaced {
            trade_id,
            signal_key,
        },
    )
}
