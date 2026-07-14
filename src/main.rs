use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use rusty_poly_signal_runner::binance::{self, Candle};
use rusty_poly_signal_runner::config::{Config, ExecutionMode};
use rusty_poly_signal_runner::interval::parse_interval_duration;
use rusty_poly_signal_runner::logger::TradeLogger;
use rusty_poly_signal_runner::microstructure::EthUsdPerpMicrostructureCollector;
use rusty_poly_signal_runner::money::MoneyManager;
use rusty_poly_signal_runner::polymarket::PolymarketClient;
use rusty_poly_signal_runner::runtime_metrics::RuntimeMetrics;
use rusty_poly_signal_runner::strategy::Strategy;
use rusty_poly_signal_runner::strategy_factory::create_strategy;
use rusty_poly_signal_runner::tracker::{PolymarketReadClient, PositionTracker};
use rusty_poly_signal_runner::trading_runtime::{
    process_closed_candle, process_microstructure_snapshot, PolymarketTradingClient, RuntimeState,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let interval_duration = parse_interval_duration(&config.interval)?;
    info!(
        "Demarrage rusty-poly-signal-runner | mode={:?} symbol={} interval={} strategy={} rsi=[{},{}]",
        config.execution_mode,
        config.symbol,
        config.interval,
        config.strategy,
        config.rsi_oversold,
        config.rsi_overbought
    );

    let trade_logger = Arc::new(TradeLogger::new(&config.logs_dir)?);
    let poly_client = Arc::new(PolymarketClient::new(config.clone()));
    let metrics = Arc::new(RuntimeMetrics::default());
    let trading_client: Arc<dyn PolymarketTradingClient> = poly_client.clone();
    let tracker_client: Arc<dyn PolymarketReadClient> = poly_client.clone();

    poly_client.warm_up().await;
    tokio::spawn({
        let poly = poly_client.clone();
        async move { poly.run_keep_alive_loop().await }
    });

    let mut active_strategy = create_strategy(&config)?;

    let initial_base_amount = if config.trade_amount_pct > 0.0
        && !matches!(config.execution_mode, ExecutionMode::DryRun)
    {
        match poly_client.get_usdc_balance().await {
            Ok(balance) => {
                let amount = (balance * config.trade_amount_pct / 100.0 * 100.0).floor() / 100.0;
                let amount = amount.max(1.0);
                info!(
                    "[MONEY] Solde USDC = {:.2} | {:.1}% = {:.2} USDC (min 1$)",
                    balance, config.trade_amount_pct, amount
                );
                amount
            }
            Err(e) => {
                warn!(
                        "[MONEY] Impossible de recuperer le solde USDC pour TRADE_AMOUNT_PCT ({}), fallback {:.2} USDC",
                        e, config.trade_amount_usdc
                    );
                config.trade_amount_usdc
            }
        }
    } else {
        config.trade_amount_usdc
    };

    let money_manager = Arc::new(tokio::sync::Mutex::new(MoneyManager::new(
        initial_base_amount,
        config.martingale_multiplier,
        config.martingale_max_amount,
        &config.logs_dir,
    )));
    if config.martingale_multiplier > 1.0 {
        let mm = money_manager.lock().await;
        info!(
            "Martingale activee | base={:.2} USDC multiplier={:.2} montant_courant={:.2} USDC (losses={})",
            initial_base_amount,
            config.martingale_multiplier,
            mm.current_amount(),
            mm.consecutive_losses()
        );
    }

    let tracker_pct = if matches!(config.execution_mode, ExecutionMode::DryRun) {
        0.0
    } else {
        config.trade_amount_pct
    };
    let tracker = Arc::new(PositionTracker::new(
        tracker_client,
        trade_logger.clone(),
        money_manager.clone(),
        &config.logs_dir,
        tracker_pct,
    ));
    tokio::spawn({
        let tracker = tracker.clone();
        async move { tracker.run_poll_loop().await }
    });

    let runtime_state = RuntimeState {
        trade_logger: trade_logger.clone(),
        poly_client: trading_client,
        money_manager: money_manager.clone(),
        tracker: tracker.clone(),
        metrics: metrics.clone(),
    };

    if active_strategy.requires_microstructure() {
        return run_microstructure_strategy(
            &config,
            interval_duration,
            active_strategy.as_mut(),
            &runtime_state,
        )
        .await;
    }

    match binance::fetch_historical_candles(&config.symbol, &config.interval, 120).await {
        Ok(candles) => {
            let now_ms = Utc::now().timestamp_millis();
            let closed: Vec<_> = candles
                .into_iter()
                .filter(|c| c.close_time.timestamp_millis() < now_ms)
                .collect();
            info!(
                "Prechargement : {} bougies fermees utilisees pour le warmup RSI",
                closed.len()
            );
            for candle in closed {
                active_strategy.warmup(&candle);
            }
        }
        Err(e) => {
            error!("Impossible de precharger l'historique Binance: {}", e);
        }
    }

    loop {
        let (tx, mut rx) = mpsc::channel::<Candle>(64);

        let ws_url = config.binance_ws_url.clone();
        let symbol = config.symbol.clone();
        let interval = config.interval.clone();

        tokio::spawn(async move {
            if let Err(e) = binance::stream_candles(&ws_url, &symbol, &interval, tx).await {
                error!("Erreur stream Binance: {}", e);
            }
        });

        while let Some(candle) = rx.recv().await {
            process_closed_candle(
                &config,
                interval_duration,
                active_strategy.as_mut(),
                &runtime_state,
                &candle,
            )
            .await;
            let snapshot = metrics.snapshot();
            let total = snapshot.no_signal
                + snapshot.filtered
                + snapshot.duplicate_signal
                + snapshot.market_resolve_failed
                + snapshot.order_failed
                + snapshot.order_placed;
            if total > 0 && total % 50 == 0 {
                info!(
                    "[METRICS] no_signal={} filtered={} duplicate={} market_errors={} order_errors={} orders={}",
                    snapshot.no_signal,
                    snapshot.filtered,
                    snapshot.duplicate_signal,
                    snapshot.market_resolve_failed,
                    snapshot.order_failed,
                    snapshot.order_placed
                );
            }
        }

        warn!("[RECONNECT] Channel Binance ferme - relance du stream dans 5s...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        poly_client.warm_up().await;
    }
}

async fn run_microstructure_strategy(
    config: &Config,
    interval_duration: ChronoDuration,
    strategy: &mut dyn Strategy,
    runtime_state: &RuntimeState,
) -> Result<()> {
    const PRECOMMIT_WINDOW: ChronoDuration = ChronoDuration::seconds(30);
    let collector = EthUsdPerpMicrostructureCollector::new()?;
    let mut last_seen_close: Option<DateTime<Utc>> = None;

    info!(
        "Demarrage collecteur microstructure ETHUSD_PERP 15m | strategie={} | precommit_max=30s",
        strategy.name()
    );

    loop {
        match collector.fetch_snapshot().await {
            Ok(snapshot) => {
                let close_time = snapshot.candle().close_time;
                if last_seen_close == Some(close_time) {
                    tokio::time::sleep(until_next_microstructure_refresh()).await;
                    continue;
                }

                last_seen_close = Some(close_time);
                let age = Utc::now().signed_duration_since(close_time);
                if age > PRECOMMIT_WINDOW {
                    warn!(
                        "[MICROSTRUCTURE] snapshot trop ancien, signal ignore | close={} age_ms={}",
                        close_time,
                        age.num_milliseconds()
                    );
                    tokio::time::sleep(until_next_microstructure_refresh()).await;
                    continue;
                }
                if age < ChronoDuration::zero() {
                    warn!(
                        "[MICROSTRUCTURE] horloge locale avant le snapshot, nouvelle tentative | close={}",
                        close_time
                    );
                    last_seen_close = None;
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }

                process_microstructure_snapshot(
                    config,
                    interval_duration,
                    strategy,
                    runtime_state,
                    &snapshot,
                )
                .await;
                tokio::time::sleep(until_next_microstructure_refresh()).await;
            }
            Err(error) => {
                warn!(
                    "[MICROSTRUCTURE] snapshot indisponible, aucun signal emis: {}",
                    error
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

fn until_next_microstructure_refresh() -> std::time::Duration {
    const PERIOD_MS: i64 = 15 * 60 * 1000;
    const GRACE_MS: i64 = 2_000;
    let now_ms = Utc::now().timestamp_millis();
    let next_boundary_ms = (now_ms.div_euclid(PERIOD_MS) + 1) * PERIOD_MS + GRACE_MS;
    let wait_ms = (next_boundary_ms - now_ms).max(1) as u64;
    std::time::Duration::from_millis(wait_ms)
}
