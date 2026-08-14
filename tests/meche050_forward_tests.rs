use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

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
    assert!(config
        .lines()
        .any(|line| { line == "PORTFOLIO_RECORDER_DELETE_STREAM_AFTER_SUMMARY=true" }));

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

#[cfg(unix)]
#[test]
fn launcher_derives_runtime_and_enabled_paths_from_selected_config() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = std::env::temp_dir().join(format!(
        "meche050-launcher-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&fixture).unwrap();
    let matrix_path = fixture.join("enabled.env");
    let matrix = PortfolioStrategy::ALL
        .into_iter()
        .flat_map(|strategy| {
            MarketSlot::ALL.into_iter().map(move |market| {
                format!(
                    "MECHE050_ENABLED_{}_{}=false",
                    strategy.key().to_ascii_uppercase(),
                    market.key().to_ascii_uppercase()
                )
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&matrix_path, format!("{matrix}\n")).unwrap();
    let config_path = fixture.join("profile.env");
    fs::write(
        &config_path,
        format!(
            "LOGS_DIR=logs/launcher-profile\nPORTFOLIO_ENABLED_CONFIG={}\n",
            matrix_path.display()
        ),
    )
    .unwrap();

    let status = launcher(root, &config_path, &["status"]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout)
        .contains("logs/launcher-profile/supervisor/portfolio_runner.console.log"));

    let matrix_status = launcher(root, &config_path, &["strategy", "status"]);
    assert!(matrix_status.status.success());
    let stdout = String::from_utf8_lossy(&matrix_status.stdout);
    assert!(stdout.lines().any(|line| line.contains("boll_fade")
        && line.contains("btc_5m")
        && line.ends_with("false")));

    fs::remove_dir_all(fixture).unwrap();
}

#[cfg(unix)]
fn launcher(root: &Path, config_path: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("bash")
        .arg(root.join("start_meche050.sh"))
        .args(arguments)
        .current_dir(root)
        .env("MECHE050_CONFIG", config_path)
        .env_remove("MECHE050_ENABLED_CONFIG")
        .env_remove("MECHE050_RUNTIME_LOGS")
        .output()
        .unwrap()
}
