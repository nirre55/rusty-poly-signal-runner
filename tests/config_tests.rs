use rusty_poly_signal_runner::config::{
    ExecutionMode, LimitPriceReference, MarketOrderType, PolymarketSlugFormat,
};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Mutex global pour sérialiser tous les tests qui touchent les variables d'environnement.
/// Les tests config s'exécutent en parallèle par défaut, ce qui cause des race conditions
/// sur les env vars partagées (EXECUTION_MODE, TRADE_AMOUNT_USDC…).
static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn test_config_parses_market_order_type_fak() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("MARKET_ORDER_TYPE", "fak");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.market_order_type, MarketOrderType::Fak);
    clear_config_env();
}

#[test]
fn test_config_rejects_unknown_market_order_type() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("MARKET_ORDER_TYPE", "ioc");
    let err = rusty_poly_signal_runner::config::Config::from_env().unwrap_err();
    assert!(err.to_string().contains("MARKET_ORDER_TYPE"));
    clear_config_env();
}

fn clear_config_env() {
    for key in [
        "EXECUTION_MODE",
        "TRADE_AMOUNT_USDC",
        "TRADE_AMOUNT_PCT",
        "EXCLUDED_DAYS",
        "EXCLUDED_HOURS",
        "LIMIT_PRICE_REFERENCE",
        "LIMIT_PRICE_OFFSET",
        "LIMIT_PRICE_HIGH_GUARD_ENABLED",
        "LIMIT_PRICE_HIGH_GUARD_THRESHOLD",
        "LIMIT_PRICE_HIGH_GUARD_PRICE",
        "MARKET_ORDER_TYPE",
        "SYMBOL",
        "INTERVAL",
        "POLYMARKET_API_KEY",
        "POLYMARKET_API_SECRET",
        "POLYMARKET_API_PASSPHRASE",
        "POLYMARKET_API_PASSPHRASE_V2",
        "POLYMARKET_API_URL",
        "POLYMARKET_SLUG_FORMAT",
        "POLYMARKET_SLUG_ASSET",
        "POLYMARKET_SLUG_PREFIX",
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER",
        "POLYMARKET_SIGNATURE_TYPE",
        "STRATEGY_CONFIG",
    ] {
        std::env::remove_var(key);
    }
}

fn without_repo_dotenv<T>(f: impl FnOnce() -> T) -> T {
    let original_dir = std::env::current_dir().unwrap();
    let temp_dir: PathBuf = std::env::temp_dir().join(format!(
        "rusty_poly_signal_runner_config_test_{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::env::set_current_dir(&temp_dir).unwrap();
    let result = f();
    std::env::set_current_dir(original_dir).unwrap();
    std::fs::remove_dir_all(temp_dir).ok();
    result
}

#[test]
fn test_execution_mode_as_str_dryrun() {
    assert_eq!(ExecutionMode::DryRun.as_str(), "DRY_RUN");
}

#[test]
fn test_execution_mode_as_str_market() {
    assert_eq!(ExecutionMode::Market.as_str(), "MARKET");
}

#[test]
fn test_execution_mode_as_str_limit() {
    assert_eq!(ExecutionMode::Limit.as_str(), "LIMIT");
}

/// Vérifie que les valeurs as_str() sont cohérentes avec les status d'ordre Polymarket
#[test]
fn test_execution_mode_as_str_uppercase_consistent() {
    for mode in [
        ExecutionMode::DryRun,
        ExecutionMode::Market,
        ExecutionMode::Limit,
    ] {
        let s = mode.as_str();
        assert_eq!(
            s,
            s.to_uppercase(),
            "as_str() doit être en MAJUSCULES: {}",
            s
        );
    }
}

/// Vérifie que Config::from_env() fonctionne avec les valeurs par défaut hors mode d'exécution.
#[test]
fn test_config_from_env_defaults() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "__default_test__");
    std::env::set_var("SYMBOL", "btcusdt");
    std::env::set_var("INTERVAL", "5m");

    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.symbol, "btcusdt");
    assert_eq!(config.interval, "5m");
    assert_eq!(config.trade_amount_usdc, 10.0); // "__default_test__" non parseable → default
    assert!(matches!(config.execution_mode, ExecutionMode::DryRun));
    std::env::remove_var("EXECUTION_MODE");
    std::env::remove_var("TRADE_AMOUNT_USDC");
    std::env::remove_var("TRADE_AMOUNT_PCT");
    clear_config_env();
}

/// Vérifie que TRADE_AMOUNT_USDC=0 est rejeté et remplacé par la valeur par défaut
#[test]
fn test_config_trade_amount_zero_uses_default() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "0");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.trade_amount_usdc, 10.0);
    std::env::remove_var("EXECUTION_MODE");
    std::env::remove_var("TRADE_AMOUNT_USDC");
    std::env::remove_var("TRADE_AMOUNT_PCT");
    clear_config_env();
}

/// Vérifie que TRADE_AMOUNT_USDC négatif est rejeté
#[test]
fn test_config_trade_amount_negative_uses_default() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "-50");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.trade_amount_usdc, 10.0);
    std::env::remove_var("EXECUTION_MODE");
    std::env::remove_var("TRADE_AMOUNT_USDC");
    std::env::remove_var("TRADE_AMOUNT_PCT");
    clear_config_env();
}

/// Vérifie que TRADE_AMOUNT_USDC non-parseable est rejeté
#[test]
fn test_config_trade_amount_invalid_string_uses_default() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "abc");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.trade_amount_usdc, 10.0);
    std::env::remove_var("EXECUTION_MODE");
    std::env::remove_var("TRADE_AMOUNT_USDC");
    std::env::remove_var("TRADE_AMOUNT_PCT");
    clear_config_env();
}

/// Vérifie qu'un EXECUTION_MODE inconnu (faute de frappe) est rejeté.
#[test]
fn test_config_unknown_execution_mode_returns_error() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "MARKET"); // majuscules incorrectes
    let result = rusty_poly_signal_runner::config::Config::from_env();
    assert!(result.is_err());
    std::env::remove_var("EXECUTION_MODE");
    clear_config_env();
}

#[test]
fn test_config_trade_amount_pct_valid_without_fixed_amount() {
    let _guard = env_lock();
    clear_config_env();
    without_repo_dotenv(|| {
        std::env::set_var("EXECUTION_MODE", "dry-run");
        std::env::set_var("TRADE_AMOUNT_PCT", "2.5");
        let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
        assert_eq!(config.trade_amount_pct, 2.5);
        assert_eq!(config.trade_amount_usdc, 10.0);
    });
    clear_config_env();
}

#[test]
fn test_config_trade_amount_pct_conflicts_with_fixed_amount() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "2.5");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    let result = rusty_poly_signal_runner::config::Config::from_env();
    assert!(result.is_err());
    clear_config_env();
}

#[test]
fn test_config_parses_excluded_days_and_hours() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("EXCLUDED_DAYS", "Sat, sun");
    std::env::set_var("EXCLUDED_HOURS", "0-9,22h-24h,bad,9-9,24-25");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.excluded_days, vec!["sat", "sun"]);
    assert_eq!(config.excluded_hours, vec![(0, 9), (22, 24)]);
    clear_config_env();
}

#[test]
fn test_config_parses_limit_price_offset() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("LIMIT_PRICE_OFFSET", "0.03");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.limit_price_offset, 0.03);
    clear_config_env();
}

#[test]
fn test_config_parses_limit_price_reference_best_bid() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("LIMIT_PRICE_REFERENCE", "best_bid");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.limit_price_reference, LimitPriceReference::BestBid);
    clear_config_env();
}

#[test]
fn test_config_rejects_unknown_limit_price_reference() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("LIMIT_PRICE_REFERENCE", "mid");
    let err = rusty_poly_signal_runner::config::Config::from_env().unwrap_err();
    assert!(err.to_string().contains("LIMIT_PRICE_REFERENCE"));
    clear_config_env();
}

#[test]
fn test_config_parses_limit_price_high_guard() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("LIMIT_PRICE_HIGH_GUARD_ENABLED", "true");
    std::env::set_var("LIMIT_PRICE_HIGH_GUARD_THRESHOLD", "0.61");
    std::env::set_var("LIMIT_PRICE_HIGH_GUARD_PRICE", "0.54");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert!(config.limit_price_high_guard.enabled);
    assert_eq!(config.limit_price_high_guard.threshold, 0.61);
    assert_eq!(config.limit_price_high_guard.price, 0.54);
    clear_config_env();
}

#[test]
fn test_config_rejects_invalid_limit_price_high_guard() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("LIMIT_PRICE_HIGH_GUARD_ENABLED", "true");
    std::env::set_var("LIMIT_PRICE_HIGH_GUARD_THRESHOLD", "0.60");
    std::env::set_var("LIMIT_PRICE_HIGH_GUARD_PRICE", "0.60");
    let err = rusty_poly_signal_runner::config::Config::from_env().unwrap_err();
    assert!(err.to_string().contains("LIMIT_PRICE_HIGH_GUARD_PRICE"));
    clear_config_env();
}

#[test]
fn test_config_parses_hourly_et_slug_format() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("POLYMARKET_SLUG_FORMAT", "hourly_et");
    std::env::set_var("POLYMARKET_SLUG_ASSET", "solana");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(
        config.polymarket_slug_format,
        PolymarketSlugFormat::HourlyEt
    );
    assert_eq!(config.polymarket_slug_asset, "solana");
    clear_config_env();
}

#[test]
fn test_config_defaults_hourly_asset_from_symbol() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("SYMBOL", "solusdt");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    assert_eq!(config.polymarket_slug_asset, "solana");
    clear_config_env();
}

#[test]
fn test_config_rejects_unknown_slug_format() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("TRADE_AMOUNT_USDC", "10");
    std::env::set_var("POLYMARKET_SLUG_FORMAT", "weekly");
    let err = rusty_poly_signal_runner::config::Config::from_env().unwrap_err();
    assert!(err.to_string().contains("POLYMARKET_SLUG_FORMAT"));
    clear_config_env();
}

/// Vérifie que Debug ne contient pas les secrets en clair
#[test]
fn test_config_debug_redacts_secrets() {
    let _guard = env_lock();
    clear_config_env();
    std::env::set_var("EXECUTION_MODE", "dry-run");
    std::env::set_var("TRADE_AMOUNT_PCT", "0");
    std::env::set_var("POLYMARKET_API_KEY", "super_secret_key");
    std::env::set_var("POLYMARKET_API_SECRET", "super_secret_value");
    let config = rusty_poly_signal_runner::config::Config::from_env().unwrap();
    let debug_str = format!("{:?}", config);
    assert!(
        !debug_str.contains("super_secret_key"),
        "La clé API ne doit pas apparaître dans Debug"
    );
    assert!(
        !debug_str.contains("super_secret_value"),
        "Le secret API ne doit pas apparaître dans Debug"
    );
    assert!(
        debug_str.contains("[REDACTED]"),
        "Les secrets doivent être remplacés par [REDACTED]"
    );
    std::env::remove_var("POLYMARKET_API_KEY");
    std::env::remove_var("POLYMARKET_API_SECRET");
    std::env::remove_var("EXECUTION_MODE");
    std::env::remove_var("TRADE_AMOUNT_PCT");
    clear_config_env();
}

#[test]
fn test_config_market_mode_requires_private_key() {
    let _guard = env_lock();
    clear_config_env();
    without_repo_dotenv(|| {
        std::env::set_var("EXECUTION_MODE", "market");
        std::env::set_var("TRADE_AMOUNT_USDC", "10");
        let result = rusty_poly_signal_runner::config::Config::from_env();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("POLYMARKET_PRIVATE_KEY"));
    });
    clear_config_env();
}

#[test]
fn test_config_market_mode_rejects_invalid_clob_url() {
    let _guard = env_lock();
    clear_config_env();
    without_repo_dotenv(|| {
        std::env::set_var("EXECUTION_MODE", "market");
        std::env::set_var("TRADE_AMOUNT_USDC", "10");
        std::env::set_var("POLYMARKET_PRIVATE_KEY", "abc123");
        std::env::set_var("POLYMARKET_API_URL", "clob.polymarket.com");
        let result = rusty_poly_signal_runner::config::Config::from_env();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http://"));
    });
    clear_config_env();
}

#[test]
fn test_config_proxy_signature_requires_funder() {
    let _guard = env_lock();
    clear_config_env();
    without_repo_dotenv(|| {
        std::env::set_var("EXECUTION_MODE", "limit");
        std::env::set_var("TRADE_AMOUNT_USDC", "10");
        std::env::set_var("POLYMARKET_PRIVATE_KEY", "abc123");
        std::env::set_var("POLYMARKET_SIGNATURE_TYPE", "2");
        let result = rusty_poly_signal_runner::config::Config::from_env();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("POLYMARKET_FUNDER"));
    });
    clear_config_env();
}
