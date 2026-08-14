//! Restart-safe Polymarket market-channel recorder for Mèche 0.50 forward tests.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::binance::Candle;
use crate::config::Config;
use crate::polymarket::{MarketInfo, PolymarketClient};
use crate::portfolio::{MarketSlot, PortfolioSignal};
use crate::recorder_metrics::{SessionAnalyzer, SessionMetricContext};
use crate::strategy::Prediction;

const MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct RecorderSettings {
    pub enabled: bool,
    pub root: PathBuf,
    pub pre_signal: Duration,
    pub setup_lead: Duration,
    pub activation_grace: Duration,
    pub resolution_timeout: Duration,
    pub reconnect_delay: Duration,
    pub delete_stream_after_summary: bool,
    pub limit_price: f64,
}

impl RecorderSettings {
    pub fn from_env(logs_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            enabled: parse_bool_env("PORTFOLIO_RECORDER_ENABLED", false),
            root: logs_dir.into(),
            pre_signal: Duration::from_secs(parse_u64_env(
                "PORTFOLIO_RECORDER_PRE_SIGNAL_SECONDS",
                10,
            )?),
            setup_lead: Duration::from_secs(parse_u64_env(
                "PORTFOLIO_RECORDER_SETUP_LEAD_SECONDS",
                20,
            )?),
            activation_grace: Duration::from_secs(parse_u64_env(
                "PORTFOLIO_RECORDER_ACTIVATION_GRACE_SECONDS",
                5,
            )?),
            resolution_timeout: Duration::from_secs(parse_u64_env(
                "PORTFOLIO_RECORDER_RESOLUTION_TIMEOUT_SECONDS",
                600,
            )?),
            reconnect_delay: Duration::from_secs(parse_u64_env(
                "PORTFOLIO_RECORDER_RECONNECT_SECONDS",
                2,
            )?),
            delete_stream_after_summary: parse_bool_env(
                "PORTFOLIO_RECORDER_DELETE_STREAM_AFTER_SUMMARY",
                false,
            ),
            limit_price: parse_f64_env("LIMIT_PRICE_FIXED", 0.50)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SessionKey {
    market: MarketSlot,
    entry_time_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSession {
    session_id: String,
    key: SessionKey,
    slug: String,
    market_info: MarketInfo,
    stream_path: PathBuf,
    recorder_started_at: DateTime<Utc>,
    activated: bool,
    #[serde(default)]
    signal_ids: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecorderState {
    #[serde(default = "schema_version")]
    schema_version: u32,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    active_sessions: Vec<PersistedSession>,
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Clone)]
struct ManagedSession {
    persisted: PersistedSession,
    tx: mpsc::Sender<SessionControl>,
}

struct RecorderInner {
    settings: RecorderSettings,
    client: Arc<PolymarketClient>,
    feed_configs: BTreeMap<MarketSlot, Config>,
    sessions: Mutex<BTreeMap<SessionKey, ManagedSession>>,
    state_write_lock: Mutex<()>,
    completion_tx: mpsc::Sender<SessionKey>,
}

/// Shared handle used by the four Binance feed tasks through the portfolio coordinator.
#[derive(Clone)]
pub struct SignalMarketRecorder {
    inner: Arc<RecorderInner>,
}

impl SignalMarketRecorder {
    pub async fn start(
        settings: RecorderSettings,
        client: Arc<PolymarketClient>,
        feed_configs: BTreeMap<MarketSlot, Config>,
    ) -> Result<Option<Self>> {
        if !settings.enabled {
            return Ok(None);
        }
        fs::create_dir_all(settings.root.join("streams"))?;
        let (completion_tx, mut completion_rx) = mpsc::channel(64);
        let recorder = Self {
            inner: Arc::new(RecorderInner {
                settings,
                client,
                feed_configs,
                sessions: Mutex::new(BTreeMap::new()),
                state_write_lock: Mutex::new(()),
                completion_tx,
            }),
        };

        recorder.recover_active_sessions().await?;

        let cleanup = recorder.clone();
        tokio::spawn(async move {
            while let Some(key) = completion_rx.recv().await {
                cleanup.inner.sessions.lock().await.remove(&key);
                if let Err(err) = cleanup.save_state().await {
                    error!("État recorder non sauvegardé après finalisation: {err:#}");
                }
            }
        });

        for market in MarketSlot::ALL {
            let scheduler = recorder.clone();
            tokio::spawn(async move { scheduler.run_scheduler(market).await });
        }

        info!(
            "Recorder Polymarket actif | pré-signal={}s | timeout résolution={}s | logs={}",
            recorder.inner.settings.pre_signal.as_secs(),
            recorder.inner.settings.resolution_timeout.as_secs(),
            recorder.inner.settings.root.display()
        );
        Ok(Some(recorder))
    }

    /// Persists every strategy signal before portfolio grouping and sizing.
    pub async fn record_signals(
        &self,
        entry_time_ms: i64,
        signals: &[PortfolioSignal],
        signal_candle: &Candle,
    ) -> Result<()> {
        if signals.is_empty() {
            return Ok(());
        }
        let market = signals[0].market;
        if signals.iter().any(|signal| signal.market != market) {
            return Err(anyhow!("un lot recorder contient plusieurs marchés"));
        }
        let key = SessionKey {
            market,
            entry_time_ms,
        };
        let session = match self.ensure_session(key.clone()).await {
            Ok(session) => session,
            Err(err) => {
                self.persist_unattached_signals(entry_time_ms, signals, signal_candle, &err)
                    .await?;
                return Err(err);
            }
        };

        let detected_at = Utc::now();
        let mut new_signal_ids = Vec::new();
        let mut activations = BTreeMap::<String, Vec<String>>::new();
        {
            let sessions = self.inner.sessions.lock().await;
            let known = sessions
                .get(&key)
                .map(|managed| managed.persisted.signal_ids.as_slice())
                .unwrap_or_default();
            for signal in signals {
                let signal_id = signal_id(signal, entry_time_ms);
                if known.contains(&signal_id) {
                    continue;
                }
                let contributors = signals
                    .iter()
                    .filter(|candidate| candidate.prediction == signal.prediction)
                    .map(|candidate| candidate.strategy.key())
                    .collect::<Vec<_>>();
                let record = SignalRecord {
                    schema_version: SCHEMA_VERSION,
                    record_type: "SIGNAL_DETECTED",
                    signal_id: signal_id.clone(),
                    session_id: Some(session.persisted.session_id.clone()),
                    strategy: signal.strategy.key(),
                    market_slot: market.key(),
                    symbol: market.symbol(),
                    timeframe: market.interval(),
                    prediction: signal.prediction.to_string(),
                    signal_candle: CandleRecord::from(signal_candle),
                    signal_candle_close_time: signal.signal_close_time,
                    target_open_time_ms: entry_time_ms,
                    target_close_time_ms: entry_time_ms + market.interval_millis() - 1,
                    detected_at_local: detected_at,
                    slug: Some(session.persisted.slug.clone()),
                    up_token_id: Some(session.persisted.market_info.up_token_id.clone()),
                    down_token_id: Some(session.persisted.market_info.down_token_id.clone()),
                    contributor_strategies: contributors,
                    contributor_count: signals
                        .iter()
                        .filter(|candidate| candidate.prediction == signal.prediction)
                        .count(),
                    theoretical_limit_price: 0.50,
                    sizing_disposition: "PENDING_WINDOW_SIZING",
                    recorder_error: None,
                    raw_stream_path: Some(
                        session.persisted.stream_path.to_string_lossy().into_owned(),
                    ),
                };
                append_jsonl(&self.signals_path(), &record)?;
                activations
                    .entry(signal.prediction.to_string())
                    .or_default()
                    .push(signal_id.clone());
                new_signal_ids.push(signal_id);
            }
        }

        if new_signal_ids.is_empty() {
            return Ok(());
        }
        {
            let mut sessions = self.inner.sessions.lock().await;
            if let Some(managed) = sessions.get_mut(&key) {
                managed.persisted.activated = true;
                managed
                    .persisted
                    .signal_ids
                    .extend(new_signal_ids.iter().cloned());
            }
        }
        self.save_state().await?;
        session
            .tx
            .send(SessionControl::Activate {
                detected_at_unix_ms: detected_at.timestamp_millis(),
                groups: activations,
            })
            .await
            .map_err(|_| anyhow!("session recorder {} arrêtée", session.persisted.session_id))?;
        Ok(())
    }

    /// Adds the independent Binance result to the recorder session for this target candle.
    pub async fn record_binance_candle(&self, market: MarketSlot, candle: &Candle) {
        let key = SessionKey {
            market,
            entry_time_ms: candle.open_time.timestamp_millis(),
        };
        let tx = self
            .inner
            .sessions
            .lock()
            .await
            .get(&key)
            .filter(|session| session.persisted.activated)
            .map(|session| session.tx.clone());
        if let Some(tx) = tx {
            if tx
                .send(SessionControl::BinanceResult(CandleRecord::from(candle)))
                .await
                .is_err()
            {
                warn!("Résultat Binance non rattaché à {}", market.key());
            }
        }
    }

    /// Appends the final sizing disposition while keeping the detected signal immutable.
    pub async fn record_sizing_update(
        &self,
        entry_time_ms: i64,
        signal: &PortfolioSignal,
        disposition: &str,
        details: Value,
    ) -> Result<()> {
        let order_candidate_amount = (disposition == "DRY_RUN_ORDER_CANDIDATE")
            .then(|| details.get("combined_amount_usdc").and_then(Value::as_f64))
            .flatten();
        append_jsonl(
            &self.inner.settings.root.join("signal_sizing.jsonl"),
            &json!({
                "schema_version": SCHEMA_VERSION,
                "record_type": "SIGNAL_SIZING",
                "recorded_at_local": Utc::now(),
                "signal_id": signal_id(signal, entry_time_ms),
                "strategy": signal.strategy.key(),
                "market_slot": signal.market.key(),
                "prediction": signal.prediction.to_string(),
                "entry_time_ms": entry_time_ms,
                "disposition": disposition,
                "details": details,
            }),
        )?;
        if let Some(amount_usdc) = order_candidate_amount {
            let key = SessionKey {
                market: signal.market,
                entry_time_ms,
            };
            let tx = self
                .inner
                .sessions
                .lock()
                .await
                .get(&key)
                .map(|session| session.tx.clone());
            if let Some(tx) = tx {
                tx.send(SessionControl::OrderCandidate {
                    prediction: signal.prediction.to_string(),
                    amount_usdc,
                })
                .await
                .map_err(|_| anyhow!("session recorder sizing arrêtée: {}", signal.market.key()))?;
            }
        }
        Ok(())
    }

    async fn run_scheduler(&self, market: MarketSlot) {
        loop {
            let now_ms = Utc::now().timestamp_millis();
            let interval_ms = market.interval_millis();
            let entry_time_ms = (now_ms.div_euclid(interval_ms) + 1) * interval_ms;
            let prepare_at_ms = entry_time_ms
                - i64::try_from(
                    (self.inner.settings.pre_signal + self.inner.settings.setup_lead).as_millis(),
                )
                .unwrap_or(30_000);
            sleep_until_unix_ms(prepare_at_ms).await;
            let key = SessionKey {
                market,
                entry_time_ms,
            };
            loop {
                match self.ensure_session(key.clone()).await {
                    Ok(_) => break,
                    Err(err) if Utc::now().timestamp_millis() < entry_time_ms => {
                        warn!(
                            "Préparation recorder {} entry={}: {err:#}; nouvel essai",
                            market.key(),
                            entry_time_ms
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(err) => {
                        warn!(
                            "Préparation recorder {} entry={} abandonnée: {err:#}",
                            market.key(),
                            entry_time_ms
                        );
                        break;
                    }
                }
            }
            let expiry_ms = entry_time_ms
                + i64::try_from(self.inner.settings.activation_grace.as_millis()).unwrap_or(5_000);
            sleep_until_unix_ms(expiry_ms).await;
            if let Err(err) = self.discard_if_inactive(&key).await {
                warn!("Nettoyage recorder préparé: {err:#}");
            }
        }
    }

    async fn ensure_session(&self, key: SessionKey) -> Result<ManagedSession> {
        if let Some(session) = self.inner.sessions.lock().await.get(&key).cloned() {
            return Ok(session);
        }
        let config = self
            .inner
            .feed_configs
            .get(&key.market)
            .ok_or_else(|| anyhow!("configuration recorder absente: {}", key.market.key()))?;
        let slug = PolymarketClient::build_configured_slug(config, key.entry_time_ms);
        let market_info = self.resolve_with_retry(&slug).await?;

        if let Some(session) = self.inner.sessions.lock().await.get(&key).cloned() {
            return Ok(session);
        }
        let started_at = Utc::now();
        let date = DateTime::<Utc>::from_timestamp_millis(key.entry_time_ms)
            .unwrap_or(started_at)
            .format("%Y-%m-%d")
            .to_string();
        let stream_path = self
            .inner
            .settings
            .root
            .join("streams")
            .join(date)
            .join(key.market.key())
            .join(format!("{slug}.jsonl"));
        let persisted = PersistedSession {
            session_id: Uuid::new_v4().to_string(),
            key: key.clone(),
            slug,
            market_info,
            stream_path,
            recorder_started_at: started_at,
            activated: false,
            signal_ids: Vec::new(),
        };
        let managed = self.spawn_worker(persisted, false);
        self.inner
            .sessions
            .lock()
            .await
            .insert(key, managed.clone());
        self.save_state().await?;
        Ok(managed)
    }

    async fn resolve_with_retry(&self, slug: &str) -> Result<MarketInfo> {
        let mut last_error = None;
        for attempt in 1..=4 {
            match self.inner.client.resolve_market(slug).await {
                Ok(info) => return Ok(info),
                Err(err) => {
                    last_error = Some(err);
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("marché {slug} introuvable")))
            .with_context(|| format!("résolution du marché recorder {slug}"))
    }

    fn spawn_worker(&self, persisted: PersistedSession, resumed: bool) -> ManagedSession {
        let (tx, rx) = mpsc::channel(64);
        let worker = SessionWorker::new(
            persisted.clone(),
            self.inner.settings.clone(),
            rx,
            self.inner.completion_tx.clone(),
            resumed,
        );
        tokio::spawn(async move { worker.run().await });
        ManagedSession { persisted, tx }
    }

    async fn discard_if_inactive(&self, key: &SessionKey) -> Result<()> {
        let removed = {
            let mut sessions = self.inner.sessions.lock().await;
            if sessions
                .get(key)
                .is_some_and(|session| !session.persisted.activated)
            {
                sessions.remove(key)
            } else {
                None
            }
        };
        if let Some(session) = removed {
            let _ = session.tx.send(SessionControl::Discard).await;
            self.save_state().await?;
        }
        Ok(())
    }

    async fn recover_active_sessions(&self) -> Result<()> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("lecture état recorder {}", path.display()))?;
        let state: RecorderState = serde_json::from_str(&body)
            .with_context(|| format!("état recorder invalide {}", path.display()))?;
        let now_ms = Utc::now().timestamp_millis();
        for persisted in state.active_sessions.into_iter().filter(|session| {
            session.activated
                && session.key.entry_time_ms
                    + session.key.market.interval_millis()
                    + i64::try_from(self.inner.settings.resolution_timeout.as_millis())
                        .unwrap_or(600_000)
                    > now_ms
        }) {
            let key = persisted.key.clone();
            let managed = self.spawn_worker(persisted, true);
            self.inner.sessions.lock().await.insert(key, managed);
        }
        self.save_state().await
    }

    async fn persist_unattached_signals(
        &self,
        entry_time_ms: i64,
        signals: &[PortfolioSignal],
        candle: &Candle,
        recorder_error: &anyhow::Error,
    ) -> Result<()> {
        for signal in signals {
            let contributors = signals
                .iter()
                .filter(|candidate| candidate.prediction == signal.prediction)
                .map(|candidate| candidate.strategy.key())
                .collect::<Vec<_>>();
            append_jsonl(
                &self.signals_path(),
                &SignalRecord {
                    schema_version: SCHEMA_VERSION,
                    record_type: "SIGNAL_RECORDER_FAILED",
                    signal_id: signal_id(signal, entry_time_ms),
                    session_id: None,
                    strategy: signal.strategy.key(),
                    market_slot: signal.market.key(),
                    symbol: signal.market.symbol(),
                    timeframe: signal.market.interval(),
                    prediction: signal.prediction.to_string(),
                    signal_candle: CandleRecord::from(candle),
                    signal_candle_close_time: signal.signal_close_time,
                    target_open_time_ms: entry_time_ms,
                    target_close_time_ms: entry_time_ms + signal.market.interval_millis() - 1,
                    detected_at_local: Utc::now(),
                    slug: None,
                    up_token_id: None,
                    down_token_id: None,
                    contributor_count: contributors.len(),
                    contributor_strategies: contributors,
                    theoretical_limit_price: 0.50,
                    sizing_disposition: "RECORDER_FAILED",
                    recorder_error: Some(format!("{recorder_error:#}")),
                    raw_stream_path: None,
                },
            )?;
        }
        Ok(())
    }

    async fn save_state(&self) -> Result<()> {
        let _write_guard = self.inner.state_write_lock.lock().await;
        let active_sessions = self
            .inner
            .sessions
            .lock()
            .await
            .values()
            .map(|session| session.persisted.clone())
            .collect();
        let state = RecorderState {
            schema_version: SCHEMA_VERSION,
            updated_at: Some(Utc::now()),
            active_sessions,
        };
        atomic_write_json(&self.state_path(), &state)
    }

    fn state_path(&self) -> PathBuf {
        self.inner.settings.root.join("recorder_state.json")
    }

    fn signals_path(&self) -> PathBuf {
        self.inner.settings.root.join("signals.jsonl")
    }
}

#[derive(Debug, Serialize)]
struct SignalRecord<'a> {
    schema_version: u32,
    record_type: &'static str,
    signal_id: String,
    session_id: Option<String>,
    strategy: &'a str,
    market_slot: &'a str,
    symbol: &'a str,
    timeframe: &'a str,
    prediction: String,
    signal_candle: CandleRecord,
    signal_candle_close_time: DateTime<Utc>,
    target_open_time_ms: i64,
    target_close_time_ms: i64,
    detected_at_local: DateTime<Utc>,
    slug: Option<String>,
    up_token_id: Option<String>,
    down_token_id: Option<String>,
    contributor_strategies: Vec<&'a str>,
    contributor_count: usize,
    theoretical_limit_price: f64,
    sizing_disposition: &'static str,
    recorder_error: Option<String>,
    raw_stream_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandleRecord {
    open_time: DateTime<Utc>,
    close_time: DateTime<Utc>,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    direction: &'static str,
}

impl From<&Candle> for CandleRecord {
    fn from(candle: &Candle) -> Self {
        Self {
            open_time: candle.open_time,
            close_time: candle.close_time,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
            volume: candle.volume,
            direction: if candle.close > candle.open {
                "UP"
            } else if candle.close < candle.open {
                "DOWN"
            } else {
                "DOJI"
            },
        }
    }
}

enum SessionControl {
    Activate {
        detected_at_unix_ms: i64,
        groups: BTreeMap<String, Vec<String>>,
    },
    OrderCandidate {
        prediction: String,
        amount_usdc: f64,
    },
    BinanceResult(CandleRecord),
    Discard,
}

#[derive(Debug, Clone, Serialize)]
struct StreamEnvelope {
    schema_version: u32,
    session_id: String,
    connection_id: String,
    sequence: u64,
    received_at_local: DateTime<Utc>,
    received_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_timestamp: Option<String>,
    server_timestamp_out_of_order: bool,
    event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    market: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_text: Option<String>,
    payload: Value,
}

struct TimedEnvelope {
    received_at_unix_ms: i64,
    envelope: StreamEnvelope,
}

struct EventRing {
    retention_ms: i64,
    events: VecDeque<TimedEnvelope>,
}

impl EventRing {
    fn new(retention: Duration) -> Self {
        Self {
            retention_ms: i64::try_from(retention.as_millis()).unwrap_or(10_000),
            events: VecDeque::new(),
        }
    }

    fn push(&mut self, envelope: StreamEnvelope) {
        let received_at_unix_ms = envelope.received_at_unix_ms;
        self.events.push_back(TimedEnvelope {
            received_at_unix_ms,
            envelope,
        });
        let cutoff = received_at_unix_ms - self.retention_ms;
        while self
            .events
            .front()
            .is_some_and(|event| event.received_at_unix_ms < cutoff)
        {
            self.events.pop_front();
        }
    }

    fn drain(&mut self) -> impl Iterator<Item = StreamEnvelope> + '_ {
        self.events.drain(..).map(|event| event.envelope)
    }
}

struct SessionWorker {
    persisted: PersistedSession,
    settings: RecorderSettings,
    controls: mpsc::Receiver<SessionControl>,
    completion_tx: mpsc::Sender<SessionKey>,
    active: bool,
    resumed: bool,
    signal_ids: Vec<String>,
    ring: EventRing,
    writer: Option<tokio::fs::File>,
    sequence: u64,
    counts: BTreeMap<String, u64>,
    reconnect_count: u64,
    gap_count: u64,
    first_server_timestamp: Option<String>,
    last_server_timestamp: Option<String>,
    last_server_timestamp_ms: Option<i64>,
    binance_result: Option<CandleRecord>,
    resolution: Option<ResolutionRecord>,
    write_failed: bool,
    analyzer: SessionAnalyzer,
}

#[derive(Default)]
struct RecoveredStreamStats {
    sequence: u64,
    counts: BTreeMap<String, u64>,
    reconnect_count: u64,
    gap_count: u64,
    first_server_timestamp: Option<String>,
    last_server_timestamp: Option<String>,
    last_server_timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct ResolutionRecord {
    source: &'static str,
    winning_asset_id: Option<String>,
    winning_outcome: Option<String>,
    observed_at_local: DateTime<Utc>,
}

impl SessionWorker {
    fn new(
        persisted: PersistedSession,
        settings: RecorderSettings,
        controls: mpsc::Receiver<SessionControl>,
        completion_tx: mpsc::Sender<SessionKey>,
        resumed: bool,
    ) -> Self {
        let active = persisted.activated;
        let signal_ids = persisted.signal_ids.clone();
        let ring = EventRing::new(settings.pre_signal);
        let recovered = if resumed {
            recover_stream_stats(&persisted.stream_path).unwrap_or_default()
        } else {
            RecoveredStreamStats::default()
        };
        let analyzer = SessionAnalyzer::new(
            persisted.market_info.up_token_id.clone(),
            persisted.market_info.down_token_id.clone(),
            settings.limit_price,
            persisted.market_info.order_min_size,
        );
        Self {
            persisted,
            settings,
            controls,
            completion_tx,
            active,
            resumed,
            signal_ids,
            ring,
            writer: None,
            sequence: recovered.sequence,
            counts: recovered.counts,
            reconnect_count: recovered.reconnect_count,
            gap_count: recovered.gap_count,
            first_server_timestamp: recovered.first_server_timestamp,
            last_server_timestamp: recovered.last_server_timestamp,
            last_server_timestamp_ms: recovered.last_server_timestamp_ms,
            binance_result: None,
            resolution: None,
            write_failed: false,
            analyzer,
        }
    }

    async fn run(mut self) {
        if self.active {
            if let Err(err) = self.ensure_writer().await {
                error!("Ouverture stream recorder: {err:#}");
                self.write_failed = true;
            }
        }
        if self.resumed {
            self.gap_count += 1;
            self.capture_internal("gap", json!({"reason": "process_restart", "resumed": true}))
                .await;
        }

        let target_close_ms =
            self.persisted.key.entry_time_ms + self.persisted.key.market.interval_millis() - 1;
        let hard_deadline_ms = target_close_ms
            + i64::try_from(self.settings.resolution_timeout.as_millis()).unwrap_or(600_000);
        let mut connection_number = 0_u64;

        'session: loop {
            if Utc::now().timestamp_millis() >= hard_deadline_ms {
                break;
            }
            let connection =
                tokio::time::timeout(Duration::from_secs(15), connect_async(MARKET_WS_URL)).await;
            let (mut socket, _) = match connection {
                Ok(Ok(connection)) => connection,
                Ok(Err(err)) => {
                    self.gap_count += 1;
                    self.capture_internal("connect_failed", json!({"error": err.to_string()}))
                        .await;
                    self.capture_internal("gap", json!({"reason": "connect_failed"}))
                        .await;
                    if self.wait_reconnect_or_control().await {
                        break 'session;
                    }
                    continue;
                }
                Err(_) => {
                    self.gap_count += 1;
                    self.capture_internal("connect_timeout", json!({})).await;
                    self.capture_internal("gap", json!({"reason": "connect_timeout"}))
                        .await;
                    if self.wait_reconnect_or_control().await {
                        break 'session;
                    }
                    continue;
                }
            };

            connection_number += 1;
            let connection_id = Uuid::new_v4().to_string();
            if connection_number > 1 {
                self.reconnect_count += 1;
                self.gap_count += 1;
                self.capture_internal(
                    "gap",
                    json!({"reason": "websocket_reconnect", "connection_id": connection_id}),
                )
                .await;
                self.capture_internal(
                    "reconnecting",
                    json!({"connection_id": connection_id, "requires_book_snapshot": true}),
                )
                .await;
            }
            self.capture_with_connection(
                &connection_id,
                "connected",
                json!({"url": MARKET_WS_URL}),
                None,
            )
            .await;
            let subscription = json!({
                "assets_ids": [
                    self.persisted.market_info.up_token_id,
                    self.persisted.market_info.down_token_id
                ],
                "type": "market",
                "custom_feature_enabled": true
            });
            if let Err(err) = socket.send(Message::Text(subscription.to_string())).await {
                self.capture_with_connection(
                    &connection_id,
                    "subscribe_failed",
                    json!({"error": err.to_string()}),
                    None,
                )
                .await;
                continue;
            }
            self.capture_with_connection(&connection_id, "subscribed", subscription, None)
                .await;
            let mut needs_snapshot = connection_number > 1 || self.resumed;

            loop {
                let remaining_ms = hard_deadline_ms - Utc::now().timestamp_millis();
                if remaining_ms <= 0 {
                    break 'session;
                }
                tokio::select! {
                    control = self.controls.recv() => {
                        match control {
                            Some(SessionControl::Activate { detected_at_unix_ms, groups }) => {
                                self.activate(detected_at_unix_ms, groups).await
                            }
                            Some(SessionControl::OrderCandidate { prediction, amount_usdc }) => {
                                self.analyzer.set_order_candidate(&prediction, amount_usdc);
                            }
                            Some(SessionControl::BinanceResult(candle)) => {
                                self.binance_result = Some(candle);
                                self.capture_internal("binance_result", json!({"available": true})).await;
                                if self.resolution.is_some() {
                                    break 'session;
                                }
                            }
                            Some(SessionControl::Discard) | None => break 'session,
                        }
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(raw))) => {
                                let parsed = serde_json::from_str::<Value>(&raw)
                                    .unwrap_or_else(|_| json!({"unparsed_text": raw}));
                                let event_type = event_type(&parsed);
                                let contains_book = payload_contains_event(&parsed, "book");
                                let contains_resolution = payload_contains_event(&parsed, "market_resolved");
                                *self.counts.entry(event_type).or_default() += 1;
                                let server_timestamp = extract_string(&parsed, "timestamp");
                                for event in self
                                    .analyzer
                                    .process_payload(&parsed, Utc::now().timestamp_millis())
                                {
                                    self.capture_compact(
                                        &connection_id,
                                        event.event_type,
                                        event.payload,
                                        event.asset_id,
                                        event.server_timestamp.or_else(|| server_timestamp.clone()),
                                    )
                                    .await;
                                }
                                if contains_resolution {
                                    self.capture_compact(
                                        &connection_id,
                                        "market_resolved",
                                        compact_resolution_payload(&parsed),
                                        None,
                                        server_timestamp.clone(),
                                    )
                                    .await;
                                }
                                if payload_contains_event(&parsed, "tick_size_change") {
                                    self.capture_compact(
                                        &connection_id,
                                        "tick_size_change",
                                        compact_tick_size_payload(&parsed),
                                        extract_string(&parsed, "asset_id"),
                                        server_timestamp,
                                    )
                                    .await;
                                }
                                if needs_snapshot && contains_book {
                                    needs_snapshot = false;
                                    self.capture_with_connection(
                                        &connection_id,
                                        "resynced",
                                        json!({"book_snapshot_received": true}),
                                        None,
                                    ).await;
                                }
                                if contains_resolution {
                                    self.resolution = Some(resolution_from_payload(&parsed));
                                    if self.binance_result.is_some() {
                                        break 'session;
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                let _ = socket.send(Message::Pong(payload)).await;
                            }
                            Some(Ok(Message::Close(frame))) => {
                                self.capture_with_connection(
                                    &connection_id,
                                    "disconnected",
                                    json!({"close": format!("{frame:?}")}),
                                    None,
                                ).await;
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(err)) => {
                                self.capture_with_connection(
                                    &connection_id,
                                    "disconnected",
                                    json!({"error": err.to_string()}),
                                    None,
                                ).await;
                                break;
                            }
                            None => {
                                self.capture_with_connection(
                                    &connection_id,
                                    "disconnected",
                                    json!({"reason": "stream_ended"}),
                                    None,
                                ).await;
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(remaining_ms.min(1_000) as u64)) => {
                        if self.resolution.is_some()
                            && Utc::now().timestamp_millis() > target_close_ms + 10_000
                        {
                            break 'session;
                        }
                    }
                }
            }
        }

        if self.active {
            self.finalize().await;
        }
        let _ = self.completion_tx.send(self.persisted.key.clone()).await;
    }

    async fn wait_reconnect_or_control(&mut self) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(self.settings.reconnect_delay) => false,
            control = self.controls.recv() => {
                match control {
                    Some(SessionControl::Activate { detected_at_unix_ms, groups }) => {
                        self.activate(detected_at_unix_ms, groups).await;
                        false
                    }
                    Some(SessionControl::OrderCandidate { prediction, amount_usdc }) => {
                        self.analyzer.set_order_candidate(&prediction, amount_usdc);
                        false
                    }
                    Some(SessionControl::BinanceResult(candle)) => {
                        self.binance_result = Some(candle);
                        false
                    }
                    Some(SessionControl::Discard) | None => true,
                }
            }
        }
    }

    async fn activate(&mut self, detected_at_unix_ms: i64, groups: BTreeMap<String, Vec<String>>) {
        for ids in groups.values() {
            for id in ids {
                if !self.signal_ids.contains(id) {
                    self.signal_ids.push(id.clone());
                }
            }
        }
        if self.active && self.writer.is_some() {
            return;
        }
        self.active = true;
        if let Err(err) = self.ensure_writer().await {
            error!("Activation stream {}: {err:#}", self.persisted.slug);
            self.write_failed = true;
            return;
        }
        let buffered = self.ring.drain().collect::<Vec<_>>();
        for envelope in buffered {
            if let Err(err) = self.write_envelope(&envelope).await {
                error!("Flush pré-signal {}: {err:#}", self.persisted.slug);
                self.write_failed = true;
                break;
            }
        }
        for (prediction, ids) in groups {
            if let Some(event) = self
                .analyzer
                .activate(&prediction, ids, detected_at_unix_ms)
            {
                self.capture_compact(
                    "internal",
                    event.event_type,
                    event.payload,
                    event.asset_id,
                    event.server_timestamp,
                )
                .await;
            }
        }
        self.capture_internal(
            "signal_activated",
            json!({"signal_ids": self.signal_ids, "pre_signal_seconds": self.settings.pre_signal.as_secs()}),
        )
        .await;
    }

    async fn ensure_writer(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }
        let parent = self
            .persisted
            .stream_path
            .parent()
            .ok_or_else(|| anyhow!("stream recorder sans dossier parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        let writer = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.persisted.stream_path)
            .await?;
        self.writer = Some(writer);
        Ok(())
    }

    async fn capture_internal(&mut self, event_type: &str, payload: Value) {
        self.capture_with_connection("internal", event_type, payload, None)
            .await;
    }

    async fn capture_with_connection(
        &mut self,
        connection_id: &str,
        derived_event_type: &str,
        payload: Value,
        raw_text: Option<String>,
    ) {
        *self
            .counts
            .entry(derived_event_type.to_string())
            .or_default() += 1;
        self.capture_compact(connection_id, derived_event_type, payload, None, None)
            .await;
        let _ = raw_text;
    }

    async fn capture_compact(
        &mut self,
        connection_id: &str,
        derived_event_type: &str,
        payload: Value,
        asset_id: Option<String>,
        explicit_server_timestamp: Option<String>,
    ) {
        self.sequence += 1;
        let received_at = Utc::now();
        let server_timestamp =
            explicit_server_timestamp.or_else(|| extract_string(&payload, "timestamp"));
        let server_timestamp_ms = server_timestamp
            .as_deref()
            .and_then(|timestamp| timestamp.parse::<i64>().ok());
        let out_of_order = server_timestamp_ms
            .zip(self.last_server_timestamp_ms)
            .is_some_and(|(current, previous)| current < previous);
        if let Some(timestamp) = server_timestamp.clone() {
            self.first_server_timestamp
                .get_or_insert_with(|| timestamp.clone());
            self.last_server_timestamp = Some(timestamp);
        }
        if let Some(timestamp) = server_timestamp_ms {
            self.last_server_timestamp_ms = Some(timestamp);
        }
        let envelope = StreamEnvelope {
            schema_version: SCHEMA_VERSION,
            session_id: self.persisted.session_id.clone(),
            connection_id: connection_id.to_string(),
            sequence: self.sequence,
            received_at_local: received_at,
            received_at_unix_ms: received_at.timestamp_millis(),
            server_timestamp,
            server_timestamp_out_of_order: out_of_order,
            event_type: derived_event_type.to_string(),
            market: extract_string(&payload, "market"),
            asset_id: asset_id.or_else(|| extract_string(&payload, "asset_id")),
            raw_text: None,
            payload,
        };
        if self.active {
            if let Err(err) = self.write_envelope(&envelope).await {
                error!("Écriture stream {}: {err:#}", self.persisted.slug);
                self.write_failed = true;
            }
        } else {
            self.ring.push(envelope);
        }
    }

    async fn write_envelope(&mut self, envelope: &StreamEnvelope) -> Result<()> {
        self.ensure_writer().await?;
        let mut line = serde_json::to_vec(envelope)?;
        line.push(b'\n');
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("writer recorder non initialisé"))?;
        writer.write_all(&line).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn finalize(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.flush().await;
            let _ = writer.shutdown().await;
        }
        let completion_status = if self.write_failed {
            "RECORDER_FAILED"
        } else if self.resolution.is_some() && self.gap_count == 0 {
            "RESOLVED_COMPLETE"
        } else if self.resolution.is_some() {
            "RESOLVED_WITH_GAPS"
        } else if self.binance_result.is_some() {
            "BINANCE_RESULT_ONLY"
        } else {
            "RESOLUTION_TIMEOUT"
        };
        let metrics_path = self.settings.root.join("session_metrics.jsonl");
        let metrics = self.analyzer.clone().finish(SessionMetricContext {
            source_format: "runtime_compact_v2".to_string(),
            session_id: self.persisted.session_id.clone(),
            market_slot: self.persisted.key.market.key().to_string(),
            entry_time_ms: self.persisted.key.entry_time_ms,
            slug: self.persisted.slug.clone(),
            winning_asset_id: self
                .resolution
                .as_ref()
                .and_then(|resolution| resolution.winning_asset_id.clone()),
            winning_outcome: self
                .resolution
                .as_ref()
                .and_then(|resolution| resolution.winning_outcome.clone()),
            raw_stream_path: Some(self.persisted.stream_path.to_string_lossy().into_owned()),
            completion_status: completion_status.to_string(),
            gap_count: self.gap_count,
            reconnect_count: self.reconnect_count,
        });
        let metrics_saved = match append_jsonl(&metrics_path, &metrics) {
            Ok(()) => true,
            Err(err) => {
                error!("Métriques recorder non sauvegardées: {err:#}");
                false
            }
        };
        let mut raw_stream_deleted = false;
        if metrics_saved && self.settings.delete_stream_after_summary {
            match tokio::fs::remove_file(&self.persisted.stream_path).await {
                Ok(()) => raw_stream_deleted = true,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    raw_stream_deleted = true;
                }
                Err(err) => warn!(
                    "Stream compact conservé {}: {err}",
                    self.persisted.stream_path.display()
                ),
            }
        }
        let retained_stream_path = (!raw_stream_deleted)
            .then(|| self.persisted.stream_path.to_string_lossy().into_owned());
        let summary = json!({
            "schema_version": SCHEMA_VERSION,
            "record_type": "SESSION_FINALIZED",
            "session_id": self.persisted.session_id,
            "market_slot": self.persisted.key.market.key(),
            "entry_time_ms": self.persisted.key.entry_time_ms,
            "slug": self.persisted.slug,
            "up_token_id": self.persisted.market_info.up_token_id,
            "down_token_id": self.persisted.market_info.down_token_id,
            "recorder_started_at": self.persisted.recorder_started_at,
            "recorder_stopped_at": Utc::now(),
            "signal_ids": self.signal_ids,
            "first_server_timestamp": self.first_server_timestamp,
            "last_server_timestamp": self.last_server_timestamp,
            "event_counts": self.counts,
            "reconnect_count": self.reconnect_count,
            "gap_count": self.gap_count,
            "resolution": self.resolution,
            "binance_target_candle": self.binance_result,
            "completion_status": completion_status,
            "metrics_path": metrics_path,
            "raw_stream_path": retained_stream_path,
            "raw_stream_deleted": raw_stream_deleted,
        });
        if let Err(err) = append_jsonl(&self.settings.root.join("sessions.jsonl"), &summary) {
            error!("Résumé recorder non sauvegardé: {err:#}");
        }
    }
}

fn event_type(payload: &Value) -> String {
    if let Value::Array(events) = payload {
        let mut types = events
            .iter()
            .map(event_type)
            .filter(|kind| kind != "unknown")
            .collect::<Vec<_>>();
        types.sort();
        types.dedup();
        return if types.is_empty() {
            "batch".to_string()
        } else {
            format!("batch:{}", types.join(","))
        };
    }
    payload
        .get("event_type")
        .or_else(|| payload.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn payload_contains_event(payload: &Value, wanted: &str) -> bool {
    match payload {
        Value::Array(events) => events
            .iter()
            .any(|event| payload_contains_event(event, wanted)),
        Value::Object(_) => event_type(payload) == wanted,
        _ => false,
    }
}

fn extract_string(payload: &Value, key: &str) -> Option<String> {
    match payload {
        Value::Array(events) => events.iter().find_map(|event| extract_string(event, key)),
        Value::Object(object) => object.get(key).map(|value| match value {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        }),
        _ => None,
    }
}

fn resolution_from_payload(payload: &Value) -> ResolutionRecord {
    ResolutionRecord {
        source: "POLYMARKET_MARKET_WS",
        winning_asset_id: extract_string(payload, "winning_asset_id")
            .or_else(|| extract_string(payload, "asset_id")),
        winning_outcome: extract_string(payload, "winning_outcome")
            .or_else(|| extract_string(payload, "outcome")),
        observed_at_local: Utc::now(),
    }
}

fn compact_resolution_payload(payload: &Value) -> Value {
    json!({
        "winning_asset_id": extract_string(payload, "winning_asset_id")
            .or_else(|| extract_string(payload, "asset_id")),
        "winning_outcome": extract_string(payload, "winning_outcome")
            .or_else(|| extract_string(payload, "outcome")),
        "timestamp": extract_string(payload, "timestamp"),
    })
}

fn compact_tick_size_payload(payload: &Value) -> Value {
    json!({
        "asset_id": extract_string(payload, "asset_id"),
        "old_tick_size": extract_string(payload, "old_tick_size"),
        "new_tick_size": extract_string(payload, "new_tick_size"),
        "timestamp": extract_string(payload, "timestamp"),
    })
}

fn signal_id(signal: &PortfolioSignal, entry_time_ms: i64) -> String {
    format!(
        "{}:{}:{}:{}",
        signal.market.key(),
        entry_time_ms,
        signal.strategy.key(),
        match signal.prediction {
            Prediction::Up => "up",
            Prediction::Down => "down",
        }
    )
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("journal recorder sans dossier parent"))?;
    fs::create_dir_all(parent)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.flush()?;
    Ok(())
}

fn recover_stream_stats(path: &Path) -> Result<RecoveredStreamStats> {
    if !path.exists() {
        return Ok(RecoveredStreamStats::default());
    }
    let file = fs::File::open(path)?;
    let mut stats = RecoveredStreamStats::default();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(sequence) = event.get("sequence").and_then(Value::as_u64) {
            stats.sequence = stats.sequence.max(sequence);
        }
        if let Some(kind) = event.get("event_type").and_then(Value::as_str) {
            *stats.counts.entry(kind.to_string()).or_default() += 1;
            if kind == "reconnecting" {
                stats.reconnect_count += 1;
            }
            if kind == "gap" {
                stats.gap_count += 1;
            }
        }
        if let Some(timestamp) = event.get("server_timestamp").and_then(Value::as_str) {
            stats
                .first_server_timestamp
                .get_or_insert_with(|| timestamp.to_string());
            stats.last_server_timestamp = Some(timestamp.to_string());
            if let Ok(timestamp_ms) = timestamp.parse::<i64>() {
                stats.last_server_timestamp_ms = Some(timestamp_ms);
            }
        }
    }
    Ok(stats)
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("état recorder sans dossier parent"))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)?;
    Ok(())
}

async fn sleep_until_unix_ms(target_ms: i64) {
    let remaining = target_ms - Utc::now().timestamp_millis();
    if remaining > 0 {
        tokio::time::sleep(Duration::from_millis(remaining as u64)).await;
    }
}

fn parse_bool_env(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn parse_u64_env(key: &str, default: u64) -> Result<u64> {
    match env::var(key) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .with_context(|| format!("{key} doit être un entier positif")),
        Err(_) => Ok(default),
    }
}

fn parse_f64_env(key: &str, default: f64) -> Result<f64> {
    match env::var(key) {
        Ok(value) => value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("{key} doit être un nombre positif")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_jsonl, event_type, payload_contains_event, recover_stream_stats, signal_id,
        EventRing, RecorderState, SessionKey, StreamEnvelope, SCHEMA_VERSION,
    };
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::time::Duration;

    use crate::portfolio::{MarketSlot, PortfolioSignal, PortfolioStrategy};
    use crate::strategy::Prediction;

    fn envelope(at_ms: i64, sequence: u64) -> StreamEnvelope {
        StreamEnvelope {
            schema_version: SCHEMA_VERSION,
            session_id: "session".to_string(),
            connection_id: "connection".to_string(),
            sequence,
            received_at_local: Utc.timestamp_millis_opt(at_ms).unwrap(),
            received_at_unix_ms: at_ms,
            server_timestamp: None,
            server_timestamp_out_of_order: false,
            event_type: "book".to_string(),
            market: None,
            asset_id: None,
            raw_text: None,
            payload: json!({"event_type": "book"}),
        }
    }

    #[test]
    fn detects_supported_events_in_single_and_batch_payloads() {
        let payload = json!([
            {"event_type": "book"},
            {"event_type": "price_change"},
            {"event_type": "market_resolved"}
        ]);
        assert_eq!(
            event_type(&json!({"event_type": "best_bid_ask"})),
            "best_bid_ask"
        );
        assert_eq!(
            event_type(&payload),
            "batch:book,market_resolved,price_change"
        );
        assert!(payload_contains_event(&payload, "market_resolved"));
    }

    #[test]
    fn ring_retains_only_the_configured_pre_signal_window() {
        let mut ring = EventRing::new(Duration::from_secs(10));
        ring.push(envelope(1_000, 1));
        ring.push(envelope(10_999, 2));
        ring.push(envelope(11_001, 3));
        let sequences = ring.drain().map(|event| event.sequence).collect::<Vec<_>>();
        assert_eq!(sequences, vec![2, 3]);
    }

    #[test]
    fn raw_payload_and_exact_text_survive_serialization() {
        let raw = r#"{"event_type":"price_change","price":"0.500","asset_id":"42"}"#;
        let mut event = envelope(12_345, 7);
        event.event_type = "price_change".to_string();
        event.raw_text = Some(raw.to_string());
        event.payload = serde_json::from_str(raw).unwrap();

        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized["raw_text"], raw);
        assert_eq!(serialized["payload"]["price"], "0.500");
        assert_eq!(serialized["payload"]["asset_id"], "42");
    }

    #[test]
    fn old_empty_state_uses_safe_schema_defaults() {
        let state: RecorderState = serde_json::from_value(json!({})).unwrap();
        assert_eq!(state.schema_version, SCHEMA_VERSION);
        assert!(state.updated_at.is_none());
        assert!(state.active_sessions.is_empty());
    }

    #[test]
    fn simultaneous_strategies_share_the_same_market_session_key() {
        let entry_time_ms = 1_786_000_000_000;
        let key_a = SessionKey {
            market: MarketSlot::Eth5m,
            entry_time_ms,
        };
        let key_b = SessionKey {
            market: MarketSlot::Eth5m,
            entry_time_ms,
        };
        assert_eq!(key_a, key_b);

        let close = Utc.timestamp_millis_opt(entry_time_ms - 1).unwrap();
        let boll = PortfolioSignal {
            strategy: PortfolioStrategy::BollFade,
            market: MarketSlot::Eth5m,
            prediction: Prediction::Down,
            signal_close_time: close,
        };
        let trio = PortfolioSignal {
            strategy: PortfolioStrategy::TrioVote2,
            market: MarketSlot::Eth5m,
            prediction: Prediction::Down,
            signal_close_time: close,
        };
        assert_ne!(
            signal_id(&boll, entry_time_ms),
            signal_id(&trio, entry_time_ms)
        );
    }

    #[test]
    fn restart_recovers_sequence_counts_and_gap_history() {
        let path = std::env::temp_dir().join(format!(
            "meche050-recorder-test-{}-{}.jsonl",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut first = envelope(1_000, 41);
        first.server_timestamp = Some("1000".to_string());
        let mut second = envelope(2_000, 42);
        second.event_type = "gap".to_string();
        second.server_timestamp = Some("2000".to_string());
        append_jsonl(&path, &first).unwrap();
        append_jsonl(&path, &second).unwrap();

        let stats = recover_stream_stats(&path).unwrap();
        assert_eq!(stats.sequence, 42);
        assert_eq!(stats.counts.get("book"), Some(&1));
        assert_eq!(stats.counts.get("gap"), Some(&1));
        assert_eq!(stats.gap_count, 1);
        assert_eq!(stats.first_server_timestamp.as_deref(), Some("1000"));
        assert_eq!(stats.last_server_timestamp.as_deref(), Some("2000"));

        std::fs::remove_file(path).unwrap();
    }
}
