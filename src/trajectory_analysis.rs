//! Anti-lookahead analysis of durable forward-test trajectories.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use crate::trajectory::open_trajectory_reader;

pub const RISK_HORIZONS_MS: [(&str, i64); 6] = [
    ("t15s", 15_000),
    ("t30s", 30_000),
    ("t60s", 60_000),
    ("t120s", 120_000),
    ("t180s", 180_000),
    ("t300s", 300_000),
];
pub const BELOW_THRESHOLDS: [(&str, f64); 4] = [
    ("below_0_45", 0.45),
    ("below_0_40", 0.40),
    ("below_0_35", 0.35),
    ("below_0_30", 0.30),
];
pub const ABOVE_THRESHOLDS: [(&str, f64); 4] = [
    ("above_0_55", 0.55),
    ("above_0_60", 0.60),
    ("above_0_65", 0.65),
    ("above_0_70", 0.70),
];

#[derive(Debug, Clone, Copy)]
pub struct ExitCostAssumptions {
    pub fee_bps: f64,
    pub slippage_bps: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrajectoryEvent {
    pub received_at_unix_ms: i64,
    pub event_type: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotePoint {
    pub observed_at_unix_ms: i64,
    pub best_bid: Option<f64>,
    pub best_bid_size: Option<f64>,
    pub bid_shares_available: Option<f64>,
    pub sell_vwap_5: Option<f64>,
    pub sell_vwap_candidate: Option<f64>,
    pub candidate_shares: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_ask_size: Option<f64>,
    pub ask_shares_at_or_below_limit: Option<f64>,
    pub last_trade_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrictFill {
    pub observed_at_unix_ms: i64,
    pub elapsed_from_signal_ms: i64,
    pub quote: QuotePoint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimedPrice {
    pub observed_at_unix_ms: i64,
    pub elapsed_from_fill_ms: i64,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdPath {
    pub first_elapsed_from_fill_ms: Option<i64>,
    pub first_best_bid: Option<f64>,
    pub first_sell_vwap_5: Option<f64>,
    pub first_gross_pnl_5_usdc: Option<f64>,
    pub first_net_pnl_5_usdc: Option<f64>,
    pub duration_ms: i64,
    pub recovered_to_half: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExitCheckpoint {
    pub target_elapsed_from_fill_ms: i64,
    pub observed_at_unix_ms: Option<i64>,
    pub unavailable_after_resolution: bool,
    pub best_bid: Option<f64>,
    pub best_bid_size: Option<f64>,
    pub bid_shares_available: Option<f64>,
    pub best_ask: Option<f64>,
    pub spread: Option<f64>,
    pub last_trade_price: Option<f64>,
    pub sell_vwap_5: Option<f64>,
    pub sell_vwap_candidate: Option<f64>,
    pub candidate_shares: Option<f64>,
    pub gross_pnl_5_usdc: Option<f64>,
    pub net_pnl_5_usdc: Option<f64>,
    pub binance_signed_move_bps: Option<f64>,
    pub time_remaining_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DrawdownPoint {
    pub peak_observed_at_unix_ms: i64,
    pub trough_observed_at_unix_ms: i64,
    pub peak_elapsed_from_fill_ms: i64,
    pub trough_elapsed_from_fill_ms: i64,
    pub peak_best_bid: f64,
    pub trough_best_bid: f64,
    pub drawdown: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskPathMetrics {
    pub mae_best_bid: Option<TimedPrice>,
    pub mfe_best_bid: Option<TimedPrice>,
    pub maximum_drawdown: Option<DrawdownPoint>,
    pub checkpoints: BTreeMap<String, ExitCheckpoint>,
    pub below_thresholds: BTreeMap<String, ThresholdPath>,
    pub above_thresholds: BTreeMap<String, ThresholdPath>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeTrajectoryAnalysis {
    pub quote_at_signal: Option<QuotePoint>,
    pub strict_fill: Option<StrictFill>,
    pub risk: Option<RiskPathMetrics>,
}

#[derive(Debug, Clone, Copy)]
struct BinancePoint {
    observed_at_unix_ms: i64,
    open: f64,
    close: f64,
}

pub fn load_trajectory_events(path: &Path) -> Result<Vec<TrajectoryEvent>> {
    let mut events = Vec::new();
    for (line_number, line) in open_trajectory_reader(path)?.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str(&line).with_context(|| {
            format!(
                "trajectoire invalide {} ligne {}",
                path.display(),
                line_number + 1
            )
        })?);
    }
    Ok(events)
}

pub fn analyze_outcome(
    events: &[TrajectoryEvent],
    outcome: &str,
    signal_at_unix_ms: i64,
    target_close_time_ms: i64,
    costs: ExitCostAssumptions,
) -> OutcomeTrajectoryAnalysis {
    let mut quotes = events
        .iter()
        .filter_map(|event| quote_from_event(event, outcome))
        .collect::<Vec<_>>();
    quotes.sort_by_key(|quote| quote.observed_at_unix_ms);
    let mut binance = events
        .iter()
        .filter_map(binance_from_event)
        .collect::<Vec<_>>();
    binance.sort_by_key(|point| point.observed_at_unix_ms);

    let quote_at_signal = quotes
        .iter()
        .rfind(|quote| quote.observed_at_unix_ms <= signal_at_unix_ms)
        .cloned();
    let strict_fill = quotes
        .iter()
        .find(|quote| {
            quote.observed_at_unix_ms >= signal_at_unix_ms
                && quote.best_ask.is_some_and(|ask| ask < 0.50)
        })
        .cloned()
        .map(|quote| StrictFill {
            observed_at_unix_ms: quote.observed_at_unix_ms,
            elapsed_from_signal_ms: (quote.observed_at_unix_ms - signal_at_unix_ms).max(0),
            quote,
        });
    let risk = strict_fill.as_ref().map(|fill| {
        build_risk_path(
            &quotes,
            &binance,
            fill,
            outcome,
            target_close_time_ms,
            costs,
        )
    });

    OutcomeTrajectoryAnalysis {
        quote_at_signal,
        strict_fill,
        risk,
    }
}

fn build_risk_path(
    quotes: &[QuotePoint],
    binance: &[BinancePoint],
    fill: &StrictFill,
    outcome: &str,
    target_close_time_ms: i64,
    costs: ExitCostAssumptions,
) -> RiskPathMetrics {
    let post_fill = quotes
        .iter()
        .filter(|quote| {
            quote.observed_at_unix_ms >= fill.observed_at_unix_ms
                && quote.observed_at_unix_ms <= target_close_time_ms
        })
        .collect::<Vec<_>>();
    let mae_best_bid = post_fill
        .iter()
        .filter_map(|quote| quote.best_bid.map(|bid| (*quote, bid)))
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(quote, value)| timed_price(quote.observed_at_unix_ms, fill, value));
    let mfe_best_bid = post_fill
        .iter()
        .filter_map(|quote| quote.best_bid.map(|bid| (*quote, bid)))
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(quote, value)| timed_price(quote.observed_at_unix_ms, fill, value));
    let maximum_drawdown = maximum_drawdown(&post_fill, fill);

    let checkpoints = RISK_HORIZONS_MS
        .into_iter()
        .map(|(name, elapsed_ms)| {
            (
                name.to_string(),
                checkpoint(
                    &post_fill,
                    binance,
                    fill,
                    outcome,
                    target_close_time_ms,
                    elapsed_ms,
                    costs,
                ),
            )
        })
        .collect();
    let below_thresholds = BELOW_THRESHOLDS
        .into_iter()
        .map(|(name, threshold)| {
            (
                name.to_string(),
                threshold_path(
                    &post_fill,
                    fill,
                    target_close_time_ms,
                    threshold,
                    true,
                    costs,
                ),
            )
        })
        .collect();
    let above_thresholds = ABOVE_THRESHOLDS
        .into_iter()
        .map(|(name, threshold)| {
            (
                name.to_string(),
                threshold_path(
                    &post_fill,
                    fill,
                    target_close_time_ms,
                    threshold,
                    false,
                    costs,
                ),
            )
        })
        .collect();

    RiskPathMetrics {
        mae_best_bid,
        mfe_best_bid,
        maximum_drawdown,
        checkpoints,
        below_thresholds,
        above_thresholds,
    }
}

fn checkpoint(
    post_fill: &[&QuotePoint],
    binance: &[BinancePoint],
    fill: &StrictFill,
    outcome: &str,
    target_close_time_ms: i64,
    elapsed_ms: i64,
    costs: ExitCostAssumptions,
) -> ExitCheckpoint {
    let target_ms = fill.observed_at_unix_ms + elapsed_ms;
    if target_ms > target_close_time_ms {
        return ExitCheckpoint {
            target_elapsed_from_fill_ms: elapsed_ms,
            observed_at_unix_ms: None,
            unavailable_after_resolution: true,
            best_bid: None,
            best_bid_size: None,
            bid_shares_available: None,
            best_ask: None,
            spread: None,
            last_trade_price: None,
            sell_vwap_5: None,
            sell_vwap_candidate: None,
            candidate_shares: None,
            gross_pnl_5_usdc: None,
            net_pnl_5_usdc: None,
            binance_signed_move_bps: None,
            time_remaining_ms: 0,
        };
    }
    let quote = post_fill
        .iter()
        .rfind(|quote| quote.observed_at_unix_ms <= target_ms)
        .copied();
    let binance_point = binance
        .iter()
        .rfind(|point| point.observed_at_unix_ms <= target_ms);
    let gross_pnl = quote
        .and_then(|quote| quote.sell_vwap_5)
        .map(|vwap| (vwap - 0.50) * 5.0);
    let net_pnl = quote
        .and_then(|quote| quote.sell_vwap_5)
        .map(|vwap| net_exit_pnl(vwap, 5.0, costs));
    let direction = if outcome.eq_ignore_ascii_case("UP") {
        1.0
    } else {
        -1.0
    };
    let signed_move = binance_point.and_then(|point| {
        (point.open > 0.0).then_some((point.close / point.open - 1.0) * 10_000.0 * direction)
    });
    let spread = quote
        .and_then(|quote| quote.best_ask.zip(quote.best_bid))
        .map(|(ask, bid)| ask - bid);
    ExitCheckpoint {
        target_elapsed_from_fill_ms: elapsed_ms,
        observed_at_unix_ms: quote.map(|quote| quote.observed_at_unix_ms),
        unavailable_after_resolution: false,
        best_bid: quote.and_then(|quote| quote.best_bid),
        best_bid_size: quote.and_then(|quote| quote.best_bid_size),
        bid_shares_available: quote.and_then(|quote| quote.bid_shares_available),
        best_ask: quote.and_then(|quote| quote.best_ask),
        spread,
        last_trade_price: quote.and_then(|quote| quote.last_trade_price),
        sell_vwap_5: quote.and_then(|quote| quote.sell_vwap_5),
        sell_vwap_candidate: quote.and_then(|quote| quote.sell_vwap_candidate),
        candidate_shares: quote.and_then(|quote| quote.candidate_shares),
        gross_pnl_5_usdc: gross_pnl,
        net_pnl_5_usdc: net_pnl,
        binance_signed_move_bps: signed_move,
        time_remaining_ms: (target_close_time_ms - target_ms).max(0),
    }
}

fn maximum_drawdown(post_fill: &[&QuotePoint], fill: &StrictFill) -> Option<DrawdownPoint> {
    let mut running_peak: Option<(i64, f64)> = None;
    let mut maximum: Option<DrawdownPoint> = None;
    for quote in post_fill {
        let Some(best_bid) = quote.best_bid else {
            continue;
        };
        if running_peak.is_none_or(|(_, peak)| best_bid > peak) {
            running_peak = Some((quote.observed_at_unix_ms, best_bid));
        }
        let Some((peak_at, peak_bid)) = running_peak else {
            continue;
        };
        let drawdown = (peak_bid - best_bid).max(0.0);
        if maximum
            .as_ref()
            .is_none_or(|current| drawdown > current.drawdown)
        {
            maximum = Some(DrawdownPoint {
                peak_observed_at_unix_ms: peak_at,
                trough_observed_at_unix_ms: quote.observed_at_unix_ms,
                peak_elapsed_from_fill_ms: (peak_at - fill.observed_at_unix_ms).max(0),
                trough_elapsed_from_fill_ms: (quote.observed_at_unix_ms - fill.observed_at_unix_ms)
                    .max(0),
                peak_best_bid: peak_bid,
                trough_best_bid: best_bid,
                drawdown,
            });
        }
    }
    maximum
}

fn threshold_path(
    post_fill: &[&QuotePoint],
    fill: &StrictFill,
    target_close_time_ms: i64,
    threshold: f64,
    below: bool,
    costs: ExitCostAssumptions,
) -> ThresholdPath {
    let matches = |price: f64| {
        if below {
            price < threshold
        } else {
            price > threshold
        }
    };
    let first = post_fill
        .iter()
        .find(|quote| quote.best_bid.is_some_and(matches))
        .copied();
    let mut duration_ms = 0_i64;
    for (index, quote) in post_fill.iter().enumerate() {
        let Some(price) = quote.best_bid else {
            continue;
        };
        if !matches(price) {
            continue;
        }
        let next_at = post_fill
            .get(index + 1)
            .map_or(target_close_time_ms, |next| next.observed_at_unix_ms)
            .min(target_close_time_ms);
        duration_ms += (next_at - quote.observed_at_unix_ms).max(0);
    }
    let recovered_to_half = below
        && first.is_some_and(|first_quote| {
            post_fill.iter().any(|quote| {
                quote.observed_at_unix_ms > first_quote.observed_at_unix_ms
                    && quote.best_bid.is_some_and(|price| price >= 0.50)
            })
        });
    let first_sell_vwap_5 = first.and_then(|quote| quote.sell_vwap_5);
    ThresholdPath {
        first_elapsed_from_fill_ms: first
            .map(|quote| (quote.observed_at_unix_ms - fill.observed_at_unix_ms).max(0)),
        first_best_bid: first.and_then(|quote| quote.best_bid),
        first_sell_vwap_5,
        first_gross_pnl_5_usdc: first_sell_vwap_5.map(|vwap| (vwap - 0.50) * 5.0),
        first_net_pnl_5_usdc: first_sell_vwap_5.map(|vwap| net_exit_pnl(vwap, 5.0, costs)),
        duration_ms,
        recovered_to_half,
    }
}

fn timed_price(observed_at_unix_ms: i64, fill: &StrictFill, value: f64) -> TimedPrice {
    TimedPrice {
        observed_at_unix_ms,
        elapsed_from_fill_ms: (observed_at_unix_ms - fill.observed_at_unix_ms).max(0),
        value,
    }
}

fn net_exit_pnl(vwap: f64, shares: f64, costs: ExitCostAssumptions) -> f64 {
    let slipped_vwap = vwap * (1.0 - costs.slippage_bps.max(0.0) / 10_000.0);
    let proceeds = slipped_vwap * shares;
    let fee = proceeds * costs.fee_bps.max(0.0) / 10_000.0;
    proceeds - fee - 0.50 * shares
}

fn quote_from_event(event: &TrajectoryEvent, outcome: &str) -> Option<QuotePoint> {
    if !matches!(event.event_type.as_str(), "quote" | "signal_snapshot") {
        return None;
    }
    let event_outcome = event.payload.get("outcome")?.as_str()?;
    if !event_outcome.eq_ignore_ascii_case(outcome) {
        return None;
    }
    Some(QuotePoint {
        observed_at_unix_ms: event
            .payload
            .get("observed_at_unix_ms")
            .and_then(Value::as_i64)
            .unwrap_or(event.received_at_unix_ms),
        best_bid: number(&event.payload, "best_bid"),
        best_bid_size: number(&event.payload, "best_bid_size"),
        bid_shares_available: number(&event.payload, "bid_shares_available"),
        sell_vwap_5: number(&event.payload, "sell_vwap_5"),
        sell_vwap_candidate: number(&event.payload, "sell_vwap_candidate"),
        candidate_shares: number(&event.payload, "candidate_shares"),
        best_ask: number(&event.payload, "best_ask"),
        best_ask_size: number(&event.payload, "best_ask_size"),
        ask_shares_at_or_below_limit: number(&event.payload, "ask_shares_at_or_below_limit"),
        last_trade_price: number(&event.payload, "last_trade_price"),
    })
}

fn binance_from_event(event: &TrajectoryEvent) -> Option<BinancePoint> {
    if event.event_type != "binance_quote" {
        return None;
    }
    Some(BinancePoint {
        observed_at_unix_ms: event.received_at_unix_ms,
        open: number(&event.payload, "open")?,
        close: number(&event.payload, "close")?,
    })
}

fn number(payload: &Value, key: &str) -> Option<f64> {
    payload.get(key).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

#[cfg(test)]
mod tests {
    use super::{analyze_outcome, ExitCostAssumptions, TrajectoryEvent};
    use serde_json::json;

    fn quote(at_ms: i64, ask: f64, bid: f64) -> TrajectoryEvent {
        TrajectoryEvent {
            received_at_unix_ms: at_ms,
            event_type: "quote".to_string(),
            payload: json!({
                "outcome":"UP",
                "observed_at_unix_ms":at_ms,
                "best_ask":ask,
                "best_bid":bid,
                "best_bid_size":10.0,
                "bid_shares_available":20.0,
                "sell_vwap_5":bid,
            }),
        }
    }

    fn costs() -> ExitCostAssumptions {
        ExitCostAssumptions {
            fee_bps: 0.0,
            slippage_bps: 0.0,
        }
    }

    #[test]
    fn strict_fill_rejects_half_and_accepts_price_below_half() {
        let events = vec![quote(1_000, 0.50, 0.49), quote(1_020, 0.49, 0.48)];

        let analysis = analyze_outcome(&events, "UP", 1_000, 10_000, costs());

        assert_eq!(
            analysis.strict_fill.map(|fill| fill.elapsed_from_signal_ms),
            Some(20)
        );
    }

    #[test]
    fn quote_at_signal_never_uses_a_future_observation() {
        let events = vec![quote(1_001, 0.49, 0.48)];

        let analysis = analyze_outcome(&events, "UP", 1_000, 10_000, costs());

        assert!(analysis.quote_at_signal.is_none());
        assert_eq!(
            analysis.strict_fill.map(|fill| fill.elapsed_from_signal_ms),
            Some(1)
        );
    }

    #[test]
    fn risk_checkpoint_is_relative_to_strict_fill() {
        let events = vec![
            quote(1_000, 0.49, 0.48),
            quote(15_000, 0.48, 0.44),
            quote(16_000, 0.47, 0.46),
        ];

        let analysis = analyze_outcome(&events, "UP", 1_000, 40_000, costs());
        let checkpoint = &analysis.risk.unwrap().checkpoints["t15s"];

        assert_eq!(checkpoint.best_bid, Some(0.46));
    }

    #[test]
    fn risk_checkpoint_after_resolution_is_unavailable() {
        let events = vec![quote(1_000, 0.49, 0.48)];

        let analysis = analyze_outcome(&events, "UP", 1_000, 10_000, costs());
        let checkpoint = &analysis.risk.unwrap().checkpoints["t15s"];

        assert!(checkpoint.unavailable_after_resolution);
    }

    #[test]
    fn adverse_threshold_records_recovery_to_half() {
        let events = vec![
            quote(1_000, 0.49, 0.48),
            quote(2_000, 0.44, 0.40),
            quote(3_000, 0.55, 0.52),
        ];

        let analysis = analyze_outcome(&events, "UP", 1_000, 4_000, costs());
        let threshold = &analysis.risk.unwrap().below_thresholds["below_0_45"];

        assert!(threshold.recovered_to_half);
    }
}
