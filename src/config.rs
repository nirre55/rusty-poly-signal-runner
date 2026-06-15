use anyhow::Result;
use std::env;
use tracing::warn;

#[derive(Debug, Clone)]
pub enum ExecutionMode {
    DryRun,
    Market,
    Limit,
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionMode::DryRun => "DRY_RUN",
            ExecutionMode::Market => "MARKET",
            ExecutionMode::Limit => "LIMIT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketOrderType {
    Fok,
    Fak,
}

impl MarketOrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MarketOrderType::Fok => "FOK",
            MarketOrderType::Fak => "FAK",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitPriceReference {
    BestAsk,
    BestBid,
}

impl LimitPriceReference {
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitPriceReference::BestAsk => "best_ask",
            LimitPriceReference::BestBid => "best_bid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolymarketSlugFormat {
    Timestamp,
    HourlyEt,
}

impl PolymarketSlugFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolymarketSlugFormat::Timestamp => "timestamp",
            PolymarketSlugFormat::HourlyEt => "hourly_et",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitPriceHighGuard {
    pub enabled: bool,
    pub threshold: f64,
    pub price: f64,
}

// P13 : impl Debug manuel pour masquer les secrets dans les logs
#[derive(Clone)]
pub struct Config {
    pub binance_ws_url: String,
    pub symbol: String,
    pub interval: String,
    pub execution_mode: ExecutionMode,
    pub trade_amount_usdc: f64,
    #[allow(dead_code)]
    pub polymarket_api_key: String,
    #[allow(dead_code)]
    pub polymarket_api_secret: String,
    #[allow(dead_code)]
    pub polymarket_api_passphrase: String,
    #[allow(dead_code)]
    pub polymarket_api_url: String,
    pub logs_dir: String,
    /// Clé privée EVM (hex, avec ou sans "0x"). Requise pour ExecutionMode::Market.
    pub evm_private_key: Option<String>,
    /// Adresse funder Polymarket (proxy/safe/deposit wallet) si différente de l'EOA signataire.
    pub polymarket_funder: Option<String>,
    /// Signature type Polymarket: 0=EOA, 1=POLY_PROXY, 2=GNOSIS_SAFE, 3=POLY_1271.
    pub polymarket_signature_type: Option<u8>,
    /// Nom de la stratégie à utiliser (ex: "three_candle_rsi7_reversal").
    pub strategy: String,
    /// Seuil RSI haut (suracheté) — signal DOWN si RSI >= ce seuil. Défaut: 65.0
    pub rsi_overbought: f64,
    /// Seuil RSI bas (survendu) — signal UP si RSI <= ce seuil. Défaut: 35.0
    pub rsi_oversold: f64,
    /// Préfixe slug Polymarket (ex: "btc-updown-5m"). Format final: {prefix}-{timestamp}
    pub polymarket_slug_prefix: String,
    /// Format du slug Polymarket: timestamp ou hourly_et.
    pub polymarket_slug_format: PolymarketSlugFormat,
    /// Asset utilise pour les slugs hourly_et, ex: bitcoin, solana.
    pub polymarket_slug_asset: String,
    /// Multiplicateur Martingale après chaque loss. 1.0 = désactivé. Défaut: 1.0
    pub martingale_multiplier: f64,
    /// Montant maximum Martingale en USDC. 0.0 = pas de plafond. Défaut: 0.0
    pub martingale_max_amount: f64,
    /// Pourcentage du solde USDC à miser par trade. 0.0 = désactivé (utilise trade_amount_usdc).
    /// Mutuellement exclusif avec TRADE_AMOUNT_USDC. Minimum 1$ appliqué. Défaut: 0.0
    pub trade_amount_pct: f64,
    /// Jours de la semaine exclus du trading (ex: ["sat", "sun"]). Vide = aucun filtre.
    pub excluded_days: Vec<String>,
    /// Plages horaires UTC exclues du trading (ex: [(0, 9)]). Vide = aucun filtre.
    pub excluded_hours: Vec<(u32, u32)>,
    /// Nombre minimum de votes pour la stratégie ensemble. Défaut: 1
    pub ensemble_min_votes: u32,
    /// Prix de reference des ordres limite: best_ask ou best_bid. Defaut: best_ask
    pub limit_price_reference: LimitPriceReference,
    /// Offset signe ajoute au prix de reference (ex: 0.01, 0, -0.01). Defaut: 0.01
    pub limit_price_offset: f64,
    /// Prix limite fixe optionnel. Si defini, ignore reference, offset et high guard.
    pub limit_price_fixed: Option<f64>,
    /// Garde-fou optionnel: si le prix limite depasse threshold, force price.
    pub limit_price_high_guard: LimitPriceHighGuard,
    pub market_order_type: MarketOrderType,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("binance_ws_url", &self.binance_ws_url)
            .field("symbol", &self.symbol)
            .field("interval", &self.interval)
            .field("execution_mode", &self.execution_mode)
            .field("trade_amount_usdc", &self.trade_amount_usdc)
            .field("polymarket_api_key", &"[REDACTED]")
            .field("polymarket_api_secret", &"[REDACTED]")
            .field("polymarket_api_passphrase", &"[REDACTED]")
            .field("polymarket_api_url", &self.polymarket_api_url)
            .field("logs_dir", &self.logs_dir)
            .field("evm_private_key", &"[REDACTED]")
            .field("polymarket_funder", &self.polymarket_funder)
            .field("polymarket_signature_type", &self.polymarket_signature_type)
            .field("strategy", &self.strategy)
            .field("rsi_overbought", &self.rsi_overbought)
            .field("rsi_oversold", &self.rsi_oversold)
            .field("polymarket_slug_prefix", &self.polymarket_slug_prefix)
            .field("polymarket_slug_format", &self.polymarket_slug_format)
            .field("polymarket_slug_asset", &self.polymarket_slug_asset)
            .field("martingale_multiplier", &self.martingale_multiplier)
            .field("martingale_max_amount", &self.martingale_max_amount)
            .field("trade_amount_pct", &self.trade_amount_pct)
            .field("excluded_days", &self.excluded_days)
            .field("excluded_hours", &self.excluded_hours)
            .field("ensemble_min_votes", &self.ensemble_min_votes)
            .field("limit_price_reference", &self.limit_price_reference)
            .field("limit_price_offset", &self.limit_price_offset)
            .field("limit_price_fixed", &self.limit_price_fixed)
            .field("limit_price_high_guard", &self.limit_price_high_guard)
            .field("market_order_type", &self.market_order_type)
            .finish()
    }
}

fn default_slug_asset(symbol: &str) -> String {
    let symbol = symbol.trim().to_ascii_lowercase();
    match symbol.as_str() {
        "btcusdt" | "btc" => "bitcoin".to_string(),
        "ethusdt" | "eth" => "ethereum".to_string(),
        "solusdt" | "sol" => "solana".to_string(),
        other => other.strip_suffix("usdt").unwrap_or(other).to_string(),
    }
}

fn parse_bool_env(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "y" | "on"
            )
        })
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Config stratégie (priorité sur .env, mais pas sur les vars OS)
        if let Ok(path) = std::env::var("STRATEGY_CONFIG") {
            dotenvy::from_path(&path).ok();
        }
        // Secrets partagés (ne remplace pas ce qui est déjà défini)
        dotenvy::dotenv().ok();

        let mode = env::var("EXECUTION_MODE")
            .map_err(|_| anyhow::anyhow!("EXECUTION_MODE requis dans le fichier de config stratégie (market | limit | dry-run)"))?;
        let execution_mode = match mode.as_str() {
            "market" => ExecutionMode::Market,
            "limit" => ExecutionMode::Limit,
            "dry-run" | "dryrun" => ExecutionMode::DryRun,
            _ => anyhow::bail!(
                "EXECUTION_MODE '{}' non reconnu — valeurs acceptées: market, limit, dry-run",
                mode
            ),
        };

        let market_order_type = match env::var("MARKET_ORDER_TYPE")
            .unwrap_or_else(|_| "fok".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "fok" => MarketOrderType::Fok,
            "fak" => MarketOrderType::Fak,
            other => anyhow::bail!(
                "MARKET_ORDER_TYPE '{}' non reconnu - valeurs acceptees: fok, fak",
                other
            ),
        };

        let limit_price_reference = match env::var("LIMIT_PRICE_REFERENCE")
            .unwrap_or_else(|_| "best_ask".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "best_ask" | "ask" => LimitPriceReference::BestAsk,
            "best_bid" | "bid" => LimitPriceReference::BestBid,
            other => anyhow::bail!(
                "LIMIT_PRICE_REFERENCE '{}' non reconnu - valeurs acceptees: best_ask, best_bid",
                other
            ),
        };

        let trade_amount_pct = env::var("TRADE_AMOUNT_PCT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        if trade_amount_pct != 0.0 {
            if !(0.0 < trade_amount_pct && trade_amount_pct <= 100.0) {
                anyhow::bail!(
                    "TRADE_AMOUNT_PCT={} invalide — doit être entre 0 et 100 exclus",
                    trade_amount_pct
                );
            }
            if env::var("TRADE_AMOUNT_USDC").is_ok() {
                anyhow::bail!("TRADE_AMOUNT_PCT et TRADE_AMOUNT_USDC sont mutuellement exclusifs — n'utiliser qu'un seul");
            }
        }

        // P11 : valider que TRADE_AMOUNT_USDC est un nombre strictement positif
        let raw_amount = env::var("TRADE_AMOUNT_USDC").unwrap_or_else(|_| "10.0".to_string());
        let trade_amount_usdc = match raw_amount.parse::<f64>() {
            Ok(v) if v > 0.0 => v,
            Ok(v) => {
                warn!(
                    "TRADE_AMOUNT_USDC={} invalide (doit être > 0) — valeur par défaut 10.0 USDC utilisée",
                    v
                );
                10.0
            }
            Err(_) => {
                warn!(
                    "TRADE_AMOUNT_USDC='{}' non parseable — valeur par défaut 10.0 USDC utilisée",
                    raw_amount
                );
                10.0
            }
        };

        let polymarket_signature_type = match env::var("POLYMARKET_SIGNATURE_TYPE") {
            Ok(raw) => match raw.parse::<u8>() {
                Ok(v @ 0..=3) => Some(v),
                Ok(v) => {
                    warn!(
                        "POLYMARKET_SIGNATURE_TYPE={} invalide (attendu 0, 1, 2 ou 3) — valeur ignorée",
                        v
                    );
                    None
                }
                Err(_) => {
                    warn!(
                        "POLYMARKET_SIGNATURE_TYPE='{}' non parseable — valeur ignorée",
                        raw
                    );
                    None
                }
            },
            Err(_) => None,
        };

        let martingale_multiplier = env::var("MARTINGALE_MULTIPLIER")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1.0);

        let martingale_max_amount = env::var("MARTINGALE_MAX_AMOUNT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let rsi_overbought = env::var("RSI_OVERBOUGHT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(65.0);
        let rsi_oversold = env::var("RSI_OVERSOLD")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(35.0);

        let symbol = env::var("SYMBOL").unwrap_or_else(|_| "btcusdt".to_string());
        let polymarket_slug_format = match env::var("POLYMARKET_SLUG_FORMAT")
            .unwrap_or_else(|_| "timestamp".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "timestamp" | "unix" | "updown_timestamp" => PolymarketSlugFormat::Timestamp,
            "hourly_et" | "hourly" | "et_hourly" => PolymarketSlugFormat::HourlyEt,
            other => anyhow::bail!(
                "POLYMARKET_SLUG_FORMAT '{}' non reconnu - valeurs acceptees: timestamp, hourly_et",
                other
            ),
        };
        let polymarket_slug_asset =
            env::var("POLYMARKET_SLUG_ASSET").unwrap_or_else(|_| default_slug_asset(&symbol));

        let limit_price_high_guard = LimitPriceHighGuard {
            enabled: parse_bool_env("LIMIT_PRICE_HIGH_GUARD_ENABLED", false),
            threshold: env::var("LIMIT_PRICE_HIGH_GUARD_THRESHOLD")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.60),
            price: env::var("LIMIT_PRICE_HIGH_GUARD_PRICE")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.55),
        };
        let limit_price_fixed = env::var("LIMIT_PRICE_FIXED")
            .ok()
            .and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.parse::<f64>())
                }
            })
            .transpose()
            .map_err(|_| {
                anyhow::anyhow!("LIMIT_PRICE_FIXED doit etre un nombre entre 0.01 et 0.99")
            })?;

        let excluded_days = env::var("EXCLUDED_DAYS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let excluded_hours: Vec<(u32, u32)> = env::var("EXCLUDED_HOURS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|range| {
                let range = range.trim();
                if range.is_empty() {
                    return None;
                }
                let parts: Vec<&str> = range.splitn(2, '-').collect();
                if parts.len() != 2 {
                    return None;
                }
                let parse_h = |s: &str| s.trim().trim_end_matches('h').parse::<u32>().ok();
                let start = parse_h(parts[0])?;
                let end = parse_h(parts[1])?;
                if start >= end || end > 24 {
                    warn!("EXCLUDED_HOURS: plage invalide '{}' ignorée", range);
                    return None;
                }
                Some((start, end))
            })
            .collect();

        let config = Config {
            binance_ws_url: env::var("BINANCE_WS_URL")
                .unwrap_or_else(|_| "wss://stream.binance.com:9443/ws".to_string()),
            symbol,
            interval: env::var("INTERVAL").unwrap_or_else(|_| "5m".to_string()),
            execution_mode,
            trade_amount_usdc,
            polymarket_api_key: env::var("POLYMARKET_API_KEY").unwrap_or_default(),
            polymarket_api_secret: env::var("POLYMARKET_API_SECRET").unwrap_or_default(),
            polymarket_api_passphrase: env::var("POLYMARKET_API_PASSPHRASE")
                .or_else(|_| env::var("POLYMARKET_API_PASSPHRASE_V2"))
                .unwrap_or_default(),
            polymarket_api_url: env::var("POLYMARKET_API_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),
            logs_dir: env::var("LOGS_DIR").unwrap_or_else(|_| "logs".to_string()),
            evm_private_key: env::var("POLYMARKET_PRIVATE_KEY").ok(),
            polymarket_funder: env::var("POLYMARKET_FUNDER").ok(),
            polymarket_signature_type,
            strategy: env::var("STRATEGY")
                .unwrap_or_else(|_| "three_candle_rsi7_reversal".to_string()),
            rsi_overbought,
            rsi_oversold,
            polymarket_slug_prefix: env::var("POLYMARKET_SLUG_PREFIX")
                .unwrap_or_else(|_| "btc-updown-5m".to_string()),
            polymarket_slug_format,
            polymarket_slug_asset,
            martingale_multiplier,
            martingale_max_amount,
            trade_amount_pct,
            excluded_days,
            excluded_hours,
            ensemble_min_votes: env::var("ENSEMBLE_MIN_VOTES")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(1),
            limit_price_reference,
            limit_price_offset: env::var("LIMIT_PRICE_OFFSET")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.01),
            limit_price_fixed,
            limit_price_high_guard,
            market_order_type,
        };
        config.validate_for_startup()?;
        Ok(config)
    }

    pub fn validate_for_startup(&self) -> Result<()> {
        if let Some(price) = self.limit_price_fixed {
            if !(0.01..=0.99).contains(&price) {
                anyhow::bail!(
                    "LIMIT_PRICE_FIXED={} invalide - doit etre entre 0.01 et 0.99",
                    price
                );
            }
        }

        if self.limit_price_high_guard.enabled {
            let guard = self.limit_price_high_guard;
            if !(0.01..=0.99).contains(&guard.threshold) {
                anyhow::bail!(
                    "LIMIT_PRICE_HIGH_GUARD_THRESHOLD={} invalide - doit etre entre 0.01 et 0.99",
                    guard.threshold
                );
            }
            if !(0.01..=0.99).contains(&guard.price) {
                anyhow::bail!(
                    "LIMIT_PRICE_HIGH_GUARD_PRICE={} invalide - doit etre entre 0.01 et 0.99",
                    guard.price
                );
            }
            if guard.price >= guard.threshold {
                anyhow::bail!(
                    "LIMIT_PRICE_HIGH_GUARD_PRICE={} doit etre inferieur au seuil {}",
                    guard.price,
                    guard.threshold
                );
            }
        }

        if matches!(self.execution_mode, ExecutionMode::DryRun) {
            return Ok(());
        }

        let private_key = self
            .evm_private_key
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if private_key.is_empty() {
            anyhow::bail!(
                "POLYMARKET_PRIVATE_KEY est requis pour EXECUTION_MODE={}",
                self.execution_mode.as_str()
            );
        }

        let clob_url = self.polymarket_api_url.trim();
        if !(clob_url.starts_with("https://") || clob_url.starts_with("http://")) {
            anyhow::bail!("POLYMARKET_API_URL doit commencer par http:// ou https:// en mode reel");
        }

        if matches!(self.polymarket_signature_type, Some(1..=3))
            && self
                .polymarket_funder
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            anyhow::bail!(
                "POLYMARKET_FUNDER est requis quand POLYMARKET_SIGNATURE_TYPE vaut 1, 2 ou 3"
            );
        }

        let api_credential_fields = [
            self.polymarket_api_key.trim(),
            self.polymarket_api_secret.trim(),
            self.polymarket_api_passphrase.trim(),
        ];
        let provided_count = api_credential_fields
            .iter()
            .filter(|value| !value.is_empty())
            .count();
        if provided_count > 0 && provided_count < api_credential_fields.len() {
            anyhow::bail!(
                "POLYMARKET_API_KEY, POLYMARKET_API_SECRET et POLYMARKET_API_PASSPHRASE doivent etre fournis ensemble"
            );
        }

        Ok(())
    }
}
