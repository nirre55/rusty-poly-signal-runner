//! Window coordination and durable order state for the shared portfolio runner.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::portfolio::{MarketSlot, PortfolioSignal, PortfolioStrategy};
use crate::strategy::Prediction;

/// A completed feed evaluation waiting to be merged with its peer feeds.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    pub market: MarketSlot,
    pub entry_time_ms: i64,
    pub signals: Vec<PortfolioSignal>,
}

/// A complete set of feed evaluations for one Polymarket entry timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowBatch {
    pub entry_time_ms: i64,
    pub expected_markets: Vec<MarketSlot>,
    pub signals: Vec<PortfolioSignal>,
}

/// Result of inserting a feed evaluation into the synchronization barrier.
#[derive(Debug, Clone, PartialEq)]
pub enum CollectResult {
    Waiting,
    Duplicate,
    Ready(WindowBatch),
}

#[derive(Debug)]
struct PendingWindow {
    expected_markets: Vec<MarketSlot>,
    events: BTreeMap<MarketSlot, FeedEvent>,
    deadline: Instant,
}

/// Collects asynchronous Binance feeds without allowing a partial window to trade.
#[derive(Debug, Default)]
pub struct WindowCollector {
    pending: BTreeMap<i64, PendingWindow>,
}

impl WindowCollector {
    pub fn insert(
        &mut self,
        event: FeedEvent,
        grace: Duration,
        now: Instant,
    ) -> Result<CollectResult> {
        let entry_time_ms = event.entry_time_ms;
        let expected_markets = expected_markets_for_entry(entry_time_ms);
        if !expected_markets.contains(&event.market) {
            return Err(anyhow!(
                "feed {} inattendu pour entrée {}",
                event.market.key(),
                entry_time_ms
            ));
        }

        {
            let pending = self
                .pending
                .entry(entry_time_ms)
                .or_insert_with(|| PendingWindow {
                    expected_markets,
                    events: BTreeMap::new(),
                    deadline: now + grace,
                });
            if pending.events.contains_key(&event.market) {
                return Ok(CollectResult::Duplicate);
            }
            pending.events.insert(event.market, event);

            if pending.events.len() != pending.expected_markets.len() {
                return Ok(CollectResult::Waiting);
            }
        }

        let pending = self
            .pending
            .remove(&entry_time_ms)
            .ok_or_else(|| anyhow!("fenêtre prête introuvable"))?;
        Ok(CollectResult::Ready(WindowBatch {
            entry_time_ms,
            expected_markets: pending.expected_markets,
            signals: pending
                .events
                .into_values()
                .flat_map(|event| event.signals)
                .collect(),
        }))
    }

    /// Drops incomplete windows after the configured grace period and returns their timestamps.
    pub fn expire(&mut self, now: Instant) -> Vec<i64> {
        let expired: Vec<i64> = self
            .pending
            .iter()
            .filter_map(|(entry_time, window)| (window.deadline <= now).then_some(*entry_time))
            .collect();
        for entry_time in &expired {
            self.pending.remove(entry_time);
        }
        expired
    }
}

/// Returns the feeds that must be present at a given next-candle opening timestamp.
pub fn expected_markets_for_entry(entry_time_ms: i64) -> Vec<MarketSlot> {
    let mut expected = vec![MarketSlot::Btc5m, MarketSlot::Eth5m];
    if entry_time_ms.rem_euclid(MarketSlot::Btc15m.interval_millis()) == 0 {
        expected.push(MarketSlot::Btc15m);
        expected.push(MarketSlot::Eth15m);
    }
    expected
}

/// Lifecycle state persisted before and after every order request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortfolioOrderPhase {
    Submitting,
    Submitted,
    Filled,
    NoEntry,
    Failed,
    MissedValidation,
}

/// A combined order plus enough context to deduplicate and settle it after restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioOrder {
    pub key: String,
    pub market: MarketSlot,
    pub slug: String,
    pub prediction: Prediction,
    pub contributor_strategies: Vec<PortfolioStrategy>,
    pub contributor_count: usize,
    pub capital_usdc: f64,
    pub window_budget_usdc: f64,
    pub allocation_usdc: f64,
    pub amount_usdc: f64,
    pub minimum_usdc: f64,
    #[serde(default)]
    pub minimum_overrides_window: bool,
    pub target_close_time_ms: i64,
    pub created_at: DateTime<Utc>,
    pub order_id: Option<String>,
    pub order_status: String,
    pub acknowledged_amount_usdc: Option<f64>,
    pub limit_price: Option<f64>,
    pub execution_price: Option<f64>,
    pub size_matched: Option<f64>,
    pub phase: PortfolioOrderPhase,
    pub outcome: Option<String>,
}

/// Details returned by the CLOB after a successful combined-order submission.
#[derive(Debug, Clone)]
pub struct OrderAcknowledgement {
    pub order_id: String,
    pub order_status: String,
    pub acknowledged_amount_usdc: f64,
    pub limit_price: Option<f64>,
    pub execution_price: Option<f64>,
    pub size_matched: Option<f64>,
}

impl PortfolioOrder {
    pub fn is_pending(&self) -> bool {
        matches!(
            self.phase,
            PortfolioOrderPhase::Submitting | PortfolioOrderPhase::Submitted
        )
    }
}

/// Durable source of truth used to fail closed after a restart in an uncertain submission state.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PortfolioBook {
    orders: Vec<PortfolioOrder>,
}

impl PortfolioBook {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = fs::read_to_string(path)
            .with_context(|| format!("lecture de l'état portefeuille {}", path.display()))?;
        serde_json::from_str(&body)
            .with_context(|| format!("état portefeuille invalide {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("chemin d'état portefeuille sans dossier"))?;
        fs::create_dir_all(parent)?;
        let temp_path = path.with_extension("json.tmp");
        fs::write(&temp_path, serde_json::to_vec_pretty(self)?)?;
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(temp_path, path)?;
        Ok(())
    }

    pub fn has_seen(&self, key: &str) -> bool {
        self.orders.iter().any(|order| order.key == key)
    }

    pub fn begin_submission(&mut self, order: PortfolioOrder) -> Result<()> {
        if self.has_seen(&order.key) {
            return Err(anyhow!("ordre portefeuille déjà connu: {}", order.key));
        }
        self.orders.push(order);
        Ok(())
    }

    pub fn mark_submitted(
        &mut self,
        key: &str,
        acknowledgement: OrderAcknowledgement,
    ) -> Result<()> {
        let order = self.order_mut(key)?;
        order.order_id = Some(acknowledgement.order_id);
        order.order_status = acknowledgement.order_status;
        order.acknowledged_amount_usdc = Some(acknowledgement.acknowledged_amount_usdc);
        order.limit_price = acknowledgement.limit_price;
        order.execution_price = acknowledgement.execution_price;
        order.size_matched = acknowledgement.size_matched;
        order.phase = PortfolioOrderPhase::Submitted;
        Ok(())
    }

    pub fn mark_failed(&mut self, key: &str, status: &str) -> Result<()> {
        let order = self.order_mut(key)?;
        order.order_status = status.to_string();
        order.phase = PortfolioOrderPhase::Failed;
        Ok(())
    }

    pub fn pending_for_market(
        &self,
        market: MarketSlot,
        close_time_ms: i64,
    ) -> Vec<PortfolioOrder> {
        self.orders
            .iter()
            .filter(|order| {
                order.market == market
                    && order.phase == PortfolioOrderPhase::Submitted
                    && order.target_close_time_ms <= close_time_ms
            })
            .cloned()
            .collect()
    }

    pub fn mark_settlement(
        &mut self,
        key: &str,
        status: String,
        phase: PortfolioOrderPhase,
        outcome: Option<String>,
    ) -> Result<()> {
        let order = self.order_mut(key)?;
        order.order_status = status;
        order.phase = phase;
        order.outcome = outcome;
        Ok(())
    }

    pub fn orders(&self) -> &[PortfolioOrder] {
        &self.orders
    }

    fn order_mut(&mut self, key: &str) -> Result<&mut PortfolioOrder> {
        self.orders
            .iter_mut()
            .find(|order| order.key == key)
            .ok_or_else(|| anyhow!("ordre portefeuille introuvable: {}", key))
    }
}

/// Appends a structured event without mutating the durable order state.
pub fn append_event(path: &Path, event: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("chemin de journal portefeuille sans dossier"))?;
    fs::create_dir_all(parent)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, event)?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::fs;
    use std::time::{Duration, Instant};

    use super::{
        CollectResult, FeedEvent, PortfolioBook, PortfolioOrder, PortfolioOrderPhase,
        WindowCollector,
    };
    use crate::portfolio::{MarketSlot, PortfolioSignal, PortfolioStrategy};
    use crate::strategy::Prediction;

    fn event(market: MarketSlot, entry_time_ms: i64) -> FeedEvent {
        FeedEvent {
            market,
            entry_time_ms,
            signals: vec![PortfolioSignal {
                strategy: PortfolioStrategy::BollFade,
                market,
                prediction: Prediction::Up,
                signal_close_time: Utc::now(),
            }],
        }
    }

    #[test]
    fn five_minute_window_waits_for_btc_and_eth() {
        let mut collector = WindowCollector::default();
        let now = Instant::now();
        let entry_time_ms = 5 * 60 * 1_000;

        assert_eq!(
            collector
                .insert(
                    event(MarketSlot::Btc5m, entry_time_ms),
                    Duration::from_secs(1),
                    now
                )
                .expect("valid event"),
            CollectResult::Waiting
        );
        let ready = collector
            .insert(
                event(MarketSlot::Eth5m, entry_time_ms),
                Duration::from_secs(1),
                now,
            )
            .expect("valid event");

        match ready {
            CollectResult::Ready(batch) => {
                assert_eq!(batch.expected_markets.len(), 2);
                assert_eq!(batch.signals.len(), 2);
            }
            other => panic!("unexpected collection result: {other:?}"),
        }
    }

    #[test]
    fn fifteen_minute_window_requires_all_four_feeds() {
        let mut collector = WindowCollector::default();
        let now = Instant::now();
        let entry_time_ms = 15 * 60 * 1_000;
        for market in [MarketSlot::Btc5m, MarketSlot::Eth5m, MarketSlot::Btc15m] {
            assert_eq!(
                collector
                    .insert(event(market, entry_time_ms), Duration::from_secs(1), now)
                    .expect("valid event"),
                CollectResult::Waiting
            );
        }

        let ready = collector
            .insert(
                event(MarketSlot::Eth15m, entry_time_ms),
                Duration::from_secs(1),
                now,
            )
            .expect("valid event");
        match ready {
            CollectResult::Ready(batch) => assert_eq!(batch.expected_markets.len(), 4),
            other => panic!("unexpected collection result: {other:?}"),
        }
    }

    #[test]
    fn incomplete_window_expires_without_becoming_tradeable() {
        let mut collector = WindowCollector::default();
        let now = Instant::now();
        let entry_time_ms = 5 * 60 * 1_000;
        let _ = collector
            .insert(
                event(MarketSlot::Btc5m, entry_time_ms),
                Duration::from_millis(10),
                now,
            )
            .expect("valid event");

        assert_eq!(
            collector.expire(now + Duration::from_millis(11)),
            vec![entry_time_ms]
        );
    }

    #[test]
    fn durable_book_blocks_duplicate_after_restart() {
        let path = std::env::temp_dir().join(format!(
            "meche050-book-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut book = PortfolioBook::default();
        let order = PortfolioOrder {
            key: "portfolio:btc-5m:UP".to_string(),
            market: MarketSlot::Btc5m,
            slug: "btc-updown-5m-0".to_string(),
            prediction: Prediction::Up,
            contributor_strategies: vec![PortfolioStrategy::BollFade],
            contributor_count: 1,
            capital_usdc: 1_000.0,
            window_budget_usdc: 35.0,
            allocation_usdc: 12.0,
            amount_usdc: 12.0,
            minimum_usdc: 2.50,
            minimum_overrides_window: false,
            target_close_time_ms: 1,
            created_at: Utc::now(),
            order_id: None,
            order_status: "SUBMITTING".to_string(),
            acknowledged_amount_usdc: None,
            limit_price: None,
            execution_price: None,
            size_matched: None,
            phase: PortfolioOrderPhase::Submitting,
            outcome: None,
        };
        book.begin_submission(order).expect("new order");
        book.save(&path).expect("save book");

        let restored = PortfolioBook::load(&path).expect("load book");
        assert!(restored.has_seen("portfolio:btc-5m:UP"));
        fs::remove_file(path).expect("remove test state");
    }
}
