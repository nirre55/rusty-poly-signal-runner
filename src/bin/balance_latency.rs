use anyhow::Result;
use rusty_poly_signal_runner::config::Config;
use rusty_poly_signal_runner::polymarket::PolymarketClient;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .init();

    let samples = std::env::var("BALANCE_LATENCY_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(10);

    let config = Config::from_env()?;
    let client = PolymarketClient::new(config);
    let mut timings = Vec::with_capacity(samples);
    let mut last_balance = 0.0;

    for index in 0..samples {
        let started = Instant::now();
        last_balance = client.get_usdc_balance().await?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        timings.push(elapsed_ms);
        println!(
            "sample={}/{} balance={:.2}USDC latency={}ms",
            index + 1,
            samples,
            last_balance,
            elapsed_ms
        );
    }

    timings.sort_unstable();
    let total: u64 = timings.iter().sum();
    let avg = total as f64 / timings.len() as f64;
    let p95_index = ((timings.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);

    println!(
        "summary samples={} last_balance={:.2}USDC min={}ms avg={:.1}ms p95={}ms max={}ms",
        timings.len(),
        last_balance,
        timings[0],
        avg,
        timings[p95_index],
        timings[timings.len() - 1]
    );

    Ok(())
}
