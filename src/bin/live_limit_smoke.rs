use anyhow::{anyhow, Result};
use chrono::Utc;
use rusty_poly_streak_rsi::config::{Config, ExecutionMode};
use rusty_poly_streak_rsi::polymarket::PolymarketClient;
use rusty_poly_streak_rsi::strategy::{Prediction, Signal};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::env::var("CONFIRM_LIVE_ORDER").as_deref() != Ok("yes") {
        return Err(anyhow!(
            "CONFIRM_LIVE_ORDER=yes requis pour autoriser un vrai ordre"
        ));
    }

    let side = match std::env::var("LIVE_TEST_SIDE")
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "UP" => Prediction::Up,
        "DOWN" => Prediction::Down,
        _ => return Err(anyhow!("LIVE_TEST_SIDE doit etre UP ou DOWN")),
    };

    let monitor_secs = std::env::var("LIVE_TEST_MONITOR_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20);

    let mut config = Config::from_env()?;
    config.execution_mode = ExecutionMode::Limit;
    if let Ok(max_usdc) = std::env::var("LIVE_TEST_MAX_USDC") {
        config.trade_amount_usdc = max_usdc.parse::<f64>()?;
    }

    let interval_ms = interval_ms(&config.interval)?;
    let next_open_ms = (Utc::now().timestamp_millis() / interval_ms + 1) * interval_ms;
    let slug = PolymarketClient::build_configured_slug(&config, next_open_ms);
    let client = PolymarketClient::new(config.clone());
    let market = client.resolve_market(&slug).await?;

    println!(
        "Live limit smoke test | slug={} side={} requested={:.2}USDC monitor={}s",
        slug, side, config.trade_amount_usdc, monitor_secs
    );

    let signal = Signal {
        prediction: side,
        signal_candle_close_time: Utc::now(),
        rsi: 50.0,
        strategy_name: "live_limit_smoke".to_string(),
    };

    let order = client
        .place_order(&signal, &market, config.trade_amount_usdc)
        .await?;
    println!(
        "order_sent id={} status={} amount_usdc={:.2}",
        order.order_id, order.status, order.amount_usdc
    );

    let status = monitor_order(&client, &order.order_id, monitor_secs).await;
    match status.as_deref() {
        Some(status) if is_filled_status(status) => {
            println!("order_filled id={} status={}", order.order_id, status);
            println!(
                "position ouverte: ce test ne choisit pas automatiquement une sortie financiere"
            );
        }
        Some(status) if is_open_status(status) => {
            let cancel = client.cancel_order(&order.order_id).await?;
            println!(
                "order_open_after_monitor status={} canceled={:?} not_canceled={:?}",
                status, cancel.canceled, cancel.not_canceled
            );
        }
        Some(status) => {
            println!(
                "order_terminal_or_unknown id={} status={}",
                order.order_id, status
            );
        }
        None => {
            let cancel = client.cancel_order(&order.order_id).await?;
            println!(
                "status_unavailable_after_monitor canceled={:?} not_canceled={:?}",
                cancel.canceled, cancel.not_canceled
            );
        }
    }

    Ok(())
}

async fn monitor_order(
    client: &PolymarketClient,
    order_id: &str,
    monitor_secs: u64,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(monitor_secs);
    let mut last_status = None;
    while tokio::time::Instant::now() < deadline {
        match client.get_order_status(order_id).await {
            Ok(status) => {
                println!("order_status id={} status={}", order_id, status);
                if !is_open_status(&status) {
                    return Some(status);
                }
                last_status = Some(status);
            }
            Err(e) => {
                eprintln!("order_status_failed id={} error={}", order_id, e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    last_status
}

fn is_open_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "LIVE" | "OPEN" | "ACTIVE" | "UNMATCHED"
    )
}

fn is_filled_status(status: &str) -> bool {
    matches!(status.to_ascii_uppercase().as_str(), "MATCHED" | "FILLED")
}

fn interval_ms(interval: &str) -> Result<i64> {
    let (value, multiplier) = if let Some(value) = interval.strip_suffix('m') {
        (value, 60 * 1000)
    } else if let Some(value) = interval.strip_suffix('h') {
        (value, 60 * 60 * 1000)
    } else {
        anyhow::bail!("interval non supporte pour ce test: {}", interval);
    };
    Ok(value.parse::<i64>()? * multiplier)
}
