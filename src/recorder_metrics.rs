//! Compact order-book reconstruction and fixed-price execution metrics.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const METRICS_SCHEMA_VERSION: u32 = 2;

const PRICE_SCALE: f64 = 1_000_000.0;
const CHECKPOINTS_MS: [(&str, i64); 9] = [
    ("t15s", 15_000),
    ("t30s", 30_000),
    ("t60s", 60_000),
    ("t120s", 120_000),
    ("t180s", 180_000),
    ("t240s", 240_000),
    ("t300s", 300_000),
    ("t600s", 600_000),
    ("t900s", 900_000),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuoteSnapshot {
    pub observed_at_unix_ms: i64,
    pub best_bid: Option<f64>,
    pub best_bid_size: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_ask_size: Option<f64>,
    pub ask_shares_at_or_below_limit: Option<f64>,
    pub last_trade_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimedValue {
    pub observed_at_unix_ms: i64,
    pub elapsed_from_signal_ms: i64,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FillObservation {
    pub observed_at_unix_ms: i64,
    pub elapsed_from_signal_ms: i64,
    pub best_ask: f64,
    pub fillable_shares: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderCandidateMetric {
    pub amount_usdc: f64,
    pub required_shares: f64,
    pub first_fully_fillable: Option<FillObservation>,
    pub immediate_fak_fillable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutcomeMetrics {
    pub outcome: String,
    pub token_id: String,
    pub signal_ids: Vec<String>,
    pub signal_at_unix_ms: Option<i64>,
    pub quote_at_signal: Option<QuoteSnapshot>,
    pub quote_observation_count: u64,
    pub first_limit_touch: Option<FillObservation>,
    pub first_minimum_fillable: Option<FillObservation>,
    pub immediate_limit_touch: bool,
    pub immediate_minimum_fillable: bool,
    pub order_candidate: Option<OrderCandidateMetric>,
    pub min_best_ask: Option<TimedValue>,
    pub max_best_ask: Option<TimedValue>,
    pub min_best_bid: Option<TimedValue>,
    pub max_best_bid: Option<TimedValue>,
    pub max_fillable_shares_at_limit: Option<TimedValue>,
    pub checkpoints: BTreeMap<String, QuoteSnapshot>,
    pub last_quote: Option<QuoteSnapshot>,
    pub winning_outcome: Option<bool>,
    pub minimum_fill_result: String,
    pub minimum_fill_pnl_usdc: Option<f64>,
    pub order_fill_result: String,
    pub order_fill_pnl_usdc: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMetricsRecord {
    pub schema_version: u32,
    pub record_type: String,
    pub generated_at: DateTime<Utc>,
    pub source_format: String,
    pub analysis_complete: bool,
    pub session_id: String,
    pub market_slot: String,
    pub entry_time_ms: i64,
    pub slug: String,
    pub limit_price: f64,
    pub minimum_shares: f64,
    #[serde(default)]
    pub completion_status: String,
    #[serde(default)]
    pub gap_count: u64,
    #[serde(default)]
    pub reconnect_count: u64,
    pub resolution_winning_asset_id: Option<String>,
    pub resolution_winning_outcome: Option<String>,
    pub outcomes: Vec<OutcomeMetrics>,
    pub raw_stream_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactEvent {
    pub event_type: &'static str,
    pub asset_id: Option<String>,
    pub server_timestamp: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct SessionMetricContext {
    pub source_format: String,
    pub analysis_complete: bool,
    pub session_id: String,
    pub market_slot: String,
    pub entry_time_ms: i64,
    pub slug: String,
    pub winning_asset_id: Option<String>,
    pub winning_outcome: Option<String>,
    pub raw_stream_path: Option<String>,
    pub completion_status: String,
    pub gap_count: u64,
    pub reconnect_count: u64,
}

#[derive(Debug, Clone, Default)]
struct TokenBook {
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
    advertised_best_bid: Option<f64>,
    advertised_best_ask: Option<f64>,
    last_trade_price: Option<f64>,
    has_book_snapshot: bool,
}

impl TokenBook {
    fn apply_book(&mut self, payload: &Value) {
        self.bids = parse_levels(payload.get("bids"));
        self.asks = parse_levels(payload.get("asks"));
        self.has_book_snapshot = true;
        self.advertised_best_bid = self
            .bids
            .last_key_value()
            .map(|(price, _)| from_price_key(*price));
        self.advertised_best_ask = self
            .asks
            .first_key_value()
            .map(|(price, _)| from_price_key(*price));
    }

    fn apply_price_change(&mut self, change: &Value) {
        let Some(price) = numeric(change.get("price")) else {
            return;
        };
        let Some(size) = numeric(change.get("size")) else {
            return;
        };
        let Some(side) = change.get("side").and_then(Value::as_str) else {
            return;
        };
        let levels = if side.eq_ignore_ascii_case("BUY") {
            &mut self.bids
        } else if side.eq_ignore_ascii_case("SELL") {
            &mut self.asks
        } else {
            return;
        };
        let key = to_price_key(price);
        if size <= f64::EPSILON {
            levels.remove(&key);
        } else {
            levels.insert(key, size);
        }
        if let Some(best_bid) = numeric(change.get("best_bid")) {
            self.advertised_best_bid = Some(best_bid);
        }
        if let Some(best_ask) = numeric(change.get("best_ask")) {
            self.advertised_best_ask = Some(best_ask);
        }
    }

    fn apply_best_bid_ask(&mut self, payload: &Value) {
        self.advertised_best_bid = numeric(payload.get("best_bid"));
        self.advertised_best_ask = numeric(payload.get("best_ask"));
    }

    fn quote(&self, observed_at_unix_ms: i64, limit_price: f64) -> QuoteSnapshot {
        let book_best_bid = self.bids.last_key_value();
        let book_best_ask = self.asks.first_key_value();
        let best_bid = self
            .advertised_best_bid
            .or_else(|| book_best_bid.map(|(price, _)| from_price_key(*price)));
        let best_ask = self
            .advertised_best_ask
            .or_else(|| book_best_ask.map(|(price, _)| from_price_key(*price)));
        let best_bid_size = best_bid.and_then(|price| self.bids.get(&to_price_key(price)).copied());
        let best_ask_size = best_ask.and_then(|price| self.asks.get(&to_price_key(price)).copied());
        let ask_shares_at_or_below_limit = self.has_book_snapshot.then(|| {
            self.asks
                .range(..=to_price_key(limit_price))
                .map(|(_, size)| *size)
                .sum()
        });
        QuoteSnapshot {
            observed_at_unix_ms,
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            ask_shares_at_or_below_limit,
            last_trade_price: self.last_trade_price,
        }
    }
}

#[derive(Debug, Clone)]
struct OutcomeTracker {
    outcome: &'static str,
    token_id: String,
    signal_ids: BTreeSet<String>,
    signal_at_unix_ms: Option<i64>,
    quote_at_signal: Option<QuoteSnapshot>,
    quote_observation_count: u64,
    first_limit_touch: Option<FillObservation>,
    first_minimum_fillable: Option<FillObservation>,
    immediate_limit_touch: bool,
    immediate_minimum_fillable: bool,
    order_candidate: Option<OrderCandidateMetric>,
    min_best_ask: Option<TimedValue>,
    max_best_ask: Option<TimedValue>,
    min_best_bid: Option<TimedValue>,
    max_best_bid: Option<TimedValue>,
    max_fillable_shares_at_limit: Option<TimedValue>,
    checkpoints: BTreeMap<String, QuoteSnapshot>,
    last_quote: Option<QuoteSnapshot>,
    depth_history: Vec<(i64, f64, f64)>,
}

impl OutcomeTracker {
    fn new(outcome: &'static str, token_id: String) -> Self {
        Self {
            outcome,
            token_id,
            signal_ids: BTreeSet::new(),
            signal_at_unix_ms: None,
            quote_at_signal: None,
            quote_observation_count: 0,
            first_limit_touch: None,
            first_minimum_fillable: None,
            immediate_limit_touch: false,
            immediate_minimum_fillable: false,
            order_candidate: None,
            min_best_ask: None,
            max_best_ask: None,
            min_best_bid: None,
            max_best_bid: None,
            max_fillable_shares_at_limit: None,
            checkpoints: BTreeMap::new(),
            last_quote: None,
            depth_history: Vec::new(),
        }
    }

    fn activate(
        &mut self,
        signal_ids: impl IntoIterator<Item = String>,
        at_ms: i64,
        quote: QuoteSnapshot,
        limit_price: f64,
        minimum_shares: f64,
    ) {
        self.signal_ids.extend(signal_ids);
        if self.signal_at_unix_ms.is_some() {
            return;
        }
        self.signal_at_unix_ms = Some(at_ms);
        self.quote_at_signal = Some(quote.clone());
        self.observe(quote, limit_price, minimum_shares);
        self.immediate_limit_touch = self
            .first_limit_touch
            .as_ref()
            .is_some_and(|touch| touch.elapsed_from_signal_ms == 0);
        self.immediate_minimum_fillable = self
            .first_minimum_fillable
            .as_ref()
            .is_some_and(|fill| fill.elapsed_from_signal_ms == 0);
    }

    fn set_order_candidate(&mut self, amount_usdc: f64, limit_price: f64) {
        if amount_usdc <= 0.0 || limit_price <= 0.0 {
            return;
        }
        let required_shares = amount_usdc / limit_price;
        let first = self.depth_history.iter().find_map(|(at_ms, ask, depth)| {
            (*ask <= limit_price && *depth + 1e-9 >= required_shares)
                .then(|| self.fill_observation(*at_ms, *ask, Some(*depth)))
                .flatten()
        });
        let immediate = first
            .as_ref()
            .is_some_and(|fill| fill.elapsed_from_signal_ms == 0);
        self.order_candidate = Some(OrderCandidateMetric {
            amount_usdc,
            required_shares,
            first_fully_fillable: first,
            immediate_fak_fillable: immediate,
        });
    }

    fn observe(&mut self, quote: QuoteSnapshot, limit_price: f64, minimum_shares: f64) {
        let Some(signal_at) = self.signal_at_unix_ms else {
            return;
        };
        let elapsed = (quote.observed_at_unix_ms - signal_at).max(0);
        self.quote_observation_count += 1;

        if let Some(previous) = self.last_quote.as_ref() {
            for (name, checkpoint_ms) in CHECKPOINTS_MS {
                if elapsed >= checkpoint_ms && !self.checkpoints.contains_key(name) {
                    self.checkpoints.insert(name.to_string(), previous.clone());
                }
            }
        }

        if let Some(best_ask) = quote.best_ask {
            update_min(
                &mut self.min_best_ask,
                best_ask,
                quote.observed_at_unix_ms,
                elapsed,
            );
            update_max(
                &mut self.max_best_ask,
                best_ask,
                quote.observed_at_unix_ms,
                elapsed,
            );
            if best_ask <= limit_price && self.first_limit_touch.is_none() {
                self.first_limit_touch = self.fill_observation(
                    quote.observed_at_unix_ms,
                    best_ask,
                    quote.ask_shares_at_or_below_limit,
                );
            }
            if let Some(depth) = quote.ask_shares_at_or_below_limit {
                self.depth_history
                    .push((quote.observed_at_unix_ms, best_ask, depth));
                update_max(
                    &mut self.max_fillable_shares_at_limit,
                    depth,
                    quote.observed_at_unix_ms,
                    elapsed,
                );
                if best_ask <= limit_price
                    && depth + 1e-9 >= minimum_shares
                    && self.first_minimum_fillable.is_none()
                {
                    self.first_minimum_fillable =
                        self.fill_observation(quote.observed_at_unix_ms, best_ask, Some(depth));
                }
                if let Some(candidate) = self.order_candidate.as_mut() {
                    if best_ask <= limit_price
                        && depth + 1e-9 >= candidate.required_shares
                        && candidate.first_fully_fillable.is_none()
                    {
                        candidate.first_fully_fillable = Some(FillObservation {
                            observed_at_unix_ms: quote.observed_at_unix_ms,
                            elapsed_from_signal_ms: elapsed,
                            best_ask,
                            fillable_shares: Some(depth),
                        });
                        candidate.immediate_fak_fillable = elapsed == 0;
                    }
                }
            }
        }
        if let Some(best_bid) = quote.best_bid {
            update_min(
                &mut self.min_best_bid,
                best_bid,
                quote.observed_at_unix_ms,
                elapsed,
            );
            update_max(
                &mut self.max_best_bid,
                best_bid,
                quote.observed_at_unix_ms,
                elapsed,
            );
        }
        self.last_quote = Some(quote);
    }

    fn fill_observation(
        &self,
        observed_at_unix_ms: i64,
        best_ask: f64,
        fillable_shares: Option<f64>,
    ) -> Option<FillObservation> {
        let signal_at = self.signal_at_unix_ms?;
        Some(FillObservation {
            observed_at_unix_ms,
            elapsed_from_signal_ms: (observed_at_unix_ms - signal_at).max(0),
            best_ask,
            fillable_shares,
        })
    }

    fn finish(
        self,
        winning_asset_id: Option<&str>,
        limit_price: f64,
        minimum_shares: f64,
    ) -> OutcomeMetrics {
        let winning_outcome = winning_asset_id.map(|winner| winner == self.token_id);
        let minimum_filled = self.first_minimum_fillable.is_some();
        let order_filled = self
            .order_candidate
            .as_ref()
            .is_some_and(|candidate| candidate.first_fully_fillable.is_some());
        let (minimum_fill_result, minimum_fill_pnl_usdc) =
            hypothetical_result(minimum_filled, winning_outcome, minimum_shares, limit_price);
        let (order_fill_result, order_fill_pnl_usdc) = self.order_candidate.as_ref().map_or_else(
            || ("NO_ORDER_CANDIDATE".to_string(), None),
            |candidate| {
                hypothetical_result(
                    order_filled,
                    winning_outcome,
                    candidate.required_shares,
                    limit_price,
                )
            },
        );
        OutcomeMetrics {
            outcome: self.outcome.to_string(),
            token_id: self.token_id,
            signal_ids: self.signal_ids.into_iter().collect(),
            signal_at_unix_ms: self.signal_at_unix_ms,
            quote_at_signal: self.quote_at_signal,
            quote_observation_count: self.quote_observation_count,
            first_limit_touch: self.first_limit_touch,
            first_minimum_fillable: self.first_minimum_fillable,
            immediate_limit_touch: self.immediate_limit_touch,
            immediate_minimum_fillable: self.immediate_minimum_fillable,
            order_candidate: self.order_candidate,
            min_best_ask: self.min_best_ask,
            max_best_ask: self.max_best_ask,
            min_best_bid: self.min_best_bid,
            max_best_bid: self.max_best_bid,
            max_fillable_shares_at_limit: self.max_fillable_shares_at_limit,
            checkpoints: self.checkpoints,
            last_quote: self.last_quote,
            winning_outcome,
            minimum_fill_result,
            minimum_fill_pnl_usdc,
            order_fill_result,
            order_fill_pnl_usdc,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionAnalyzer {
    limit_price: f64,
    minimum_shares: f64,
    up_token_id: String,
    down_token_id: String,
    books: BTreeMap<String, TokenBook>,
    trackers: BTreeMap<String, OutcomeTracker>,
    last_compact_quote: BTreeMap<String, QuoteSnapshot>,
}

impl SessionAnalyzer {
    pub fn new(
        up_token_id: impl Into<String>,
        down_token_id: impl Into<String>,
        limit_price: f64,
        minimum_shares: f64,
    ) -> Self {
        let up_token_id = up_token_id.into();
        let down_token_id = down_token_id.into();
        let mut books = BTreeMap::new();
        books.insert(up_token_id.clone(), TokenBook::default());
        books.insert(down_token_id.clone(), TokenBook::default());
        let mut trackers = BTreeMap::new();
        trackers.insert(
            "UP".to_string(),
            OutcomeTracker::new("UP", up_token_id.clone()),
        );
        trackers.insert(
            "DOWN".to_string(),
            OutcomeTracker::new("DOWN", down_token_id.clone()),
        );
        Self {
            limit_price,
            minimum_shares,
            up_token_id,
            down_token_id,
            books,
            trackers,
            last_compact_quote: BTreeMap::new(),
        }
    }

    pub fn activate(
        &mut self,
        prediction: &str,
        signal_ids: impl IntoIterator<Item = String>,
        at_ms: i64,
    ) -> Option<CompactEvent> {
        let outcome = normalize_outcome(prediction)?;
        let token_id = self.token_id(outcome).to_string();
        let quote = self.books.get(&token_id)?.quote(at_ms, self.limit_price);
        self.trackers.get_mut(outcome)?.activate(
            signal_ids,
            at_ms,
            quote.clone(),
            self.limit_price,
            self.minimum_shares,
        );
        Some(compact_quote_event(
            outcome,
            &token_id,
            "signal_snapshot",
            &quote,
        ))
    }

    /// Restores an activation from the compact snapshot written at signal time.
    pub fn activate_from_compact_snapshot(
        &mut self,
        prediction: &str,
        signal_ids: impl IntoIterator<Item = String>,
        at_ms: i64,
        payload: &Value,
    ) -> bool {
        let Some(outcome) = normalize_outcome(prediction) else {
            return false;
        };
        let Some(quote) = quote_from_compact_payload(payload, at_ms) else {
            return false;
        };
        if let Some(tracker) = self.trackers.get_mut(outcome) {
            tracker.activate(
                signal_ids,
                at_ms,
                quote,
                self.limit_price,
                self.minimum_shares,
            );
            return true;
        }
        false
    }

    /// Replays a deduplicated compact quote after a process restart or backfill.
    pub fn process_compact_quote(&mut self, payload: &Value, received_at_ms: i64) -> bool {
        let Some(outcome) = payload
            .get("outcome")
            .and_then(Value::as_str)
            .and_then(normalize_outcome)
        else {
            return false;
        };
        let Some(quote) = quote_from_compact_payload(payload, received_at_ms) else {
            return false;
        };
        if let Some(tracker) = self.trackers.get_mut(outcome) {
            tracker.observe(quote, self.limit_price, self.minimum_shares);
            return true;
        }
        false
    }

    pub fn set_order_candidate(&mut self, prediction: &str, amount_usdc: f64) {
        let Some(outcome) = normalize_outcome(prediction) else {
            return;
        };
        if let Some(tracker) = self.trackers.get_mut(outcome) {
            tracker.set_order_candidate(amount_usdc, self.limit_price);
        }
    }

    pub fn process_payload(&mut self, payload: &Value, received_at_ms: i64) -> Vec<CompactEvent> {
        let mut events = Vec::new();
        for_each_payload_event(payload, &mut |event| {
            let kind = event
                .get("event_type")
                .or_else(|| event.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match kind {
                "book" => {
                    if let Some(asset_id) = event.get("asset_id").and_then(Value::as_str) {
                        if let Some(book) = self.books.get_mut(asset_id) {
                            book.apply_book(event);
                            self.observe_asset(asset_id, received_at_ms, &mut events);
                        }
                    }
                }
                "price_change" => {
                    let mut changed_assets = BTreeSet::new();
                    if let Some(changes) = event.get("price_changes").and_then(Value::as_array) {
                        for change in changes {
                            let Some(asset_id) = change.get("asset_id").and_then(Value::as_str)
                            else {
                                continue;
                            };
                            if let Some(book) = self.books.get_mut(asset_id) {
                                book.apply_price_change(change);
                                changed_assets.insert(asset_id.to_string());
                            }
                        }
                    }
                    for asset_id in changed_assets {
                        self.observe_asset(&asset_id, received_at_ms, &mut events);
                    }
                }
                "best_bid_ask" => {
                    if let Some(asset_id) = event.get("asset_id").and_then(Value::as_str) {
                        if let Some(book) = self.books.get_mut(asset_id) {
                            book.apply_best_bid_ask(event);
                            self.observe_asset(asset_id, received_at_ms, &mut events);
                        }
                    }
                }
                "last_trade_price" => {
                    if let Some(asset_id) = event.get("asset_id").and_then(Value::as_str) {
                        if let Some(book) = self.books.get_mut(asset_id) {
                            book.last_trade_price = numeric(event.get("price"));
                            self.observe_asset(asset_id, received_at_ms, &mut events);
                        }
                    }
                }
                _ => {}
            }
        });
        events
    }

    pub fn finish(self, context: SessionMetricContext) -> SessionMetricsRecord {
        let winning_asset_id = context.winning_asset_id.as_deref();
        let outcomes = self
            .trackers
            .into_values()
            .map(|tracker| tracker.finish(winning_asset_id, self.limit_price, self.minimum_shares))
            .collect();
        SessionMetricsRecord {
            schema_version: METRICS_SCHEMA_VERSION,
            record_type: "SESSION_METRICS".to_string(),
            generated_at: Utc::now(),
            source_format: context.source_format,
            analysis_complete: context.analysis_complete,
            session_id: context.session_id,
            market_slot: context.market_slot,
            entry_time_ms: context.entry_time_ms,
            slug: context.slug,
            limit_price: self.limit_price,
            minimum_shares: self.minimum_shares,
            completion_status: context.completion_status,
            gap_count: context.gap_count,
            reconnect_count: context.reconnect_count,
            resolution_winning_asset_id: context.winning_asset_id,
            resolution_winning_outcome: context.winning_outcome,
            outcomes,
            raw_stream_path: context.raw_stream_path,
        }
    }

    fn token_id(&self, outcome: &str) -> &str {
        if outcome == "UP" {
            &self.up_token_id
        } else {
            &self.down_token_id
        }
    }

    fn outcome_for_asset(&self, asset_id: &str) -> Option<&'static str> {
        if asset_id == self.up_token_id {
            Some("UP")
        } else if asset_id == self.down_token_id {
            Some("DOWN")
        } else {
            None
        }
    }

    fn observe_asset(
        &mut self,
        asset_id: &str,
        received_at_ms: i64,
        events: &mut Vec<CompactEvent>,
    ) {
        let Some(outcome) = self.outcome_for_asset(asset_id) else {
            return;
        };
        let Some(book) = self.books.get(asset_id) else {
            return;
        };
        let quote = book.quote(received_at_ms, self.limit_price);
        if let Some(tracker) = self.trackers.get_mut(outcome) {
            tracker.observe(quote.clone(), self.limit_price, self.minimum_shares);
        }
        let changed = self
            .last_compact_quote
            .get(asset_id)
            .is_none_or(|previous| quote_values_changed(previous, &quote));
        if changed {
            self.last_compact_quote
                .insert(asset_id.to_string(), quote.clone());
            events.push(compact_quote_event(outcome, asset_id, "quote", &quote));
        }
    }
}

fn quote_from_compact_payload(payload: &Value, observed_at_unix_ms: i64) -> Option<QuoteSnapshot> {
    payload.get("outcome").and_then(Value::as_str)?;
    Some(QuoteSnapshot {
        observed_at_unix_ms,
        best_bid: numeric(payload.get("best_bid")),
        best_bid_size: numeric(payload.get("best_bid_size")),
        best_ask: numeric(payload.get("best_ask")),
        best_ask_size: numeric(payload.get("best_ask_size")),
        ask_shares_at_or_below_limit: numeric(payload.get("ask_shares_at_or_below_limit")),
        last_trade_price: numeric(payload.get("last_trade_price")),
    })
}

fn compact_quote_event(
    outcome: &str,
    asset_id: &str,
    event_type: &'static str,
    quote: &QuoteSnapshot,
) -> CompactEvent {
    CompactEvent {
        event_type,
        asset_id: Some(asset_id.to_string()),
        server_timestamp: None,
        payload: json!({
            "outcome": outcome,
            "asset_id": asset_id,
            "best_bid": quote.best_bid,
            "best_bid_size": quote.best_bid_size,
            "best_ask": quote.best_ask,
            "best_ask_size": quote.best_ask_size,
            "ask_shares_at_or_below_limit": quote.ask_shares_at_or_below_limit,
            "last_trade_price": quote.last_trade_price,
        }),
    }
}

fn quote_values_changed(previous: &QuoteSnapshot, current: &QuoteSnapshot) -> bool {
    previous.best_bid != current.best_bid
        || previous.best_bid_size != current.best_bid_size
        || previous.best_ask != current.best_ask
        || previous.best_ask_size != current.best_ask_size
        || previous.ask_shares_at_or_below_limit != current.ask_shares_at_or_below_limit
        || previous.last_trade_price != current.last_trade_price
}

fn hypothetical_result(
    filled: bool,
    winning_outcome: Option<bool>,
    shares: f64,
    limit_price: f64,
) -> (String, Option<f64>) {
    if !filled {
        return ("NOT_FILLED".to_string(), Some(0.0));
    }
    match winning_outcome {
        Some(true) => ("WIN".to_string(), Some(shares * (1.0 - limit_price))),
        Some(false) => ("LOSS".to_string(), Some(-(shares * limit_price))),
        None => ("PENDING".to_string(), None),
    }
}

fn update_min(slot: &mut Option<TimedValue>, value: f64, at_ms: i64, elapsed_ms: i64) {
    if slot.as_ref().is_none_or(|current| value < current.value) {
        *slot = Some(TimedValue {
            observed_at_unix_ms: at_ms,
            elapsed_from_signal_ms: elapsed_ms,
            value,
        });
    }
}

fn update_max(slot: &mut Option<TimedValue>, value: f64, at_ms: i64, elapsed_ms: i64) {
    if slot.as_ref().is_none_or(|current| value > current.value) {
        *slot = Some(TimedValue {
            observed_at_unix_ms: at_ms,
            elapsed_from_signal_ms: elapsed_ms,
            value,
        });
    }
}

fn parse_levels(value: Option<&Value>) -> BTreeMap<i64, f64> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|level| {
            let price = numeric(level.get("price"))?;
            let size = numeric(level.get("size"))?;
            (size > f64::EPSILON).then_some((to_price_key(price), size))
        })
        .collect()
}

fn numeric(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(value) => value.parse().ok(),
        Value::Number(value) => value.as_f64(),
        _ => None,
    }
}

fn normalize_outcome(value: &str) -> Option<&'static str> {
    if value.eq_ignore_ascii_case("UP") {
        Some("UP")
    } else if value.eq_ignore_ascii_case("DOWN") {
        Some("DOWN")
    } else {
        None
    }
}

fn for_each_payload_event(payload: &Value, callback: &mut impl FnMut(&Value)) {
    match payload {
        Value::Array(events) => {
            for event in events {
                for_each_payload_event(event, callback);
            }
        }
        Value::Object(_) => callback(payload),
        _ => {}
    }
}

fn to_price_key(price: f64) -> i64 {
    (price * PRICE_SCALE).round() as i64
}

fn from_price_key(price: i64) -> f64 {
    price as f64 / PRICE_SCALE
}

#[cfg(test)]
mod tests {
    use super::{SessionAnalyzer, SessionMetricContext};
    use serde_json::json;

    fn analyzer() -> SessionAnalyzer {
        SessionAnalyzer::new("up-token", "down-token", 0.50, 5.0)
    }

    #[test]
    fn book_and_price_changes_reconstruct_fillable_depth() {
        let mut analyzer = analyzer();
        analyzer.process_payload(
            &json!({
                "event_type":"book",
                "asset_id":"up-token",
                "bids":[{"price":"0.48","size":"9"}],
                "asks":[{"price":"0.50","size":"4"},{"price":"0.51","size":"10"}]
            }),
            900,
        );
        analyzer.activate("UP", ["signal-1".to_string()], 1_000);
        analyzer.set_order_candidate("UP", 2.50);
        analyzer.process_payload(
            &json!({
                "event_type":"price_change",
                "price_changes":[{
                    "asset_id":"up-token",
                    "price":"0.50",
                    "size":"7",
                    "side":"SELL",
                    "best_bid":"0.48",
                    "best_ask":"0.50"
                }]
            }),
            1_250,
        );
        let metrics = analyzer.finish(SessionMetricContext {
            source_format: "test".to_string(),
            analysis_complete: true,
            session_id: "session".to_string(),
            market_slot: "btc_5m".to_string(),
            entry_time_ms: 1_000,
            slug: "slug".to_string(),
            winning_asset_id: Some("up-token".to_string()),
            winning_outcome: Some("Up".to_string()),
            raw_stream_path: None,
            completion_status: "RESOLVED_COMPLETE".to_string(),
            gap_count: 0,
            reconnect_count: 0,
        });
        let up = metrics
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome == "UP")
            .unwrap();
        assert_eq!(
            up.first_minimum_fillable
                .as_ref()
                .map(|fill| fill.elapsed_from_signal_ms),
            Some(250)
        );
        assert_eq!(up.order_fill_result, "WIN");
        assert_eq!(up.order_fill_pnl_usdc, Some(2.5));
    }

    #[test]
    fn immediate_fak_requires_enough_depth_at_signal() {
        let mut analyzer = analyzer();
        analyzer.process_payload(
            &json!({
                "event_type":"book",
                "asset_id":"down-token",
                "bids":[],
                "asks":[{"price":"0.49","size":"8"}]
            }),
            900,
        );
        analyzer.activate("DOWN", ["signal-1".to_string()], 1_000);
        analyzer.set_order_candidate("DOWN", 2.50);
        let metrics = analyzer.finish(SessionMetricContext {
            source_format: "test".to_string(),
            analysis_complete: true,
            session_id: "session".to_string(),
            market_slot: "eth_5m".to_string(),
            entry_time_ms: 1_000,
            slug: "slug".to_string(),
            winning_asset_id: Some("up-token".to_string()),
            winning_outcome: Some("Up".to_string()),
            raw_stream_path: None,
            completion_status: "RESOLVED_COMPLETE".to_string(),
            gap_count: 0,
            reconnect_count: 0,
        });
        let down = metrics
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome == "DOWN")
            .unwrap();
        assert!(down.immediate_limit_touch);
        assert!(down.immediate_minimum_fillable);
        assert!(down
            .order_candidate
            .as_ref()
            .is_some_and(|candidate| candidate.immediate_fak_fillable));
        assert_eq!(down.order_fill_result, "LOSS");
        assert_eq!(down.order_fill_pnl_usdc, Some(-2.5));
    }

    #[test]
    fn unchanged_quotes_are_not_persisted_twice() {
        let mut analyzer = analyzer();
        let payload = json!({
            "event_type":"best_bid_ask",
            "asset_id":"up-token",
            "best_bid":"0.48",
            "best_ask":"0.52"
        });
        assert_eq!(analyzer.process_payload(&payload, 1_000).len(), 1);
        assert!(analyzer.process_payload(&payload, 1_001).is_empty());
    }

    #[test]
    fn compact_snapshots_rebuild_fill_metrics_after_restart() {
        let mut analyzer = analyzer();
        assert!(analyzer.activate_from_compact_snapshot(
            "UP",
            ["signal-1".to_string()],
            1_000,
            &json!({
                "outcome":"UP",
                "asset_id":"up-token",
                "best_bid":0.48,
                "best_ask":0.50,
                "ask_shares_at_or_below_limit":4.0
            }),
        ));
        assert!(analyzer.process_compact_quote(
            &json!({
                "outcome":"UP",
                "asset_id":"up-token",
                "best_bid":0.49,
                "best_ask":0.50,
                "ask_shares_at_or_below_limit":6.0
            }),
            1_250,
        ));
        analyzer.set_order_candidate("UP", 2.50);

        let metrics = analyzer.finish(SessionMetricContext {
            source_format: "test-resume".to_string(),
            analysis_complete: true,
            session_id: "session".to_string(),
            market_slot: "btc_5m".to_string(),
            entry_time_ms: 1_000,
            slug: "slug".to_string(),
            winning_asset_id: Some("up-token".to_string()),
            winning_outcome: Some("Up".to_string()),
            raw_stream_path: None,
            completion_status: "RESOLVED_WITH_GAPS".to_string(),
            gap_count: 1,
            reconnect_count: 0,
        });
        let up = metrics
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome == "UP")
            .unwrap();
        assert_eq!(
            up.first_minimum_fillable
                .as_ref()
                .map(|fill| fill.elapsed_from_signal_ms),
            Some(250)
        );
        assert_eq!(up.order_fill_result, "WIN");
    }
}
