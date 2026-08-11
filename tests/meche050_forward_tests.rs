use std::fs;
use std::path::Path;

use rusty_poly_signal_runner::portfolio::{EnabledStrategies, MarketSlot, PortfolioStrategy};

#[test]
fn forward_profile_is_dry_run_recorder_with_all_outputs_enabled() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config_path = root.join("configs/meche050_forward.env");
    let matrix_path = root.join("configs/meche050_forward_enabled.env");
    let config = fs::read_to_string(&config_path).unwrap();

    assert!(config.lines().any(|line| line == "EXECUTION_MODE=dry-run"));
    assert!(config
        .lines()
        .any(|line| line == "LOGS_DIR=logs/meche050-forward"));
    assert!(config.lines().any(|line| line == "LIMIT_PRICE_FIXED=0.50"));
    assert!(config
        .lines()
        .any(|line| line == "PORTFOLIO_RECORDER_ENABLED=true"));

    let enabled = EnabledStrategies::load(&matrix_path).unwrap();
    for strategy in PortfolioStrategy::ALL {
        for market in MarketSlot::ALL {
            assert!(
                enabled.is_enabled(strategy, market),
                "{} doit être actif sur {}",
                strategy.key(),
                market.key()
            );
        }
    }
}
