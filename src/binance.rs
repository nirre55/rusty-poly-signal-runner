use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{error, info, warn};

const BINANCE_WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BINANCE_WS_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(90);
const BINANCE_WS_DEFAULT_BASE: &str = "wss://stream.binance.com:9443/ws";
const BINANCE_WS_PORT_443_BASE: &str = "wss://stream.binance.com:443/ws";
const BINANCE_WS_MARKET_DATA_BASE: &str = "wss://data-stream.binance.vision:443/ws";

// Les champs high, low, volume seront utilisés par les stratégies futures (EMA, ATR, etc.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Candle {
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    #[allow(dead_code)]
    pub is_closed: bool,
}

impl Candle {
    pub fn is_green(&self) -> bool {
        self.close >= self.open
    }

    pub fn is_red(&self) -> bool {
        self.close < self.open
    }
}

#[derive(Debug, Deserialize)]
struct KlineEvent {
    #[serde(rename = "k")]
    kline: KlineData,
}

#[derive(Debug, Deserialize)]
struct KlineData {
    #[serde(rename = "t")]
    open_time: i64,
    #[serde(rename = "T")]
    close_time: i64,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "v")]
    volume: String,
    #[serde(rename = "x")]
    is_closed: bool,
}

/// Parse une réponse brute de l'API klines Binance en Vec<Candle>.
/// Les entrées malformées sont ignorées (filter_map retourne None).
pub fn parse_klines(rows: Vec<serde_json::Value>) -> Vec<Candle> {
    rows.into_iter()
        .filter_map(|row| {
            let arr = row.as_array()?;
            if arr.len() < 7 {
                return None;
            }
            // P2 : ? au lieu de unwrap_or_default() — timestamp invalide → bougie ignorée
            Some(Candle {
                open_time: DateTime::from_timestamp_millis(arr[0].as_i64()?)?,
                close_time: DateTime::from_timestamp_millis(arr[6].as_i64()?)?,
                open: arr[1].as_str()?.parse().ok()?,
                high: arr[2].as_str()?.parse().ok()?,
                low: arr[3].as_str()?.parse().ok()?,
                close: arr[4].as_str()?.parse().ok()?,
                volume: arr[5].as_str()?.parse().ok()?,
                is_closed: true,
            })
        })
        .collect()
}

/// Récupère les `limit` dernières bougies fermées via l'API REST Binance.
/// Utilisé au démarrage pour précharger l'historique avant le WebSocket.
pub async fn fetch_historical_candles(
    symbol: &str,
    interval: &str,
    limit: u32,
) -> Result<Vec<Candle>> {
    let url = format!(
        "https://api.binance.com/api/v3/klines?symbol={}&interval={}&limit={}",
        symbol.to_uppercase(),
        interval,
        limit
    );

    // P6 : client avec timeout pour éviter un blocage indéfini
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("Erreur création client HTTP Binance")?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Erreur HTTP Binance REST")?
        .json::<Vec<serde_json::Value>>()
        .await
        .context("Erreur parsing JSON klines")?;

    let candles = parse_klines(response);

    info!(
        "Préchargement : {} bougies historiques chargées ({} {})",
        candles.len(),
        symbol,
        interval
    );
    Ok(candles)
}

pub async fn stream_candles(
    url: &str,
    symbol: &str,
    interval: &str,
    tx: mpsc::Sender<Candle>,
) -> Result<()> {
    let ws_bases = binance_ws_bases(url);
    info!(
        "Connexion au WebSocket Binance: {}",
        ws_url(&ws_bases[0], symbol, interval)
    );

    let mut reconnect_delay_secs = 5u64;
    let mut last_close_time_ms: i64 = 0;

    loop {
        let connect_result = connect_binance_ws(&ws_bases, symbol, interval).await;

        match connect_result {
            Ok((ws_stream, connected_url)) => {
                if connected_url != ws_url(&ws_bases[0], symbol, interval) {
                    warn!(
                        "[RECONNECT] WebSocket Binance connecte via endpoint fallback: {}",
                        connected_url
                    );
                }
                info!("Connecté au WebSocket Binance");

                // Rattraper les bougies manquées pendant la déconnexion
                if last_close_time_ms > 0 {
                    match catch_up_missed_candles(symbol, interval, last_close_time_ms, &tx).await {
                        Ok(count) if count > 0 => {
                            info!("[RECONNECT] {} bougies manquées rattrapées", count);
                        }
                        Err(e) => {
                            warn!(
                                "[RECONNECT] Impossible de rattraper les bougies manquées: {}",
                                e
                            );
                        }
                        _ => {}
                    }
                }

                reconnect_delay_secs = 5; // reset après connexion réussie
                let (mut write, mut read) = ws_stream.split();

                loop {
                    let msg = match tokio::time::timeout(BINANCE_WS_INACTIVITY_TIMEOUT, read.next())
                        .await
                    {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            warn!("WebSocket Binance ferme par le serveur, reconnexion...");
                            break;
                        }
                        Err(_) => {
                            warn!(
                                "WebSocket Binance silencieux depuis {}s, reconnexion...",
                                BINANCE_WS_INACTIVITY_TIMEOUT.as_secs()
                            );
                            break;
                        }
                    };

                    match msg {
                        Ok(Message::Text(text)) => {
                            match serde_json::from_str::<KlineEvent>(&text) {
                                Ok(event) => {
                                    if !event.kline.is_closed {
                                        continue;
                                    }

                                    // P2 : rejeter les timestamps invalides au lieu de retourner l'epoch
                                    let open_time = match DateTime::from_timestamp_millis(
                                        event.kline.open_time,
                                    ) {
                                        Some(t) => t,
                                        None => {
                                            warn!(
                                                "open_time invalide ({}), bougie ignorée",
                                                event.kline.open_time
                                            );
                                            continue;
                                        }
                                    };
                                    let close_time = match DateTime::from_timestamp_millis(
                                        event.kline.close_time,
                                    ) {
                                        Some(t) => t,
                                        None => {
                                            warn!(
                                                "close_time invalide ({}), bougie ignorée",
                                                event.kline.close_time
                                            );
                                            continue;
                                        }
                                    };

                                    // P4 : rejeter les prix non parseable ou nuls/négatifs
                                    let open: f64 = match event.kline.open.parse() {
                                        Ok(v) if v > 0.0 => v,
                                        _ => {
                                            warn!(
                                                "Prix open invalide '{}', bougie ignorée",
                                                event.kline.open
                                            );
                                            continue;
                                        }
                                    };
                                    let high: f64 = match event.kline.high.parse() {
                                        Ok(v) if v > 0.0 => v,
                                        _ => {
                                            warn!(
                                                "Prix high invalide '{}', bougie ignorée",
                                                event.kline.high
                                            );
                                            continue;
                                        }
                                    };
                                    let low: f64 = match event.kline.low.parse() {
                                        Ok(v) if v > 0.0 => v,
                                        _ => {
                                            warn!(
                                                "Prix low invalide '{}', bougie ignorée",
                                                event.kline.low
                                            );
                                            continue;
                                        }
                                    };
                                    let close: f64 = match event.kline.close.parse() {
                                        Ok(v) if v > 0.0 => v,
                                        _ => {
                                            warn!(
                                                "Prix close invalide '{}', bougie ignorée",
                                                event.kline.close
                                            );
                                            continue;
                                        }
                                    };
                                    let volume: f64 = event.kline.volume.parse().unwrap_or(0.0);

                                    let candle = Candle {
                                        open_time,
                                        close_time,
                                        open,
                                        high,
                                        low,
                                        close,
                                        volume,
                                        is_closed: true,
                                    };

                                    let candle_close_ms = candle.close_time.timestamp_millis();

                                    // Ignorer les bougies déjà envoyées (ex: après rattrapage)
                                    if candle_close_ms <= last_close_time_ms {
                                        continue;
                                    }

                                    match tx.send(candle).await {
                                        Ok(_) => {
                                            last_close_time_ms = candle_close_ms;
                                        }
                                        Err(_) => {
                                            return Ok(());
                                        }
                                    }
                                }
                                Err(e) => warn!("Impossible de parser le message kline: {}", e),
                            }
                        }
                        Ok(Message::Ping(payload)) => {
                            if let Err(e) = write.send(Message::Pong(payload)).await {
                                warn!("Impossible de repondre au ping Binance: {}", e);
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => {
                            warn!("WebSocket fermé, reconnexion…");
                            break;
                        }
                        Err(e) => {
                            error!("Erreur WebSocket: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(BinanceWsConnectError::Connect(e)) => {
                error!(
                    "Echec connexion WebSocket Binance: {}. Si le message contient 'failed to lookup address information', c'est une panne DNS/reseau temporaire; le bot retente avec backoff.",
                    e
                );
            }
            Err(BinanceWsConnectError::Timeout) => {
                error!("Timeout connexion WebSocket Binance (15s)");
            }
        }

        warn!(
            "[RECONNECT] Reconnexion au WebSocket Binance dans {}s…",
            reconnect_delay_secs
        );
        tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
        reconnect_delay_secs = (reconnect_delay_secs * 2).min(60);
    }
}

// Endpoints de secours pour les bougies publiques Binance.
#[derive(Debug)]
enum BinanceWsConnectError {
    Connect(tokio_tungstenite::tungstenite::Error),
    Timeout,
}

type BinanceWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect_binance_ws(
    bases: &[String],
    symbol: &str,
    interval: &str,
) -> std::result::Result<(BinanceWsStream, String), BinanceWsConnectError> {
    let mut last_error = None;
    let mut saw_timeout = false;

    for base in bases {
        let url = ws_url(base, symbol, interval);
        let connect_result =
            tokio::time::timeout(BINANCE_WS_CONNECT_TIMEOUT, connect_async(&url)).await;

        match connect_result {
            Ok(Ok((ws_stream, _))) => return Ok((ws_stream, url)),
            Ok(Err(e)) => {
                warn!("Endpoint WebSocket Binance indisponible {}: {}", url, e);
                last_error = Some(e);
            }
            Err(_) => {
                warn!(
                    "Timeout connexion WebSocket Binance sur endpoint {}, essai suivant...",
                    url
                );
                saw_timeout = true;
            }
        }
    }

    match last_error {
        Some(e) => Err(BinanceWsConnectError::Connect(e)),
        None if saw_timeout => Err(BinanceWsConnectError::Timeout),
        None => unreachable!("au moins un endpoint Binance doit etre configure"),
    }
}

fn ws_url(base: &str, symbol: &str, interval: &str) -> String {
    format!("{}/{symbol}@kline_{interval}", base.trim_end_matches('/'))
}

fn binance_ws_bases(primary: &str) -> Vec<String> {
    let mut bases = Vec::new();
    push_unique_ws_base(&mut bases, primary);
    push_unique_ws_base(&mut bases, BINANCE_WS_DEFAULT_BASE);
    push_unique_ws_base(&mut bases, BINANCE_WS_PORT_443_BASE);
    push_unique_ws_base(&mut bases, BINANCE_WS_MARKET_DATA_BASE);
    bases
}

fn push_unique_ws_base(bases: &mut Vec<String>, base: &str) {
    let trimmed = base.trim().trim_end_matches('/');
    if !trimmed.is_empty() && !bases.iter().any(|known| known == trimmed) {
        bases.push(trimmed.to_string());
    }
}

/// Apres reconnexion WS, recupere les bougies fermees manquees via REST
/// et les injecte dans le channel pour que la strategie reste a jour.
async fn catch_up_missed_candles(
    symbol: &str,
    interval: &str,
    last_close_time_ms: i64,
    tx: &mpsc::Sender<Candle>,
) -> Result<usize> {
    let candles = fetch_historical_candles(symbol, interval, 30).await?;
    let now_ms = Utc::now().timestamp_millis();
    let mut count = 0;

    for candle in candles {
        let ct_ms = candle.close_time.timestamp_millis();
        // Envoyer uniquement les bougies fermées qu'on n'a pas encore vues
        if ct_ms > last_close_time_ms && ct_ms < now_ms {
            match tx.send(candle).await {
                Ok(_) => count += 1,
                Err(_) => break,
            }
        }
    }
    Ok(count)
}
