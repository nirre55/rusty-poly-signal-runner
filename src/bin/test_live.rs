//! Test d'intégration live Polymarket.
//!
//! Lance avec : cargo run --bin test_live
//!
//! Ce binaire :
//!  1. Résout le marché btc-updown-5m courant via l'API Gamma
//!  2. Place un ordre UP FAK à partir de 1 USDC
//!  3. Attend 5 secondes
//!  4. Vérifie le statut de l'ordre via GET /orders/{id}
//!  5. Affiche un résumé pass/fail
//!
//! Sécurité :
//!  - nécessite `CONFIRM_LIVE_ORDER=yes` pour éviter d'ouvrir un ordre réel par erreur.

use anyhow::Result;
use chrono::Utc;
use rusty_poly_signal_runner::config::{Config, ExecutionMode};
use rusty_poly_signal_runner::polymarket::PolymarketClient;
use rusty_poly_signal_runner::strategy::{Prediction, Signal};

#[tokio::main]
async fn main() -> Result<()> {
    // Logs console
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Chargement .env + config
    let mut config = Config::from_env()?;

    if std::env::var("CONFIRM_LIVE_ORDER").as_deref() != Ok("yes") {
        eprintln!(
            "[ABORT] Test live bloqué. Définir CONFIRM_LIVE_ORDER=yes pour autoriser l'ouverture d'un ordre réel."
        );
        std::process::exit(1);
    }

    // Forcer mode Market pour le test (même si .env dit dry-run)
    if matches!(config.execution_mode, ExecutionMode::DryRun) {
        eprintln!("[WARN] .env est en dry-run — passage forcé en mode Market pour ce test.");
        config.execution_mode = ExecutionMode::Market;
    }
    config.trade_amount_usdc = 1.0; // minimum = 1$ sur Polymarket
    let trade_amount = config.trade_amount_usdc;

    let client = PolymarketClient::new(config);

    // ── Étape 1 : warm-up TCP/TLS ─────────────────────────────────────────────
    println!("\n[1/4] Pré-chauffe connexion CLOB...");
    client.warm_up().await;

    // ── Étape 1b : vérifier que l'API Gamma répond ───────────────────────────
    let now_ms = Utc::now().timestamp_millis();
    let interval_ms = 5 * 60 * 1000i64;
    let next_open_ms = (now_ms / interval_ms + 1) * interval_ms;
    let slug = PolymarketClient::build_slug("btc-updown-5m", next_open_ms);
    let gamma_url = format!("https://gamma-api.polymarket.com/markets?slug={}", slug);

    println!("[1b] Vérification Gamma API pour slug={}", slug);
    let http = reqwest::Client::new();
    match http.get(&gamma_url).send().await {
        Ok(resp) => {
            println!("      ✓ HTTP {}", resp.status());
        }
        Err(e) => eprintln!("[WARN] Impossible de contacter Gamma API: {}", e),
    }

    // ── Étape 2 : résoudre le marché courant ──────────────────────────────────
    println!("[2/4] Résolution marché : slug={}", slug);
    let market = match client.resolve_market(&slug).await {
        Ok(m) => {
            println!("      ✓ condition_id={}", m.condition_id);
            println!("      ✓ UP  token={}", m.up_token_id);
            println!("      ✓ DOWN token={}", m.down_token_id);
            m
        }
        Err(e) => {
            eprintln!("\n[FAIL] Résolution marché échouée: {}", e);
            std::process::exit(1);
        }
    };

    // ── Étape 3 : placer un ordre UP à 1 USDC ────────────────────────────────
    let signal = Signal {
        prediction: Prediction::Up,
        signal_candle_close_time: Utc::now(),
        rsi: 30.0,
        strategy_name: "test_live".to_string(),
    };

    println!("[3/4] Placement ordre UP (1 USDC)...");
    let order = match client.place_order(&signal, &market, trade_amount).await {
        Ok(o) => {
            println!("      ✓ order_id={}", o.order_id);
            println!("      ✓ status={}", o.status);
            o
        }
        Err(e) => {
            eprintln!("\n[FAIL] Placement ordre échoué: {}", e);
            std::process::exit(1);
        }
    };

    // ── Étape 4 : attendre 5s puis vérifier le statut ─────────────────────────
    println!("[4/4] Attente 5 secondes...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    match client.get_order_status(&order.order_id).await {
        Ok(status) => {
            println!("      ✓ Statut après 5s : {}", status);
            println!("\n══════════════════════════════════════");
            println!(" RÉSULTAT : OK — pipeline Polymarket fonctionnel");
            println!("══════════════════════════════════════\n");
        }
        Err(e) => {
            eprintln!("\n[WARN] Ordre placé mais get_order_status échoué: {}", e);
            eprintln!(
                "       L'ordre a bien été envoyé (id={}), seul le polling a échoué.",
                order.order_id
            );
        }
    }

    Ok(())
}
