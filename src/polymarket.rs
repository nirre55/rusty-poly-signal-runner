use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::signers::{local::PrivateKeySigner, Signer};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use chrono::{DateTime, Datelike, Timelike, Utc};
use chrono_tz::America::New_York;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::request::{BalanceAllowanceRequest, OrdersRequest};
use polymarket_client_sdk_v2::clob::types::{
    Amount, AssetType, OrderType as SdkOrderType, Side as SdkSide,
    SignatureType as SdkSignatureType,
};
use polymarket_client_sdk_v2::clob::{Client as SdkClobClient, Config as SdkConfig};
use polymarket_client_sdk_v2::types::Decimal;
use polymarket_client_sdk_v2::POLYGON;
use serde::{Deserialize, Serialize};
use serde_json;
use sha2::Sha256;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{
    Config, ExecutionMode, LimitPriceHighGuard, LimitPriceReference, MarketOrderType,
    PolymarketSlugFormat,
};
use crate::strategy::{Prediction, Signal};

// ── Constantes ───────────────────────────────────────────────────────────────

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";
const DEFAULT_CLOB_API_BASE: &str = "https://clob.polymarket.com";
const MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const CTF_EXCHANGE_ADDR: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
const POLYGON_CHAIN_ID: u64 = 137;
const CLOB_AUTH_MSG: &str = "This message attests that I control the given wallet";
const FOK_RETRY_DELAYS_SECS: [u64; 3] = [3, 7, 10];
const CLOB_TEMPORARY_RETRY_DELAYS_SECS: [u64; 3] = [5, 15, 30];
const MAX_RETRY_AFTER_SECS: u64 = 60;

// ── Types publics (API inchangée) ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketInfo {
    pub condition_id: String,
    pub up_token_id: String,
    pub down_token_id: String,
    pub slug: String,
    /// Taille minimale d'un ordre en shares (ex: 5.0 = 5 shares minimum)
    pub order_min_size: f64,
}

#[derive(Debug, Clone)]
pub struct OrderResult {
    pub order_id: String,
    pub status: String,
    pub amount_usdc: f64,
    pub limit_price: Option<f64>,
    pub execution_price: Option<f64>,
    pub execution_price_source: Option<String>,
    pub size_matched: Option<f64>,
    #[allow(dead_code)]
    pub submitted_at: DateTime<Utc>,
    pub ack_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderExecutionDetails {
    pub status: String,
    pub order_price: Option<f64>,
    pub average_price: Option<f64>,
    pub size_matched: Option<f64>,
}

impl OrderExecutionDetails {
    fn execution_price_with_source(&self) -> Option<(f64, &'static str)> {
        self.average_price
            .map(|price| (price, "average_price"))
            .or_else(|| self.order_price.map(|price| (price, "order_price")))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrderSummary {
    pub id: String,
    pub status: String,
    pub asset_id: String,
    pub side: String,
    pub original_size: String,
    pub size_matched: String,
    pub price: String,
    pub outcome: String,
    pub order_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOrderSummary {
    pub canceled: Vec<String>,
    pub not_canceled: Vec<(String, String)>,
}

// ── Types internes ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OrderBookLevel {
    price: String,
    #[serde(default)]
    size: String,
}

#[derive(Deserialize)]
struct OrderBook {
    #[serde(default)]
    asks: Vec<OrderBookLevel>,
    #[serde(default)]
    bids: Vec<OrderBookLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitOrderQuote {
    pub limit_price: f64,
    pub expected_shares: f64,
    pub effective_usdc: f64,
    pub adjusted_to_min_size: bool,
    pub high_guard_applied: bool,
    pub uncapped_limit_price: f64,
}

#[derive(Deserialize)]
struct GammaMarket {
    #[serde(alias = "conditionId")]
    condition_id: String,
    /// JSON-encodé : "[\"Up\", \"Down\"]"
    outcomes: String,
    /// JSON-encodé : "[\"<token_id_up>\", \"<token_id_down>\"]"
    #[serde(alias = "clobTokenIds")]
    clob_token_ids: String,
    /// Taille minimale d'un ordre en shares
    #[serde(alias = "orderMinSize", default = "default_order_min_size")]
    order_min_size: f64,
}

fn default_order_min_size() -> f64 {
    5.0
}

pub fn parse_gamma_market_body(slug: &str, body: &str) -> Result<MarketInfo> {
    let markets: Vec<GammaMarket> = serde_json::from_str(body).map_err(|e| {
        anyhow!(
            "Gamma API parse JSON: {} | body={}",
            e,
            &body[..body.len().min(300)]
        )
    })?;

    let market = markets
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Aucun marche trouve pour le slug '{}'", slug))?;

    let outcomes: Vec<String> = serde_json::from_str(&market.outcomes)
        .map_err(|e| anyhow!("Impossible de parser outcomes: {}", e))?;
    let token_ids: Vec<String> = serde_json::from_str(&market.clob_token_ids)
        .map_err(|e| anyhow!("Impossible de parser clobTokenIds: {}", e))?;

    let up_idx = outcomes
        .iter()
        .position(|o| o.eq_ignore_ascii_case("up"))
        .ok_or_else(|| anyhow!("Outcome 'Up' introuvable pour le slug '{}'", slug))?;
    let down_idx = outcomes
        .iter()
        .position(|o| o.eq_ignore_ascii_case("down"))
        .ok_or_else(|| anyhow!("Outcome 'Down' introuvable pour le slug '{}'", slug))?;

    let up_token_id = token_ids
        .get(up_idx)
        .ok_or_else(|| anyhow!("Token UP manquant dans clobTokenIds pour '{}'", slug))?
        .clone();
    let down_token_id = token_ids
        .get(down_idx)
        .ok_or_else(|| anyhow!("Token DOWN manquant dans clobTokenIds pour '{}'", slug))?
        .clone();

    Ok(MarketInfo {
        condition_id: market.condition_id,
        up_token_id,
        down_token_id,
        slug: slug.to_string(),
        order_min_size: market.order_min_size,
    })
}

pub fn parse_best_ask_body(body: &str) -> Option<f64> {
    let book: OrderBook = serde_json::from_str(body).ok()?;
    book.asks
        .iter()
        .filter_map(|level| level.price.parse::<f64>().ok())
        .min_by(f64::total_cmp)
}

pub fn parse_best_bid_body(body: &str) -> Option<f64> {
    let book: OrderBook = serde_json::from_str(body).ok()?;
    book.bids
        .iter()
        .filter_map(|level| level.price.parse::<f64>().ok())
        .max_by(f64::total_cmp)
}

pub fn parse_book_reference_price_body(body: &str, reference: LimitPriceReference) -> Option<f64> {
    match reference {
        LimitPriceReference::BestAsk => parse_best_ask_body(body),
        LimitPriceReference::BestBid => parse_best_bid_body(body),
    }
}

pub fn parse_market_ws_best_ask_message(token_id: &str, body: &str) -> Option<f64> {
    parse_market_ws_reference_price_message(token_id, body, LimitPriceReference::BestAsk)
}

pub fn parse_market_ws_reference_price_message(
    token_id: &str,
    body: &str,
    reference: LimitPriceReference,
) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let event_type = value.get("event_type")?.as_str()?;

    match event_type {
        "best_bid_ask" => {
            let asset_id = value.get("asset_id")?.as_str()?;
            if asset_id == token_id {
                let field = match reference {
                    LimitPriceReference::BestAsk => "best_ask",
                    LimitPriceReference::BestBid => "best_bid",
                };
                value.get(field)?.as_str()?.parse::<f64>().ok()
            } else {
                None
            }
        }
        "book" => {
            let asset_id = value.get("asset_id")?.as_str()?;
            if asset_id == token_id {
                let side = match reference {
                    LimitPriceReference::BestAsk => "asks",
                    LimitPriceReference::BestBid => "bids",
                };
                let prices = value
                    .get(side)?
                    .as_array()?
                    .iter()
                    .filter_map(|level| level.get("price")?.as_str()?.parse::<f64>().ok());
                match reference {
                    LimitPriceReference::BestAsk => prices.min_by(f64::total_cmp),
                    LimitPriceReference::BestBid => prices.max_by(f64::total_cmp),
                }
            } else {
                None
            }
        }
        "price_change" => {
            let field = match reference {
                LimitPriceReference::BestAsk => "best_ask",
                LimitPriceReference::BestBid => "best_bid",
            };
            let prices = value
                .get("price_changes")?
                .as_array()?
                .iter()
                .filter(|change| {
                    change.get("asset_id").and_then(|id| id.as_str()) == Some(token_id)
                })
                .filter_map(|change| change.get(field)?.as_str()?.parse::<f64>().ok());
            match reference {
                LimitPriceReference::BestAsk => prices.min_by(f64::total_cmp),
                LimitPriceReference::BestBid => prices.max_by(f64::total_cmp),
            }
        }
        _ => None,
    }
}

pub fn calculate_limit_order_quote(
    amount_usdc: f64,
    min_size: f64,
    reference_price: Option<f64>,
    limit_price_offset: f64,
    high_guard: LimitPriceHighGuard,
) -> LimitOrderQuote {
    let base_price = reference_price.unwrap_or(0.50);
    let uncapped_limit_price = (base_price + limit_price_offset).clamp(0.01, 0.99);
    let high_guard_applied = high_guard.enabled && uncapped_limit_price > high_guard.threshold;
    let limit_price = if high_guard_applied {
        high_guard.price.clamp(0.01, 0.99)
    } else {
        uncapped_limit_price
    };
    let expected_shares = amount_usdc / limit_price;

    if expected_shares < min_size {
        let effective_usdc = (min_size * limit_price * 100.0).ceil() / 100.0;
        LimitOrderQuote {
            limit_price,
            expected_shares,
            effective_usdc,
            adjusted_to_min_size: true,
            high_guard_applied,
            uncapped_limit_price,
        }
    } else {
        LimitOrderQuote {
            limit_price,
            expected_shares,
            effective_usdc: amount_usdc,
            adjusted_to_min_size: false,
            high_guard_applied,
            uncapped_limit_price,
        }
    }
}

pub fn calculate_available_shares_up_to_price(body: &str, limit_price: f64) -> Option<f64> {
    let book: OrderBook = serde_json::from_str(body).ok()?;
    Some(
        book.asks
            .iter()
            .filter_map(|level| {
                let price = level.price.parse::<f64>().ok()?;
                let size = level.size.parse::<f64>().ok()?;
                (price <= limit_price).then_some(size)
            })
            .sum(),
    )
}

pub fn validate_sufficient_usdc_balance(required_usdc: f64, available_usdc: f64) -> Result<()> {
    if available_usdc + 0.000_001 < required_usdc {
        return Err(anyhow!(
            "solde USDC insuffisant pour l'ordre: requis={:.2} disponible={:.2}",
            required_usdc,
            available_usdc
        ));
    }

    Ok(())
}

pub fn parse_order_status_body(body: &str) -> Result<String> {
    Ok(parse_order_execution_details_body(body)?.status)
}

pub fn parse_order_execution_details_body(body: &str) -> Result<OrderExecutionDetails> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        anyhow!(
            "parse order status JSON: {} | body={}",
            e,
            &body[..body.len().min(300)]
        )
    })?;

    let order = order_details_value(&value);
    let status = order
        .and_then(|order| order.get("status"))
        .and_then(|status| status.as_str())
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "order status absent dans la reponse Polymarket | body={}",
                &body[..body.len().min(300)]
            )
        })?;

    Ok(OrderExecutionDetails {
        status: status.to_string(),
        order_price: order.and_then(|order| numeric_field(order, &["price"])),
        average_price: order.and_then(|order| {
            numeric_field(
                order,
                &["average_price", "avg_price", "averagePrice", "avgPrice"],
            )
        }),
        size_matched: order.and_then(|order| {
            numeric_field(
                order,
                &["size_matched", "matched_size", "sizeMatched", "matchedSize"],
            )
        }),
    })
}

fn order_details_value(value: &serde_json::Value) -> Option<&serde_json::Value> {
    value
        .get("order")
        .or_else(|| value.as_array().and_then(|orders| orders.first()))
        .or(Some(value))
}

fn numeric_field(value: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(json_number)
}

fn json_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
}

#[derive(Debug, Clone)]
struct ApiCreds {
    api_key: String,
    secret: String,
    passphrase: String,
    address: String,
}

#[derive(Deserialize)]
struct ApiKeyResponse {
    #[serde(rename = "apiKey")]
    api_key: String,
    secret: String,
    passphrase: String,
}

// ── Client ────────────────────────────────────────────────────────────────────

pub struct PolymarketClient {
    config: Config,
    gamma_api_base: String,
    clob_api_base: String,
    http: reqwest::Client,
    signer: Option<Arc<PrivateKeySigner>>,
    api_creds: Mutex<Option<ApiCreds>>,
    /// Cache (slug → MarketInfo) : un marché actif à la fois, renouvelé si le slug change.
    market_cache: Mutex<Option<(String, MarketInfo)>>,
    /// Client SDK authentifié, créé une seule fois et réutilisé pour tous les ordres.
    /// Conserve les caches internes (tick_size, fee_rate_bps) entre les appels.
    sdk_client: Mutex<Option<SdkClobClient<Authenticated<Normal>>>>,
    /// Signer SDK pré-construit avec chain_id, réutilisé pour signer les ordres.
    sdk_signer: Option<PrivateKeySigner>,
}

impl PolymarketClient {
    pub fn new(config: Config) -> Self {
        Self::new_with_api_bases(config, GAMMA_API_BASE, "")
    }

    pub fn new_with_api_bases(
        config: Config,
        gamma_api_base: impl Into<String>,
        clob_api_base_override: impl AsRef<str>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .tcp_keepalive(Some(Duration::from_secs(20)))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(90))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let parsed_pk = config.evm_private_key.as_deref().and_then(|pk| {
            let pk = pk.trim_start_matches("0x");
            match PrivateKeySigner::from_str(pk) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!(
                        "POLYMARKET_PRIVATE_KEY invalide — mode réel désactivé: {}",
                        e
                    );
                    None
                }
            }
        });

        let signer = parsed_pk.as_ref().map(|s| Arc::new(s.clone()));
        let sdk_signer = parsed_pk.map(|s| s.with_chain_id(Some(POLYGON)));

        let gamma_api_base = gamma_api_base.into().trim_end_matches('/').to_string();
        let clob_override = clob_api_base_override.as_ref().trim().trim_end_matches('/');
        let configured_clob_api_base = if clob_override.is_empty() {
            config.polymarket_api_url.trim().trim_end_matches('/')
        } else {
            clob_override
        };
        let clob_api_base = if configured_clob_api_base.is_empty() {
            DEFAULT_CLOB_API_BASE.to_string()
        } else {
            configured_clob_api_base.to_string()
        };

        Self {
            config,
            gamma_api_base,
            clob_api_base,
            http,
            signer,
            api_creds: Mutex::new(None),
            market_cache: Mutex::new(None),
            sdk_client: Mutex::new(None),
            sdk_signer,
        }
    }

    pub fn clob_api_base(&self) -> &str {
        &self.clob_api_base
    }

    pub fn gamma_api_base(&self) -> &str {
        &self.gamma_api_base
    }

    /// Pré-chauffe la connexion TCP/TLS vers le CLOB (payer le handshake une seule fois).
    /// À appeler dans `main()` avant la boucle de trading.
    pub async fn warm_up(&self) {
        match self
            .http
            .get(format!("{}/ok", self.clob_api_base))
            .send()
            .await
        {
            Ok(_) => info!("Connexion CLOB Polymarket pré-chauffée"),
            Err(e) => warn!("warm_up CLOB échoué (non bloquant): {}", e),
        }
        // Pré-créer le client SDK authentifié pour que le premier ordre soit aussi rapide que les suivants.
        match self.get_or_create_sdk_client().await {
            Ok(_) => info!("Client SDK Polymarket pré-authentifié"),
            Err(e) => warn!("warm_up SDK échoué (non bloquant): {}", e),
        }
    }

    /// Construit le slug Polymarket depuis le timestamp d'ouverture de la bougie cible.
    /// Format : `{prefix}-<UNIX_TIMESTAMP_SECONDES>`
    /// Exemple : "btc-updown-5m-1742256301"
    pub fn build_slug(prefix: &str, open_time_ms: i64) -> String {
        let unix_secs = open_time_ms / 1000;
        format!("{}-{}", prefix, unix_secs)
    }

    /// Résout slug → condition_id + tokenIds UP/DOWN via l'API Gamma Polymarket.
    /// Résultat mis en cache : un seul appel réseau par slug distinct.
    pub fn build_configured_slug(config: &Config, open_time_ms: i64) -> String {
        match config.polymarket_slug_format {
            PolymarketSlugFormat::Timestamp => {
                Self::build_slug(&config.polymarket_slug_prefix, open_time_ms)
            }
            PolymarketSlugFormat::HourlyEt => {
                Self::build_hourly_et_slug(&config.polymarket_slug_asset, open_time_ms)
                    .unwrap_or_else(|| {
                        Self::build_slug(&config.polymarket_slug_prefix, open_time_ms)
                    })
            }
        }
    }

    pub fn build_hourly_et_slug(asset: &str, open_time_ms: i64) -> Option<String> {
        let utc = DateTime::<Utc>::from_timestamp_millis(open_time_ms)?;
        let et = utc.with_timezone(&New_York);
        let month = match et.month() {
            1 => "january",
            2 => "february",
            3 => "march",
            4 => "april",
            5 => "may",
            6 => "june",
            7 => "july",
            8 => "august",
            9 => "september",
            10 => "october",
            11 => "november",
            12 => "december",
            _ => return None,
        };
        let hour_12 = match et.hour() % 12 {
            0 => 12,
            h => h,
        };
        let am_pm = if et.hour() < 12 { "am" } else { "pm" };
        Some(format!(
            "{}-up-or-down-{}-{}-{}-{}{}-et",
            asset.trim().to_ascii_lowercase().replace(' ', "-"),
            month,
            et.day(),
            et.year(),
            hour_12,
            am_pm
        ))
    }

    pub async fn resolve_market(&self, slug: &str) -> Result<MarketInfo> {
        use std::time::Instant;

        {
            let cache = self.market_cache.lock().await;
            if let Some((cached_slug, info)) = cache.as_ref() {
                if cached_slug == slug {
                    return Ok(info.clone());
                }
            }
        }

        let t_resolve = Instant::now();
        let url = format!("{}/markets?slug={}", self.gamma_api_base, slug);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Gamma API GET échoué: {}", e))?;
        let gamma_http_ms = t_resolve.elapsed().as_millis();

        if !resp.status().is_success() {
            return Err(anyhow!("Gamma API {} → HTTP {}", url, resp.status()));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("Gamma API lecture body: {}", e))?;

        let info = parse_gamma_market_body(slug, &body)?;

        debug!(
            "Marché résolu: slug={} condition_id={} UP={} DOWN={} | timing: gamma_http={}ms total={}ms",
            slug, info.condition_id, info.up_token_id, info.down_token_id,
            gamma_http_ms, t_resolve.elapsed().as_millis()
        );
        *self.market_cache.lock().await = Some((slug.to_string(), info.clone()));
        Ok(info)
    }

    /// Pré-chauffe les caches SDK (tick_size, fee_rate_bps, neg_risk) pour les tokens
    /// d'un marché résolu. À appeler après resolve_market pour que build() soit instantané.
    pub async fn warm_sdk_caches(&self, market: &MarketInfo) {
        let client = match self.get_or_create_sdk_client().await {
            Ok(c) => c,
            Err(_) => return,
        };
        // Pré-fetch tick_size + fee_rate_bps + neg_risk pour les deux tokens (UP et DOWN).
        // Les erreurs sont ignorées — ce n'est qu'un warm-up.
        for token_id in [&market.up_token_id, &market.down_token_id] {
            if let Ok(tid) = U256::from_str_radix(token_id.as_str(), 10) {
                let _ = client.tick_size(tid).await;
                let _ = client.neg_risk(tid).await;
            }
        }
    }

    /// Ping keep-alive vers le CLOB pour garder la connexion TCP/TLS chaude.
    /// À lancer dans un tokio::spawn.
    pub async fn run_keep_alive_loop(&self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(20));
        loop {
            ticker.tick().await;
            let _ = self
                .http
                .get(format!("{}/ok", self.clob_api_base))
                .send()
                .await;
        }
    }

    /// Place un ordre sur Polymarket selon le signal reçu.
    ///
    /// - `DryRun` : simule sans appel réseau (aucune clé requise).
    /// - `Market` : ordre FAK signé EIP-712 + headers HMAC-SHA256 L2.
    /// - `Limit`  : non implémenté.
    pub async fn place_order(
        &self,
        signal: &Signal,
        market: &MarketInfo,
        amount_usdc: f64,
    ) -> Result<OrderResult> {
        let token_id_str = match &signal.prediction {
            Prediction::Up => &market.up_token_id,
            Prediction::Down => &market.down_token_id,
        };

        let submitted_at = Utc::now();

        match self.config.execution_mode {
            ExecutionMode::DryRun => {
                info!(
                    "[DRY-RUN] Ordre simulé | type=FAK token={} amount={:.2} USDC",
                    token_id_str, amount_usdc
                );
                Ok(OrderResult {
                    order_id: format!("dry-run-{}", Uuid::new_v4()),
                    status: "DRY_RUN".to_string(),
                    amount_usdc,
                    limit_price: None,
                    execution_price: None,
                    execution_price_source: None,
                    size_matched: None,
                    submitted_at,
                    ack_at: Utc::now(),
                })
            }

            ExecutionMode::Market => {
                self.submit_market_order_with_retry(token_id_str, submitted_at, amount_usdc)
                    .await
            }

            ExecutionMode::Limit => {
                self.submit_limit_order_with_retry(
                    token_id_str,
                    submitted_at,
                    amount_usdc,
                    market.order_min_size,
                )
                .await
            }
        }
    }

    /// Récupère le statut courant d'un ordre via `GET /orders/{order_id}`.
    /// Requiert le signer (mode Market uniquement — les ordres dry-run ne sont pas tracés).
    pub async fn get_order_status(&self, order_id: &str) -> Result<String> {
        Ok(self.get_order_execution_details(order_id).await?.status)
    }

    pub async fn get_order_execution_details(
        &self,
        order_id: &str,
    ) -> Result<OrderExecutionDetails> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| anyhow!("get_order_status requiert POLYMARKET_PRIVATE_KEY"))?
            .clone();

        let creds = self.get_or_derive_creds(&signer).await?;
        let timestamp = Utc::now().timestamp().to_string();
        let path = format!("/data/order/{}", order_id);
        let sig = Self::compute_hmac_sig(&creds.secret, &timestamp, "GET", &path, "")?;

        let resp = self
            .http
            .get(format!("{}{}", self.clob_api_base, path))
            .header("POLY_ADDRESS", &creds.address)
            .header("POLY_API_KEY", &creds.api_key)
            .header("POLY_PASSPHRASE", &creds.passphrase)
            .header("POLY_SIGNATURE", &sig)
            .header("POLY_TIMESTAMP", &timestamp)
            .send()
            .await
            .map_err(|e| anyhow!("GET /data/order/{}: {}", order_id, e))?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "GET /data/order/{} → HTTP {}",
                order_id,
                resp.status()
            ));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("lecture order status body: {}", e))?;

        parse_order_execution_details_body(&body)
    }

    // ── Order book ────────────────────────────────────────────────────────────

    /// Retourne le prix de reference limite depuis le CLOB public.
    /// Retourne None si le book est vide ou si l'appel échoue.
    async fn get_limit_reference_price(&self, token_id_str: &str) -> Option<f64> {
        let url = format!("{}/book?token_id={}", self.clob_api_base, token_id_str);
        if let Ok(resp) = self.http.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.text().await {
                    if let Some(reference_price) =
                        parse_book_reference_price_body(&body, self.config.limit_price_reference)
                    {
                        return Some(reference_price);
                    }
                }
            }
        }

        warn!(
            "[LIMIT] /book indisponible ou vide pour token={} reference={} - tentative WebSocket snapshot",
            token_id_str,
            self.config.limit_price_reference.as_str()
        );
        self.get_reference_price_ws_snapshot(
            token_id_str,
            Duration::from_millis(1500),
            self.config.limit_price_reference,
        )
        .await
    }

    pub async fn get_best_ask_ws_snapshot(
        &self,
        token_id_str: &str,
        timeout_duration: Duration,
    ) -> Option<f64> {
        self.get_reference_price_ws_snapshot(
            token_id_str,
            timeout_duration,
            LimitPriceReference::BestAsk,
        )
        .await
    }

    pub async fn get_reference_price_ws_snapshot(
        &self,
        token_id_str: &str,
        timeout_duration: Duration,
        reference: LimitPriceReference,
    ) -> Option<f64> {
        let subscription = serde_json::json!({
            "assets_ids": [token_id_str],
            "type": "market",
            "custom_feature_enabled": true
        })
        .to_string();

        let fut = async {
            let (mut ws, _) = connect_async(MARKET_WS_URL).await.ok()?;
            ws.send(Message::Text(subscription)).await.ok()?;
            while let Some(message) = ws.next().await {
                match message.ok()? {
                    Message::Text(text) => {
                        if let Some(reference_price) =
                            parse_market_ws_reference_price_message(token_id_str, &text, reference)
                        {
                            return Some(reference_price);
                        }
                    }
                    Message::Binary(bytes) => {
                        let text = String::from_utf8(bytes).ok()?;
                        if let Some(reference_price) =
                            parse_market_ws_reference_price_message(token_id_str, &text, reference)
                        {
                            return Some(reference_price);
                        }
                    }
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload)).await.ok()?;
                    }
                    Message::Close(_) => return None,
                    _ => {}
                }
            }
            None
        };

        tokio::time::timeout(timeout_duration, fut)
            .await
            .ok()
            .flatten()
    }

    async fn get_available_shares_up_to_price(
        &self,
        token_id_str: &str,
        limit_price: f64,
    ) -> Option<f64> {
        let url = format!("{}/book?token_id={}", self.clob_api_base, token_id_str);
        let body = self.http.get(&url).send().await.ok()?.text().await.ok()?;
        calculate_available_shares_up_to_price(&body, limit_price)
    }

    /// Ordre limite GTC au prix `LIMIT_PRICE_REFERENCE + LIMIT_PRICE_OFFSET`.
    async fn submit_limit_order(
        &self,
        token_id_str: &str,
        submitted_at: DateTime<Utc>,
        amount_usdc: f64,
        min_size: f64,
    ) -> Result<OrderResult> {
        use std::time::Instant;

        let sdk_signer = self
            .sdk_signer
            .as_ref()
            .ok_or_else(|| anyhow!("POLYMARKET_PRIVATE_KEY requis pour le mode Limit"))?;

        let t_book = Instant::now();
        let reference = self.config.limit_price_reference;
        let reference_price = self.get_limit_reference_price(token_id_str).await;
        let book_ms = t_book.elapsed().as_millis();
        let quote = calculate_limit_order_quote(
            amount_usdc,
            min_size,
            reference_price,
            self.config.limit_price_offset,
            self.config.limit_price_high_guard,
        );

        let limit_price = match reference_price {
            Some(price) => {
                let p = quote.limit_price;
                info!(
                    "[LIMIT] reference={} price={:.4} offset={:.4} -> limit_price={:.4} (book={}ms)",
                    reference.as_str(),
                    price,
                    self.config.limit_price_offset,
                    p,
                    book_ms
                );
                p
            }
            None => {
                let fallback = quote.limit_price;
                warn!(
                    "[LIMIT] reference={} indisponible - fallback price={:.4}",
                    reference.as_str(),
                    fallback
                );
                fallback
            }
        };

        // Vérifier que le montant couvre le minimum de shares (par défaut 5 sur Polymarket).
        // shares = USDC / prix → si insuffisant, on monte au minimum requis.
        if quote.high_guard_applied {
            warn!(
                "[LIMIT] high guard applique | calculated_price={:.4} threshold={:.4} forced_price={:.4}",
                quote.uncapped_limit_price,
                self.config.limit_price_high_guard.threshold,
                quote.limit_price
            );
        }

        let expected_shares = quote.expected_shares;
        let effective_usdc = if quote.adjusted_to_min_size {
            let min_usdc = quote.effective_usdc;
            warn!(
                "[LIMIT] {:.2} USDC → {:.2} shares < minimum {:.0} shares. Ajustement à {:.2} USDC",
                amount_usdc, expected_shares, min_size, min_usdc
            );
            min_usdc
        } else {
            amount_usdc
        };

        let t0 = Instant::now();
        let client = self.get_or_create_sdk_client().await?;
        let sdk_client_ms = t0.elapsed().as_millis();

        let truncated_usdc = (effective_usdc * 100.0).floor() / 100.0;
        let available_usdc = self.get_usdc_balance().await?;
        validate_sufficient_usdc_balance(truncated_usdc, available_usdc)?;

        let size_shares = truncated_usdc / limit_price;
        let size_decimal = Decimal::from_str(&format!("{:.2}", size_shares))
            .map_err(|e| anyhow!("taille Decimal invalide: {}", e))?;
        let price_decimal = Decimal::from_str(&format!("{:.2}", limit_price))
            .map_err(|e| anyhow!("prix limite Decimal invalide: {}", e))?;

        if let Some(available_shares) = self
            .get_available_shares_up_to_price(token_id_str, limit_price)
            .await
        {
            if available_shares + 0.000_001 < quote.expected_shares.max(min_size) {
                warn!(
                    "[LIMIT] profondeur immediate faible | token={} available_shares={:.2} required_shares={:.2} limit_price={:.4}",
                    token_id_str,
                    available_shares,
                    quote.expected_shares.max(min_size),
                    limit_price
                );
            }
        }

        let token_id_u256 = U256::from_str_radix(token_id_str, 10)
            .map_err(|e| anyhow!("token_id parse U256: {}", e))?;
        let t1 = Instant::now();
        let order = client
            .limit_order()
            .token_id(token_id_u256)
            .side(SdkSide::Buy)
            .price(price_decimal)
            .size(size_decimal)
            .order_type(SdkOrderType::GTC)
            .build()
            .await
            .map_err(|e| anyhow!("SDK build limit_order: {}", e))?;
        let build_ms = t1.elapsed().as_millis();

        let t2 = Instant::now();
        let signed_order = client
            .sign(sdk_signer, order)
            .await
            .map_err(|e| anyhow!("SDK sign order: {}", e))?;
        let sign_ms = t2.elapsed().as_millis();

        let t3 = Instant::now();
        let resp = client
            .post_order(signed_order)
            .await
            .map_err(|e| anyhow!("SDK post_order: {}", e))?;
        let post_ms = t3.elapsed().as_millis();
        let ack_at = Utc::now();
        let order_id = format!("{:?}", resp.order_id).trim_matches('"').to_string();
        let status = format!("{:?}", resp.status).trim_matches('"').to_string();
        let execution_details = self.get_order_execution_details(&order_id).await.ok();
        let execution_price = execution_details
            .as_ref()
            .and_then(OrderExecutionDetails::execution_price_with_source);

        info!(
            "Ordre GTC envoye | token={} amount={:.2}USDC limit_price={:.4} executed_price={} executed_price_source={} size_matched={} | book={}ms sdk={}ms build={}ms sign={}ms post={}ms",
            token_id_str, truncated_usdc, limit_price,
            execution_price
                .map(|(price, _)| format!("{:.4}", price))
                .unwrap_or_else(|| "unknown".to_string()),
            execution_price
                .map(|(_, source)| source)
                .unwrap_or("unavailable"),
            execution_details
                .as_ref()
                .and_then(|details| details.size_matched)
                .map(|size| format!("{:.4}", size))
                .unwrap_or_else(|| "unknown".to_string()),
            book_ms, sdk_client_ms, build_ms, sign_ms, post_ms
        );

        Ok(OrderResult {
            order_id,
            status,
            amount_usdc: effective_usdc,
            limit_price: Some(limit_price),
            execution_price: execution_price.map(|(price, _)| price),
            execution_price_source: execution_price.map(|(_, source)| source.to_string()),
            size_matched: execution_details.and_then(|details| details.size_matched),
            submitted_at,
            ack_at,
        })
    }

    async fn submit_limit_order_with_retry(
        &self,
        token_id_str: &str,
        submitted_at: DateTime<Utc>,
        amount_usdc: f64,
        min_size: f64,
    ) -> Result<OrderResult> {
        let mut temporary_attempt = 0usize;

        loop {
            match self
                .submit_limit_order(token_id_str, submitted_at, amount_usdc, min_size)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e)
                    if Self::is_clob_temporary_order_error(&e)
                        && temporary_attempt < CLOB_TEMPORARY_RETRY_DELAYS_SECS.len() =>
                {
                    let delay_secs = Self::temporary_order_retry_delay_secs(&e, temporary_attempt);
                    warn!(
                        "CLOB temporairement indisponible pour ordre GTC token={} — retry {}/{} dans {}s: {}",
                        token_id_str,
                        temporary_attempt + 1,
                        CLOB_TEMPORARY_RETRY_DELAYS_SECS.len(),
                        delay_secs,
                        e
                    );
                    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    temporary_attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ── Helpers privés ────────────────────────────────────────────────────────

    /// Retourne les credentials API, les dérivant via L1 si pas encore en cache.
    async fn get_or_derive_creds(&self, signer: &PrivateKeySigner) -> Result<ApiCreds> {
        let mut guard = self.api_creds.lock().await;
        if let Some(creds) = guard.as_ref() {
            return Ok(creds.clone());
        }
        let creds = Self::derive_api_creds(&self.http, &self.clob_api_base, signer).await?;
        *guard = Some(creds.clone());
        Ok(creds)
    }

    /// Auth L1 : signe le message ClobAuth (EIP-712) et appelle POST /auth/api-key.
    async fn derive_api_creds(
        http: &reqwest::Client,
        clob_api_base: &str,
        signer: &PrivateKeySigner,
    ) -> Result<ApiCreds> {
        let timestamp = Utc::now().timestamp().to_string();
        let address = signer.address();
        let address_str = format!("{}", address);

        let signing_hash = Self::clob_auth_signing_hash(address, &timestamp, 0)?;
        let sig = signer
            .sign_hash(&signing_hash)
            .await
            .map_err(|e| anyhow!("ClobAuth signing: {:?}", e))?;
        let sig_hex = Self::sig_to_hex(&sig);

        // Essaye POST (créer), si 4xx essaye GET (récupérer une clé existante)
        let resp = http
            .post(format!("{}/auth/api-key", clob_api_base))
            .header("POLY_ADDRESS", &address_str)
            .header("POLY_SIGNATURE", &sig_hex)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_NONCE", "0")
            .send()
            .await
            .map_err(|e| anyhow!("POST /auth/api-key: {}", e))?;

        let key_resp: ApiKeyResponse = if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| anyhow!("parse api-key response: {}", e))?
        } else {
            // Clé déjà existante — la récupérer avec GET
            let resp2 = http
                .get(format!("{}/auth/derive-api-key", clob_api_base))
                .header("POLY_ADDRESS", &address_str)
                .header("POLY_SIGNATURE", &sig_hex)
                .header("POLY_TIMESTAMP", &timestamp)
                .header("POLY_NONCE", "0")
                .send()
                .await
                .map_err(|e| anyhow!("GET /auth/derive-api-key: {}", e))?;

            if !resp2.status().is_success() {
                return Err(anyhow!(
                    "Impossible de dériver les credentials Polymarket: HTTP {}",
                    resp2.status()
                ));
            }
            resp2
                .json()
                .await
                .map_err(|e| anyhow!("parse derive-api-key response: {}", e))?
        };

        info!("Credentials Polymarket CLOB dérivés pour {}", address_str);
        Ok(ApiCreds {
            api_key: key_resp.api_key,
            secret: key_resp.secret,
            passphrase: key_resp.passphrase,
            address: address_str,
        })
    }

    // ── EIP-712 ───────────────────────────────────────────────────────────────

    /// Domain separator du contrat CTFExchange (Polygon mainnet, chain_id=137).
    pub fn ctf_domain_separator() -> Result<[u8; 32]> {
        let domain_typehash = keccak256(
            b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let name_hash = keccak256(b"Polymarket CTF Exchange");
        let version_hash = keccak256(b"2");
        let contract = Address::from_str(CTF_EXCHANGE_ADDR)
            .map_err(|_| anyhow!("adresse CTFExchange invalide"))?;

        let mut buf = [0u8; 5 * 32];
        buf[0..32].copy_from_slice(domain_typehash.as_slice());
        buf[32..64].copy_from_slice(name_hash.as_slice());
        buf[64..96].copy_from_slice(version_hash.as_slice());
        buf[96..128].copy_from_slice(&U256::from(POLYGON_CHAIN_ID).to_be_bytes::<32>());
        let mut addr_pad = [0u8; 32];
        addr_pad[12..].copy_from_slice(contract.as_slice());
        buf[128..160].copy_from_slice(&addr_pad);

        Ok(*keccak256(buf))
    }

    /// Domain separator de ClobAuthDomain (pas de verifyingContract).
    pub fn clob_auth_domain_separator() -> [u8; 32] {
        let domain_typehash =
            keccak256(b"EIP712Domain(string name,string version,uint256 chainId)");
        let name_hash = keccak256(b"ClobAuthDomain");
        let version_hash = keccak256(b"1");

        let mut buf = [0u8; 4 * 32];
        buf[0..32].copy_from_slice(domain_typehash.as_slice());
        buf[32..64].copy_from_slice(name_hash.as_slice());
        buf[64..96].copy_from_slice(version_hash.as_slice());
        buf[96..128].copy_from_slice(&U256::from(POLYGON_CHAIN_ID).to_be_bytes::<32>());

        *keccak256(buf)
    }

    /// Hash EIP-712 complet d'un ordre CTFExchange à signer.
    #[expect(
        clippy::too_many_arguments,
        reason = "EIP-712 Order hash mirrors the external contract field list"
    )]
    pub fn order_signing_hash(
        salt: U256,
        maker: Address,
        token_id: U256,
        maker_amount: U256,
        taker_amount: U256,
        fee_rate_bps: U256,
        side: u8,
        signature_type: u8,
    ) -> Result<B256> {
        let order_typehash = keccak256(
            b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,\
              uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,\
              uint256 feeRateBps,uint8 side,uint8 signatureType)",
        );

        let mut maker_pad = [0u8; 32];
        maker_pad[12..].copy_from_slice(maker.as_slice());

        // 13 champs × 32 octets = 416 octets
        let mut buf = [0u8; 13 * 32];
        buf[0..32].copy_from_slice(order_typehash.as_slice());
        buf[32..64].copy_from_slice(&salt.to_be_bytes::<32>());
        buf[64..96].copy_from_slice(&maker_pad); // maker
        buf[96..128].copy_from_slice(&maker_pad); // signer = maker (EOA)
                                                  // taker = Address::ZERO (buf[128..160] déjà à zéro)
        buf[160..192].copy_from_slice(&token_id.to_be_bytes::<32>());
        buf[192..224].copy_from_slice(&maker_amount.to_be_bytes::<32>());
        buf[224..256].copy_from_slice(&taker_amount.to_be_bytes::<32>());
        // expiration = 0 (buf[256..288] zéro)
        // nonce = 0 (buf[288..320] zéro)
        buf[320..352].copy_from_slice(&fee_rate_bps.to_be_bytes::<32>());
        buf[383] = side; // side uint8 dans le slot [352..384], octet LSB
        buf[415] = signature_type; // signatureType uint8 dans le slot [384..416], octet LSB

        let struct_hash = keccak256(buf);
        let domain_sep = Self::ctf_domain_separator()?;

        // "\x19\x01" || domainSeparator || structHash
        let mut final_buf = [0u8; 66];
        final_buf[0] = 0x19;
        final_buf[1] = 0x01;
        final_buf[2..34].copy_from_slice(&domain_sep);
        final_buf[34..66].copy_from_slice(struct_hash.as_slice());

        Ok(keccak256(final_buf))
    }

    /// Hash EIP-712 du message ClobAuth à signer pour l'auth L1.
    pub fn clob_auth_signing_hash(address: Address, timestamp: &str, nonce: u64) -> Result<B256> {
        let typehash =
            keccak256(b"ClobAuth(address address,string timestamp,uint256 nonce,string message)");

        let mut addr_pad = [0u8; 32];
        addr_pad[12..].copy_from_slice(address.as_slice());
        let ts_hash = keccak256(timestamp.as_bytes());
        let msg_hash = keccak256(CLOB_AUTH_MSG.as_bytes());

        // 5 champs × 32 octets
        let mut buf = [0u8; 5 * 32];
        buf[0..32].copy_from_slice(typehash.as_slice());
        buf[32..64].copy_from_slice(&addr_pad);
        buf[64..96].copy_from_slice(ts_hash.as_slice());
        buf[96..128].copy_from_slice(&U256::from(nonce).to_be_bytes::<32>());
        buf[128..160].copy_from_slice(msg_hash.as_slice());

        let struct_hash = keccak256(buf);
        let domain_sep = Self::clob_auth_domain_separator();

        let mut final_buf = [0u8; 66];
        final_buf[0] = 0x19;
        final_buf[1] = 0x01;
        final_buf[2..34].copy_from_slice(&domain_sep);
        final_buf[34..66].copy_from_slice(struct_hash.as_slice());

        Ok(keccak256(final_buf))
    }

    /// Sérialise une signature alloy en "0x<r><s><v>" (65 octets, v = 27 ou 28).
    fn sig_to_hex(sig: &alloy::primitives::Signature) -> String {
        let r = sig.r();
        let s = sig.s();
        let v = 27u8 + u8::from(sig.v());
        let mut bytes = [0u8; 65];
        bytes[..32].copy_from_slice(&r.to_be_bytes::<32>());
        bytes[32..64].copy_from_slice(&s.to_be_bytes::<32>());
        bytes[64] = v;
        format!(
            "0x{}",
            bytes
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        )
    }

    /// Calcule la signature HMAC-SHA256 pour les headers L2.
    /// message = timestamp + method + path + body (apostrophes → guillemets)
    pub fn compute_hmac_sig(
        secret: &str,
        timestamp: &str,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<String> {
        let secret_bytes = URL_SAFE
            .decode(secret)
            .map_err(|e| anyhow!("HMAC secret decode: {}", e))?;

        let body_normalized = body.replace('\'', "\"");
        let message = format!("{}{}{}{}", timestamp, method, path, body_normalized);

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_bytes)
            .map_err(|e| anyhow!("HMAC key: {}", e))?;
        mac.update(message.as_bytes());
        let result = mac.finalize().into_bytes();

        Ok(URL_SAFE.encode(result))
    }

    /// Retourne le client SDK authentifié, le créant au premier appel puis le réutilisant.
    /// Élimine le coût de authenticate() + dérivation API key à chaque ordre (~400ms).
    /// Les caches internes du SDK (tick_size, fee_rate_bps) sont aussi conservés.
    async fn get_or_create_sdk_client(&self) -> Result<SdkClobClient<Authenticated<Normal>>> {
        let mut guard = self.sdk_client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }

        let sdk_signer = self
            .sdk_signer
            .as_ref()
            .ok_or_else(|| anyhow!("POLYMARKET_PRIVATE_KEY requis pour le mode Market"))?;

        let auth_builder = SdkClobClient::new(&self.clob_api_base, SdkConfig::default())
            .map_err(|e| anyhow!("SDK client init: {}", e))?
            .authentication_builder(sdk_signer);

        let client = if let Some(funder) = self.config.polymarket_funder.as_deref() {
            let funder = Address::from_str(funder)
                .map_err(|e| anyhow!("POLYMARKET_FUNDER invalide: {}", e))?;
            let signature_type = match self.config.polymarket_signature_type.unwrap_or(2) {
                0 => SdkSignatureType::Eoa,
                1 => SdkSignatureType::Proxy,
                2 => SdkSignatureType::GnosisSafe,
                3 => SdkSignatureType::Poly1271,
                other => {
                    return Err(anyhow!(
                        "POLYMARKET_SIGNATURE_TYPE={} invalide (attendu 0, 1, 2 ou 3)",
                        other
                    ));
                }
            };
            auth_builder
                .funder(funder)
                .signature_type(signature_type)
                .authenticate()
                .await
                .map_err(|e| anyhow!("SDK authenticate avec funder: {}", e))?
        } else {
            auth_builder
                .authenticate()
                .await
                .map_err(|e| anyhow!("SDK authenticate: {}", e))?
        };

        info!("Client SDK Polymarket authentifié et mis en cache");
        *guard = Some(client.clone());
        Ok(client)
    }

    async fn submit_market_order(
        &self,
        token_id_str: &str,
        submitted_at: DateTime<Utc>,
        amount_usdc: f64,
    ) -> Result<OrderResult> {
        use std::time::Instant;

        let sdk_signer = self
            .sdk_signer
            .as_ref()
            .ok_or_else(|| anyhow!("POLYMARKET_PRIVATE_KEY requis pour le mode Market"))?;

        let t0 = Instant::now();
        let client = self.get_or_create_sdk_client().await?;
        let sdk_client_ms = t0.elapsed().as_millis();

        // Polymarket exige max 2 décimales pour le maker amount (USDC)
        let truncated_usdc = (amount_usdc * 100.0).floor() / 100.0;
        let amount = Decimal::from_str(&format!("{:.2}", truncated_usdc))
            .map_err(|e| anyhow!("montant Decimal invalide: {}", e))?;

        let available_usdc = self.get_usdc_balance().await?;
        validate_sufficient_usdc_balance(truncated_usdc, available_usdc)?;

        // Prix plafond 0.99 : le CLOB matche au meilleur ask disponible.
        // Évite le fetch de l'order book (~200-250ms) à chaque ordre.
        let max_price =
            Decimal::from_str("0.99").map_err(|e| anyhow!("prix max Decimal invalide: {}", e))?;

        let token_id_u256 = U256::from_str_radix(token_id_str, 10)
            .map_err(|e| anyhow!("token_id parse U256: {}", e))?;
        let order_type = match self.config.market_order_type {
            MarketOrderType::Fok => SdkOrderType::FOK,
            MarketOrderType::Fak => SdkOrderType::FAK,
        };

        let t1 = Instant::now();
        let order = client
            .market_order()
            .token_id(token_id_u256)
            .amount(Amount::usdc(amount).map_err(|e| anyhow!("Amount::usdc: {}", e))?)
            .price(max_price)
            .side(SdkSide::Buy)
            .order_type(order_type)
            .build()
            .await
            .map_err(|e| anyhow!("SDK build market_order: {}", e))?;
        let build_ms = t1.elapsed().as_millis();

        let t2 = Instant::now();
        let signed_order = client
            .sign(sdk_signer, order)
            .await
            .map_err(|e| anyhow!("SDK sign order: {}", e))?;
        let sign_ms = t2.elapsed().as_millis();

        let t3 = Instant::now();
        let resp = client
            .post_order(signed_order)
            .await
            .map_err(|e| anyhow!("SDK post_order: {}", e))?;
        let post_ms = t3.elapsed().as_millis();
        let ack_at = Utc::now();

        info!(
            "Ordre FOK envoyé via SDK | token={} amount={:.2}USDC | timing: sdk_client={}ms build={}ms sign={}ms post={}ms total={}ms",
            token_id_str, amount_usdc,
            sdk_client_ms, build_ms, sign_ms, post_ms, t0.elapsed().as_millis()
        );

        Ok(OrderResult {
            order_id: format!("{:?}", resp.order_id).trim_matches('"').to_string(),
            status: format!("{:?}", resp.status).trim_matches('"').to_string(),
            amount_usdc,
            limit_price: Some(0.99),
            execution_price: None,
            execution_price_source: None,
            size_matched: None,
            submitted_at,
            ack_at,
        })
    }

    async fn submit_market_order_with_retry(
        &self,
        token_id_str: &str,
        submitted_at: DateTime<Utc>,
        amount_usdc: f64,
    ) -> Result<OrderResult> {
        let mut fok_attempt = 0usize;
        let mut temporary_attempt = 0usize;

        loop {
            match self
                .submit_market_order(token_id_str, submitted_at, amount_usdc)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e)
                    if Self::is_clob_temporary_order_error(&e)
                        && temporary_attempt < CLOB_TEMPORARY_RETRY_DELAYS_SECS.len() =>
                {
                    let delay_secs = Self::temporary_order_retry_delay_secs(&e, temporary_attempt);
                    warn!(
                        "CLOB temporairement indisponible pour ordre FOK token={} - retry {}/{} dans {}s: {}",
                        token_id_str,
                        temporary_attempt + 1,
                        CLOB_TEMPORARY_RETRY_DELAYS_SECS.len(),
                        delay_secs,
                        e
                    );
                    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    temporary_attempt += 1;
                }
                Err(e)
                    if Self::is_fok_unfilled_error(&e)
                        && fok_attempt < FOK_RETRY_DELAYS_SECS.len() =>
                {
                    let delay_secs = FOK_RETRY_DELAYS_SECS[fok_attempt];
                    warn!(
                        "Ordre FOK non rempli immédiatement pour token={} — retry {}/{} dans {}s",
                        token_id_str,
                        fok_attempt + 1,
                        FOK_RETRY_DELAYS_SECS.len(),
                        delay_secs
                    );
                    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    fok_attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn is_fok_unfilled_error(err: &anyhow::Error) -> bool {
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("fok orders are fully filled or killed")
            || msg.contains("order couldn't be fully filled")
    }

    pub(crate) fn is_clob_temporary_order_error(err: &anyhow::Error) -> bool {
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("425")
            || msg.contains("too early")
            || msg.contains("post-only")
            || msg.contains("post only")
            || msg.contains("post_only")
            || msg.contains("retry-after")
            || msg.contains("temporarily unavailable")
            || msg.contains("service unavailable")
            || msg.contains("engine restart")
            || msg.contains("matching engine")
    }

    pub(crate) fn temporary_order_retry_delay_secs(err: &anyhow::Error, attempt: usize) -> u64 {
        let fallback = CLOB_TEMPORARY_RETRY_DELAYS_SECS
            .get(attempt)
            .copied()
            .unwrap_or_else(|| *CLOB_TEMPORARY_RETRY_DELAYS_SECS.last().unwrap_or(&30));

        Self::parse_retry_after_secs(&err.to_string())
            .map(|secs| secs.clamp(1, MAX_RETRY_AFTER_SECS))
            .unwrap_or(fallback)
    }

    pub(crate) fn parse_retry_after_secs(message: &str) -> Option<u64> {
        let lower = message.to_ascii_lowercase();
        let (_, tail) = lower.split_once("retry-after")?;
        let digits: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse::<u64>().ok()
    }

    pub async fn get_usdc_balance(&self) -> Result<f64> {
        let client = self.get_or_create_sdk_client().await?;
        let req = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Collateral)
            .build();
        let _ = client.update_balance_allowance(req.clone()).await;
        let resp = client
            .balance_allowance(req)
            .await
            .map_err(|e| anyhow!("get_usdc_balance échoué: {}", e))?;
        let raw: f64 = resp.balance.to_string().parse().unwrap_or(0.0);
        Ok((raw / 1_000_000.0 * 100.0).floor() / 100.0)
    }

    pub async fn get_open_orders(&self, token_id: Option<&str>) -> Result<Vec<OpenOrderSummary>> {
        let client = self.get_or_create_sdk_client().await?;
        let mut request = OrdersRequest::builder().build();
        if let Some(token_id) = token_id {
            request.asset_id = Some(
                U256::from_str_radix(token_id, 10)
                    .map_err(|e| anyhow!("token_id parse U256: {}", e))?,
            );
        }

        let page = client
            .orders(&request, None)
            .await
            .map_err(|e| anyhow!("get_open_orders echoue: {}", e))?;

        Ok(page
            .data
            .into_iter()
            .map(|order| OpenOrderSummary {
                id: order.id,
                status: format!("{:?}", order.status),
                asset_id: order.asset_id.to_string(),
                side: format!("{:?}", order.side),
                original_size: order.original_size.to_string(),
                size_matched: order.size_matched.to_string(),
                price: order.price.to_string(),
                outcome: order.outcome,
                order_type: format!("{:?}", order.order_type),
            })
            .collect())
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<CancelOrderSummary> {
        if order_id.trim().is_empty() {
            return Err(anyhow!("order_id requis pour cancel_order"));
        }

        let client = self.get_or_create_sdk_client().await?;
        let response = client
            .cancel_order(order_id)
            .await
            .map_err(|e| anyhow!("cancel_order echoue: {}", e))?;
        let mut not_canceled = response.not_canceled.into_iter().collect::<Vec<_>>();
        not_canceled.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(CancelOrderSummary {
            canceled: response.canceled,
            not_canceled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PolymarketClient;

    #[test]
    fn detects_temporary_clob_order_errors() {
        for message in [
            "SDK post_order: Status: error(425 Too Early)",
            "post_only_mode: matching engine restart",
            "service unavailable retry-after: 12",
        ] {
            let err = anyhow::anyhow!(message);
            assert!(PolymarketClient::is_clob_temporary_order_error(&err));
        }
    }

    #[test]
    fn retry_after_seconds_are_parsed_and_capped() {
        let err = anyhow::anyhow!("HTTP 425 retry-after: 120");

        assert_eq!(
            PolymarketClient::temporary_order_retry_delay_secs(&err, 0),
            60
        );
    }

    #[test]
    fn temporary_retry_uses_fallback_when_retry_after_is_absent() {
        let err = anyhow::anyhow!("HTTP 425 Too Early");

        assert_eq!(
            PolymarketClient::temporary_order_retry_delay_secs(&err, 1),
            15
        );
    }
}
