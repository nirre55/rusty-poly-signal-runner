use anyhow::{bail, Result};
use std::path::PathBuf;

use rusty_poly_signal_runner::config::Config;
use rusty_poly_signal_runner::microstructure_audit::{verify_audit_file, AUDIT_FILE_NAME};

fn main() -> Result<()> {
    let config_path = parse_config_path()?;
    std::env::set_var("STRATEGY_CONFIG", &config_path);
    let config = Config::from_env()?;
    if config.strategy != "ethusd_perp_coinm_15m_microstructure_mixed_13" {
        bail!(
            "ce verificateur est reserve a mixed_13; strategie configuree: {}",
            config.strategy
        );
    }

    let audit_path = PathBuf::from(&config.logs_dir).join(AUDIT_FILE_NAME);
    let report = verify_audit_file(&audit_path)?;
    println!("Fichier: {}", audit_path.display());
    println!(
        "Records: {} | decisions: {} | UP: {} | DOWN: {} | SKIP: {} | collection_errors: {}",
        report.total_records,
        report.decisions,
        report.up,
        report.down,
        report.skip,
        report.collection_errors
    );
    if report.is_valid() {
        println!("Audit: OK");
        return Ok(());
    }

    println!("Audit: ECHEC ({} divergence(s))", report.failures.len());
    for failure in &report.failures {
        println!("- {failure}");
    }
    bail!("journal microstructure invalide")
}

fn parse_config_path() -> Result<String> {
    let mut arguments = std::env::args().skip(1);
    let Some(flag) = arguments.next() else {
        bail!("usage: audit_microstructure_decisions --config <chemin>");
    };
    if flag != "--config" {
        bail!("argument inconnu: {flag}; usage: --config <chemin>");
    }
    let Some(path) = arguments.next() else {
        bail!("--config requiert un chemin");
    };
    if arguments.next().is_some() {
        bail!("usage: audit_microstructure_decisions --config <chemin>");
    }
    Ok(path)
}
