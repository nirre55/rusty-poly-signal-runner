use anyhow::Result;
use chrono::Utc;
use rusty_poly_signal_runner::config::{Config, LimitPriceHighGuard, LimitPriceReference};
use rusty_poly_signal_runner::polymarket::{
    calculate_available_shares_up_to_price, calculate_limit_order_quote,
    parse_book_reference_price_body, validate_sufficient_usdc_balance, PolymarketClient,
};

#[derive(Debug)]
struct TokenQuote<'a> {
    side: &'a str,
    token_id: &'a str,
    reference_price: Option<f64>,
    ws_reference_price: Option<f64>,
    limit_price: f64,
    expected_shares: f64,
    effective_usdc: f64,
    adjusted_to_min_size: bool,
    high_guard_applied: bool,
    available_shares_at_limit: Option<f64>,
}

struct QuoteRequest<'a> {
    side: &'a str,
    token_id: &'a str,
    requested_usdc: f64,
    min_size: f64,
    limit_price_reference: LimitPriceReference,
    limit_price_offset: f64,
    limit_price_high_guard: LimitPriceHighGuard,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let client = PolymarketClient::new(config.clone());
    let http = reqwest::Client::new();

    let interval_ms = interval_ms(&config.interval)?;
    let now_ms = Utc::now().timestamp_millis();
    let next_open_ms = (now_ms / interval_ms + 1) * interval_ms;
    let slug = PolymarketClient::build_configured_slug(&config, next_open_ms);

    println!("Live limit diagnostic (read-only)");
    println!(
        "symbol={} interval={} slug={}",
        config.symbol, config.interval, slug
    );

    let market = client.resolve_market(&slug).await?;
    let balance = client.get_usdc_balance().await?;
    let requested_usdc = if config.trade_amount_pct > 0.0 {
        ((balance * config.trade_amount_pct / 100.0) * 100.0)
            .floor()
            .max(100.0)
            / 100.0
    } else {
        config.trade_amount_usdc
    };

    println!(
        "balance={:.2} USDC requested={:.2} USDC min_size={:.2} shares reference={} offset={:.4} high_guard={} threshold={:.4} price={:.4}",
        balance,
        requested_usdc,
        market.order_min_size,
        config.limit_price_reference.as_str(),
        config.limit_price_offset,
        config.limit_price_high_guard.enabled,
        config.limit_price_high_guard.threshold,
        config.limit_price_high_guard.price
    );

    let up = quote_token(
        &client,
        &http,
        &config.polymarket_api_url,
        QuoteRequest {
            side: "UP",
            token_id: &market.up_token_id,
            requested_usdc,
            min_size: market.order_min_size,
            limit_price_reference: config.limit_price_reference,
            limit_price_offset: config.limit_price_offset,
            limit_price_high_guard: config.limit_price_high_guard,
        },
    )
    .await?;
    let down = quote_token(
        &client,
        &http,
        &config.polymarket_api_url,
        QuoteRequest {
            side: "DOWN",
            token_id: &market.down_token_id,
            requested_usdc,
            min_size: market.order_min_size,
            limit_price_reference: config.limit_price_reference,
            limit_price_offset: config.limit_price_offset,
            limit_price_high_guard: config.limit_price_high_guard,
        },
    )
    .await?;

    print_quote(&up, balance);
    print_quote(&down, balance);

    Ok(())
}

async fn quote_token<'a>(
    client: &PolymarketClient,
    http: &reqwest::Client,
    clob_api_base: &str,
    request: QuoteRequest<'a>,
) -> Result<TokenQuote<'a>> {
    let base = clob_api_base.trim_end_matches('/');
    let body = http
        .get(format!("{}/book?token_id={}", base, request.token_id))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let reference_price = parse_book_reference_price_body(&body, request.limit_price_reference);
    let ws_reference_price = client
        .get_reference_price_ws_snapshot(
            request.token_id,
            std::time::Duration::from_millis(1500),
            request.limit_price_reference,
        )
        .await;
    let quote = calculate_limit_order_quote(
        request.requested_usdc,
        request.min_size,
        reference_price,
        request.limit_price_offset,
        request.limit_price_high_guard,
    );
    let available_shares_at_limit =
        calculate_available_shares_up_to_price(&body, quote.limit_price);

    Ok(TokenQuote {
        side: request.side,
        token_id: request.token_id,
        reference_price,
        ws_reference_price,
        limit_price: quote.limit_price,
        expected_shares: quote.expected_shares,
        effective_usdc: quote.effective_usdc,
        adjusted_to_min_size: quote.adjusted_to_min_size,
        high_guard_applied: quote.high_guard_applied,
        available_shares_at_limit,
    })
}

fn print_quote(quote: &TokenQuote<'_>, balance: f64) {
    let balance_status = match validate_sufficient_usdc_balance(quote.effective_usdc, balance) {
        Ok(()) => "OK",
        Err(_) => "INSUFFICIENT_BALANCE",
    };

    println!(
        "{} token={} rest_reference_price={} ws_reference_price={} limit_price={:.4} expected_shares={:.2} available_shares_at_limit={} effective_usdc={:.2} adjusted_to_min_size={} high_guard_applied={} balance_status={}",
        quote.side,
        quote.token_id,
        quote
            .reference_price
            .map(|price| format!("{:.4}", price))
            .unwrap_or_else(|| "NONE".to_string()),
        quote
            .ws_reference_price
            .map(|price| format!("{:.4}", price))
            .unwrap_or_else(|| "NONE".to_string()),
        quote.limit_price,
        quote.expected_shares,
        quote
            .available_shares_at_limit
            .map(|shares| format!("{:.2}", shares))
            .unwrap_or_else(|| "UNKNOWN".to_string()),
        quote.effective_usdc,
        quote.adjusted_to_min_size,
        quote.high_guard_applied,
        balance_status
    );
}

fn interval_ms(interval: &str) -> Result<i64> {
    let (value, multiplier) = if let Some(value) = interval.strip_suffix('m') {
        (value, 60 * 1000)
    } else if let Some(value) = interval.strip_suffix('h') {
        (value, 60 * 60 * 1000)
    } else {
        anyhow::bail!("interval non supporte pour ce diagnostic: {}", interval);
    };
    Ok(value.parse::<i64>()? * multiplier)
}
