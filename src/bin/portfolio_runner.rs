use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use rusty_poly_signal_runner::binance::{self, Candle};
use rusty_poly_signal_runner::config::{Config, ExecutionMode, PolymarketSlugFormat};
use rusty_poly_signal_runner::market_recorder::{RecorderSettings, SignalMarketRecorder};
use rusty_poly_signal_runner::polymarket::{MarketInfo, PolymarketClient};
use rusty_poly_signal_runner::portfolio::{
    group_signals, size_window, EnabledStrategies, MarketSlot, PortfolioSettings, PortfolioSignal,
    PortfolioStrategy, SizingDecision,
};
use rusty_poly_signal_runner::portfolio_runtime::{
    append_event, CollectResult, FeedEvent, OrderAcknowledgement, PortfolioBook, PortfolioOrder,
    PortfolioOrderPhase, WindowBatch, WindowCollector,
};
use rusty_poly_signal_runner::strategies::meche::{BollFade, ReversalPro, StreakRsi, TrioVote2};
use rusty_poly_signal_runner::strategy::{Prediction, Signal, Strategy};

const FIXED_LIMIT_PRICE: f64 = 0.50;
const MINIMUM_SHARES: f64 = 5.0;

struct FeedCandleEvent {
    feed: FeedEvent,
    candle: Candle,
}

#[derive(Default)]
struct FeedStrategies {
    boll_fade: BollFade,
    streak_rsi: StreakRsi,
    trio_vote2: TrioVote2,
    reversal_pro: ReversalPro,
}

impl FeedStrategies {
    fn warmup(&mut self, candle: &Candle) {
        self.boll_fade.warmup(candle);
        self.streak_rsi.warmup(candle);
        self.trio_vote2.warmup(candle);
        self.reversal_pro.warmup(candle);
    }

    fn evaluate(
        &mut self,
        candle: &Candle,
        market: MarketSlot,
        enabled: &EnabledStrategies,
    ) -> Vec<PortfolioSignal> {
        [
            (
                PortfolioStrategy::BollFade,
                self.boll_fade.on_closed_candle(candle),
            ),
            (
                PortfolioStrategy::StreakRsi,
                self.streak_rsi.on_closed_candle(candle),
            ),
            (
                PortfolioStrategy::TrioVote2,
                self.trio_vote2.on_closed_candle(candle),
            ),
            (
                PortfolioStrategy::ReversalPro,
                self.reversal_pro.on_closed_candle(candle),
            ),
        ]
        .into_iter()
        .filter_map(|(strategy, signal)| {
            enabled
                .is_enabled(strategy, market)
                .then_some(signal)
                .flatten()
                .map(|signal| PortfolioSignal {
                    strategy,
                    market,
                    prediction: signal.prediction,
                    signal_close_time: signal.signal_candle_close_time,
                })
        })
        .collect()
    }
}

struct ResolvedGroup {
    market: MarketSlot,
    prediction: Prediction,
    slug: String,
    info: MarketInfo,
    minimum_usdc: f64,
}

struct WindowContext<'a> {
    client: &'a PolymarketClient,
    config: &'a Config,
    settings: &'a PortfolioSettings,
    feed_configs: &'a BTreeMap<MarketSlot, Config>,
    book: &'a mut PortfolioBook,
    state_path: PathBuf,
    event_path: PathBuf,
    recorder: Option<&'a SignalMarketRecorder>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    validate_portfolio_config(&config)?;
    let settings = PortfolioSettings::from_env()?;
    let enabled = EnabledStrategies::load(&settings.enabled_path)?;
    let state_path = Path::new(&config.logs_dir).join("portfolio_state.json");
    let event_path = Path::new(&config.logs_dir).join("portfolio_events.jsonl");
    let mut book = PortfolioBook::load(&state_path)?;
    let feed_configs = build_feed_configs(&config);

    info!(
        "Mèche 0,50 | mode={:?} | W={:.2}% | f={:.2}% | min_override={} | sync={}ms | active_orders={}",
        config.execution_mode,
        settings.sizing.window_budget_pct,
        settings.sizing.signal_cap_pct,
        settings.sizing.allow_minimum_above_window,
        settings.sync_grace.as_millis(),
        book.orders()
            .iter()
            .filter(|order| order.is_pending())
            .count(),
    );

    let recorder_settings = RecorderSettings::from_env(&config.logs_dir)?;
    if recorder_settings.enabled && !matches!(config.execution_mode, ExecutionMode::DryRun) {
        return Err(anyhow!(
            "PORTFOLIO_RECORDER_ENABLED=true requiert EXECUTION_MODE=dry-run"
        ));
    }
    let client = Arc::new(PolymarketClient::new(config.clone()));
    if should_submit_real_order(&config.execution_mode) {
        client.warm_up().await;
        tokio::spawn({
            let client = client.clone();
            async move { client.run_keep_alive_loop().await }
        });
    } else {
        info!("Dry-run: pré-authentification SDK et boucle CLOB désactivées");
    }

    let recorder =
        SignalMarketRecorder::start(recorder_settings, client.clone(), feed_configs.clone())
            .await?;

    let (tx, mut rx) = mpsc::channel::<FeedCandleEvent>(256);
    for market in MarketSlot::ALL {
        tokio::spawn(run_feed(
            config.clone(),
            market,
            enabled.clone(),
            recorder.clone(),
            tx.clone(),
        ));
    }
    drop(tx);

    let mut collector = WindowCollector::default();
    let mut expiry = tokio::time::interval(Duration::from_millis(100));
    let mut window_context = WindowContext {
        client: &client,
        config: &config,
        settings: &settings,
        feed_configs: &feed_configs,
        book: &mut book,
        state_path,
        event_path,
        recorder: recorder.as_ref(),
    };

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                if let Some(recorder) = recorder.as_ref() {
                    if let Err(error) = recorder
                        .record_signals(
                            event.feed.entry_time_ms,
                            &event.feed.signals,
                            &event.candle,
                        )
                        .await
                    {
                        error!("Enregistrement signal Polymarket: {error:#}");
                    }
                }
                settle_orders(
                    window_context.client,
                    window_context.book,
                    &window_context.state_path,
                    &window_context.event_path,
                    &event,
                ).await;
                match collector.insert(event.feed, settings.sync_grace, Instant::now()) {
                    Ok(CollectResult::Waiting) => {}
                    Ok(CollectResult::Duplicate) => {
                        warn!("Fenêtre dupliquée ignorée");
                    }
                    Ok(CollectResult::Ready(batch)) => {
                        if let Err(error) = finalize_window(&mut window_context, batch).await {
                            error!("Finalisation fenêtre portefeuille: {error:#}");
                        }
                    }
                    Err(error) => error!("Collecteur portefeuille: {error:#}"),
                }
            }
            _ = expiry.tick() => {
                for entry_time_ms in collector.expire(Instant::now()) {
                    warn!("Fenêtre incomplète ignorée | entry_time_ms={entry_time_ms}");
                    log_event(
                        &window_context.event_path,
                        "WINDOW_INCOMPLETE",
                        json!({"entry_time_ms": entry_time_ms}),
                    );
                }
            }
            else => return Ok(()),
        }
    }
}

fn validate_portfolio_config(config: &Config) -> Result<()> {
    if !matches!(
        config.execution_mode,
        ExecutionMode::Limit | ExecutionMode::DryRun
    ) {
        return Err(anyhow!(
            "portfolio_runner requiert EXECUTION_MODE=limit ou dry-run"
        ));
    }
    if config.limit_price_fixed != Some(FIXED_LIMIT_PRICE) {
        return Err(anyhow!(
            "portfolio_runner requiert LIMIT_PRICE_FIXED={FIXED_LIMIT_PRICE:.2}"
        ));
    }
    Ok(())
}

fn build_feed_configs(base: &Config) -> BTreeMap<MarketSlot, Config> {
    MarketSlot::ALL
        .into_iter()
        .map(|market| {
            let mut config = base.clone();
            config.symbol = market.symbol().to_string();
            config.interval = market.interval().to_string();
            config.polymarket_slug_prefix = market.slug_prefix().to_string();
            config.polymarket_slug_format = PolymarketSlugFormat::Timestamp;
            config.polymarket_slug_asset = if market.symbol().starts_with("btc") {
                "bitcoin".to_string()
            } else {
                "ethereum".to_string()
            };
            (market, config)
        })
        .collect()
}

async fn run_feed(
    base_config: Config,
    market: MarketSlot,
    enabled: EnabledStrategies,
    recorder: Option<SignalMarketRecorder>,
    tx: mpsc::Sender<FeedCandleEvent>,
) {
    let mut strategies = FeedStrategies::default();
    match binance::fetch_historical_candles(market.symbol(), market.interval(), 120).await {
        Ok(candles) => {
            let now_ms = Utc::now().timestamp_millis();
            let mut warmed = 0_usize;
            for candle in candles
                .into_iter()
                .filter(|candle| candle.close_time.timestamp_millis() < now_ms)
            {
                strategies.warmup(&candle);
                warmed += 1;
            }
            info!("Warmup {} | {} bougies", market.key(), warmed);
        }
        Err(error) => warn!("Warmup {} échoué: {error:#}", market.key()),
    }

    loop {
        let (candle_tx, mut candle_rx) = mpsc::channel::<Candle>(64);
        let url = base_config.binance_ws_url.clone();
        let symbol = market.symbol().to_string();
        let interval = market.interval().to_string();
        tokio::spawn(async move {
            if let Err(error) =
                binance::stream_candle_updates(&url, &symbol, &interval, candle_tx).await
            {
                warn!("Flux Binance {} interrompu: {error:#}", market.key());
            }
        });

        while let Some(candle) = candle_rx.recv().await {
            if let Some(recorder) = recorder.as_ref() {
                recorder.record_binance_candle(market, &candle).await;
            }
            if !candle.is_closed {
                continue;
            }
            let signals = strategies.evaluate(&candle, market, &enabled);
            if !signals.is_empty() {
                info!(
                    "Signaux {} | close={} | stratégies={}",
                    market.key(),
                    candle.close_time,
                    signals
                        .iter()
                        .map(|signal| signal.strategy.key())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
            let event = FeedCandleEvent {
                feed: FeedEvent {
                    market,
                    entry_time_ms: candle.close_time.timestamp_millis() + 1,
                    signals,
                },
                candle,
            };
            if tx.send(event).await.is_err() {
                return;
            }
        }

        warn!("Reconnect Binance {} dans 5 secondes", market.key());
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn finalize_window(context: &mut WindowContext<'_>, batch: WindowBatch) -> Result<()> {
    if batch.signals.is_empty() {
        log_event(
            &context.event_path,
            "WINDOW_EMPTY",
            json!({"entry_time_ms": batch.entry_time_ms}),
        );
        return Ok(());
    }

    let capital_usdc = if matches!(context.config.execution_mode, ExecutionMode::DryRun) {
        context.settings.dry_run_capital_usdc
    } else {
        match context.client.get_usdc_balance().await {
            Ok(balance) if balance > 0.0 => balance,
            Ok(_) => {
                warn!("Fenêtre ignorée: solde USDC nul");
                record_all_sizing_updates(
                    context,
                    &batch,
                    "SKIPPED_NO_BALANCE",
                    json!({"capital_usdc": 0.0}),
                )
                .await;
                log_event(
                    &context.event_path,
                    "WINDOW_SKIPPED_NO_BALANCE",
                    json!({"entry_time_ms": batch.entry_time_ms}),
                );
                return Ok(());
            }
            Err(error) => {
                warn!("Fenêtre ignorée: lecture solde USDC échouée: {error:#}");
                record_all_sizing_updates(
                    context,
                    &batch,
                    "SKIPPED_BALANCE_ERROR",
                    json!({"error": error.to_string()}),
                )
                .await;
                log_event(
                    &context.event_path,
                    "WINDOW_SKIPPED_BALANCE_ERROR",
                    json!({"entry_time_ms": batch.entry_time_ms, "error": error.to_string()}),
                );
                return Ok(());
            }
        }
    };

    let groups = group_signals(&batch.signals);
    let mut resolved_groups = Vec::with_capacity(groups.len());
    for group in &groups {
        let feed_config = context
            .feed_configs
            .get(&group.market)
            .ok_or_else(|| anyhow!("configuration feed absente: {}", group.market.key()))?;
        let slug = PolymarketClient::build_configured_slug(feed_config, batch.entry_time_ms);
        let key = order_key(&slug, &group.prediction);
        if context.book.has_seen(&key) {
            warn!("Fenêtre dupliquée déjà persistée: {key}");
            record_all_sizing_updates(context, &batch, "SKIPPED_DUPLICATE", json!({"key": key}))
                .await;
            log_event(
                &context.event_path,
                "WINDOW_SKIPPED_DUPLICATE",
                json!({"entry_time_ms": batch.entry_time_ms, "key": key}),
            );
            return Ok(());
        }

        let info = match context.client.resolve_market(&slug).await {
            Ok(info) => info,
            Err(error) => {
                warn!("Fenêtre ignorée: marché {slug} introuvable: {error:#}");
                record_all_sizing_updates(
                    context,
                    &batch,
                    "SKIPPED_MARKET_ERROR",
                    json!({"slug": slug, "error": error.to_string()}),
                )
                .await;
                log_event(
                    &context.event_path,
                    "WINDOW_SKIPPED_MARKET_ERROR",
                    json!({"entry_time_ms": batch.entry_time_ms, "slug": slug, "error": error.to_string()}),
                );
                return Ok(());
            }
        };
        resolved_groups.push(ResolvedGroup {
            market: group.market,
            prediction: group.prediction.clone(),
            slug,
            minimum_usdc: info.order_min_size.max(MINIMUM_SHARES) * FIXED_LIMIT_PRICE,
            info,
        });
    }

    let minimums: Vec<f64> = resolved_groups
        .iter()
        .map(|group| group.minimum_usdc)
        .collect();
    match size_window(context.settings.sizing, capital_usdc, groups, &minimums)? {
        SizingDecision::Empty => Ok(()),
        SizingDecision::SkipMinimumsExceedBudget {
            window_budget_usdc,
            total_usdc,
        } => {
            record_all_sizing_updates(
                context,
                &batch,
                "SKIPPED_MINIMUMS_EXCEED_WINDOW",
                json!({
                    "capital_usdc": capital_usdc,
                    "window_budget_usdc": window_budget_usdc,
                    "minimums_total_usdc": total_usdc,
                }),
            )
            .await;
            warn!(
                "Fenêtre ignorée: minima {:.2}$ > budget W {:.2}$",
                total_usdc, window_budget_usdc
            );
            log_event(
                &context.event_path,
                "WINDOW_SKIPPED_MINIMUMS",
                json!({
                    "entry_time_ms": batch.entry_time_ms,
                    "window_budget_usdc": window_budget_usdc,
                    "total_usdc": total_usdc,
                }),
            );
            Ok(())
        }
        SizingDecision::SkipInsufficientCapital {
            capital_usdc,
            minimum_usdc,
        } => {
            record_all_sizing_updates(
                context,
                &batch,
                "SKIPPED_INSUFFICIENT_CAPITAL",
                json!({
                    "capital_usdc": capital_usdc,
                    "minimum_usdc": minimum_usdc,
                }),
            )
            .await;
            warn!(
                "Fenetre ignoree: capital disponible {:.2}$ < minimum {:.2}$",
                capital_usdc, minimum_usdc
            );
            log_event(
                &context.event_path,
                "WINDOW_SKIPPED_INSUFFICIENT_CAPITAL",
                json!({
                    "entry_time_ms": batch.entry_time_ms,
                    "capital_usdc": capital_usdc,
                    "minimum_usdc": minimum_usdc,
                }),
            );
            Ok(())
        }
        SizingDecision::Submit {
            window_budget_usdc,
            orders,
            total_usdc,
            minimum_overrides_window,
            skipped_insufficient_capital_orders,
            ..
        } => {
            for signal in &batch.signals {
                let sized = orders.iter().find(|order| {
                    order.group.market == signal.market
                        && order.group.prediction == signal.prediction
                        && order
                            .group
                            .contributors
                            .iter()
                            .any(|contributor| contributor.strategy == signal.strategy)
                });
                if let Some(sized) = sized {
                    record_sizing_update(
                        context,
                        batch.entry_time_ms,
                        signal,
                        "DRY_RUN_ORDER_CANDIDATE",
                        json!({
                            "capital_usdc": capital_usdc,
                            "window_budget_usdc": window_budget_usdc,
                            "group_allocation_usdc": sized.allocation_usdc,
                            "per_signal_allocation_usdc": sized.allocation_usdc
                                / sized.group.contributors.len() as f64,
                            "combined_amount_usdc": sized.amount_usdc,
                            "minimum_usdc": sized.minimum_usdc,
                            "contributor_count": sized.group.contributors.len(),
                            "minimum_overrides_window": minimum_overrides_window,
                            "fixed_limit_price": FIXED_LIMIT_PRICE,
                            "minimum_shares": MINIMUM_SHARES,
                        }),
                    )
                    .await;
                } else {
                    record_sizing_update(
                        context,
                        batch.entry_time_ms,
                        signal,
                        "SKIPPED_AFTER_MINIMUM_CAPITAL_PRIORITY",
                        json!({
                            "capital_usdc": capital_usdc,
                            "window_budget_usdc": window_budget_usdc,
                            "skipped_orders": skipped_insufficient_capital_orders,
                        }),
                    )
                    .await;
                }
            }
            if minimum_overrides_window {
                warn!(
                    "Minimum 5 shares prioritaire: total {:.2}$ > budget W {:.2}$",
                    total_usdc, window_budget_usdc
                );
                log_event(
                    &context.event_path,
                    "WINDOW_MINIMUM_OVERRIDE",
                    json!({
                        "entry_time_ms": batch.entry_time_ms,
                        "window_budget_usdc": window_budget_usdc,
                        "total_usdc": total_usdc,
                    }),
                );
            }
            if skipped_insufficient_capital_orders > 0 {
                warn!(
                    "{} order(s) skipped: insufficient CLOB capital after minimums",
                    skipped_insufficient_capital_orders
                );
                log_event(
                    &context.event_path,
                    "WINDOW_PARTIAL_INSUFFICIENT_CAPITAL",
                    json!({
                        "entry_time_ms": batch.entry_time_ms,
                        "skipped_orders": skipped_insufficient_capital_orders,
                    }),
                );
            }
            info!(
                "Fenêtre prête | entry={} | signaux={} | ordres={} | total={:.2}$ / W={:.2}$",
                batch.entry_time_ms,
                batch.signals.len(),
                orders.len(),
                total_usdc,
                window_budget_usdc,
            );
            for sized in orders {
                let resolved = resolved_groups
                    .iter()
                    .find(|candidate| {
                        candidate.market == sized.group.market
                            && candidate.prediction == sized.group.prediction
                    })
                    .ok_or_else(|| anyhow!("groupe de marché résolu introuvable"))?;
                submit_combined_order(
                    context,
                    batch.entry_time_ms,
                    capital_usdc,
                    window_budget_usdc,
                    minimum_overrides_window,
                    &sized,
                    resolved,
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn submit_combined_order(
    context: &mut WindowContext<'_>,
    entry_time_ms: i64,
    capital_usdc: f64,
    window_budget_usdc: f64,
    minimum_overrides_window: bool,
    sized: &rusty_poly_signal_runner::portfolio::SizedOrder,
    resolved: &ResolvedGroup,
) -> Result<()> {
    let key = order_key(&resolved.slug, &sized.group.prediction);
    let contributor_strategies = sized
        .group
        .contributors
        .iter()
        .map(|signal| signal.strategy)
        .collect::<Vec<_>>();
    let signal_close_time = sized
        .group
        .contributors
        .first()
        .map(|signal| signal.signal_close_time)
        .ok_or_else(|| anyhow!("ordre combiné sans contributeur"))?;
    let order = PortfolioOrder {
        key: key.clone(),
        market: sized.group.market,
        slug: resolved.slug.clone(),
        prediction: sized.group.prediction.clone(),
        contributor_strategies,
        contributor_count: sized.group.contributors.len(),
        capital_usdc,
        window_budget_usdc,
        allocation_usdc: sized.allocation_usdc,
        amount_usdc: sized.amount_usdc,
        minimum_usdc: sized.minimum_usdc,
        minimum_overrides_window,
        target_close_time_ms: entry_time_ms + sized.group.market.interval_millis() - 1,
        created_at: Utc::now(),
        order_id: None,
        order_status: "SUBMITTING".to_string(),
        acknowledged_amount_usdc: None,
        limit_price: None,
        execution_price: None,
        size_matched: None,
        phase: PortfolioOrderPhase::Submitting,
        outcome: None,
    };
    context.book.begin_submission(order)?;
    context.book.save(&context.state_path)?;

    if !should_submit_real_order(&context.config.execution_mode) {
        context.book.mark_settlement(
            &key,
            "DRY_RUN".to_string(),
            PortfolioOrderPhase::NoEntry,
            Some("DRY_RUN".to_string()),
        )?;
        context.book.save(&context.state_path)?;
        log_event(
            &context.event_path,
            "ORDER_DRY_RUN",
            json!({
                "key": key,
                "amount_usdc": sized.amount_usdc,
                "limit_price": FIXED_LIMIT_PRICE,
                "order_submission_called": false,
            }),
        );
        return Ok(());
    }

    let signal = Signal {
        prediction: sized.group.prediction.clone(),
        signal_candle_close_time: signal_close_time,
        rsi: sized.group.contributors.len() as f64,
        strategy_name: "meche050_portfolio".to_string(),
    };
    match context
        .client
        .place_order(&signal, &resolved.info, sized.amount_usdc)
        .await
    {
        Ok(result) => {
            context.book.mark_submitted(
                &key,
                OrderAcknowledgement {
                    order_id: result.order_id,
                    order_status: result.status,
                    acknowledged_amount_usdc: result.amount_usdc,
                    limit_price: result.limit_price,
                    execution_price: result.execution_price,
                    size_matched: result.size_matched,
                },
            )?;
            context.book.save(&context.state_path)?;
            log_event(
                &context.event_path,
                "ORDER_SUBMITTED",
                json!({"key": key, "amount_usdc": sized.amount_usdc}),
            );
        }
        Err(error) => {
            let status = format!("ORDER_FAILED: {error}");
            context.book.mark_failed(&key, &status)?;
            context.book.save(&context.state_path)?;
            warn!("Ordre combiné échoué {key}: {error:#}");
            log_event(
                &context.event_path,
                "ORDER_FAILED",
                json!({"key": key, "error": error.to_string()}),
            );
        }
    }
    Ok(())
}

async fn settle_orders(
    client: &PolymarketClient,
    book: &mut PortfolioBook,
    state_path: &Path,
    event_path: &Path,
    event: &FeedCandleEvent,
) {
    let close_time_ms = event.candle.close_time.timestamp_millis();
    for order in book.pending_for_market(event.feed.market, close_time_ms) {
        let Some(order_id) = order.order_id.as_deref() else {
            warn!(
                "Ordre {} reste SUBMITTING après restart; aucune reprise automatique",
                order.key
            );
            continue;
        };
        let status = match client.get_order_status(order_id).await {
            Ok(status) => status,
            Err(error) => {
                warn!("Statut ordre {} indisponible: {error:#}", order.key);
                continue;
            }
        };

        let settlement = if is_non_fill_terminal(&status) {
            Some((PortfolioOrderPhase::NoEntry, Some("NO_ENTRY".to_string())))
        } else if is_filled(&status) && order.target_close_time_ms == close_time_ms {
            Some(outcome_for_candle(&order.prediction, &event.candle))
        } else if is_filled(&status) && order.target_close_time_ms < close_time_ms {
            Some((
                PortfolioOrderPhase::MissedValidation,
                Some("MISSED_VALIDATION".to_string()),
            ))
        } else {
            None
        };

        let Some((phase, outcome)) = settlement else {
            continue;
        };
        if let Err(error) = book.mark_settlement(&order.key, status.clone(), phase, outcome.clone())
        {
            error!("Mise à jour ordre {}: {error:#}", order.key);
            continue;
        }
        if let Err(error) = book.save(state_path) {
            error!("Sauvegarde règlement {}: {error:#}", order.key);
            continue;
        }
        log_event(
            event_path,
            "ORDER_SETTLED",
            json!({"key": order.key, "status": status, "outcome": outcome}),
        );
    }
}

fn order_key(slug: &str, prediction: &Prediction) -> String {
    format!("meche050:{}:{}", slug.to_ascii_lowercase(), prediction)
}

fn should_submit_real_order(mode: &ExecutionMode) -> bool {
    !matches!(mode, ExecutionMode::DryRun)
}

fn is_filled(status: &str) -> bool {
    matches!(status.to_ascii_uppercase().as_str(), "MATCHED" | "FILLED")
}

fn is_non_fill_terminal(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "CANCELLED" | "EXPIRED" | "UNMATCHED"
    )
}

fn outcome_for_candle(
    prediction: &Prediction,
    candle: &Candle,
) -> (PortfolioOrderPhase, Option<String>) {
    if candle.close == candle.open {
        return (
            PortfolioOrderPhase::MissedValidation,
            Some("DOJI".to_string()),
        );
    }
    let candle_is_up = candle.close > candle.open;
    let won = matches!(prediction, Prediction::Up) == candle_is_up;
    (
        PortfolioOrderPhase::Filled,
        Some(if won { "WIN" } else { "LOSS" }.to_string()),
    )
}

fn log_event(path: &Path, event_type: &str, details: serde_json::Value) {
    let event = json!({
        "at": Utc::now().to_rfc3339(),
        "event": event_type,
        "details": details,
    });
    if let Err(error) = append_event(path, &event) {
        warn!("Journal portefeuille indisponible: {error:#}");
    }
}

async fn record_all_sizing_updates(
    context: &WindowContext<'_>,
    batch: &WindowBatch,
    disposition: &str,
    details: serde_json::Value,
) {
    for signal in &batch.signals {
        record_sizing_update(
            context,
            batch.entry_time_ms,
            signal,
            disposition,
            details.clone(),
        )
        .await;
    }
}

async fn record_sizing_update(
    context: &WindowContext<'_>,
    entry_time_ms: i64,
    signal: &PortfolioSignal,
    disposition: &str,
    details: serde_json::Value,
) {
    let Some(recorder) = context.recorder else {
        return;
    };
    if let Err(error) = recorder
        .record_sizing_update(entry_time_ms, signal, disposition, details)
        .await
    {
        warn!("Journal sizing signal indisponible: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::should_submit_real_order;
    use rusty_poly_signal_runner::config::ExecutionMode;

    #[test]
    fn recorder_dry_run_never_uses_the_real_order_path() {
        assert!(!should_submit_real_order(&ExecutionMode::DryRun));
        assert!(should_submit_real_order(&ExecutionMode::Limit));
        assert!(should_submit_real_order(&ExecutionMode::Market));
    }
}
