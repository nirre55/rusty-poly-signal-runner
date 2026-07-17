use std::sync::atomic::{AtomicU64, Ordering};

use crate::trading_runtime::ClosedCandleAction;

#[derive(Debug, Default)]
pub struct RuntimeMetrics {
    no_signal: AtomicU64,
    audit_failed: AtomicU64,
    filtered: AtomicU64,
    duplicate_signal: AtomicU64,
    market_resolve_failed: AtomicU64,
    order_failed: AtomicU64,
    order_placed: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeMetricsSnapshot {
    pub no_signal: u64,
    pub audit_failed: u64,
    pub filtered: u64,
    pub duplicate_signal: u64,
    pub market_resolve_failed: u64,
    pub order_failed: u64,
    pub order_placed: u64,
}

impl RuntimeMetrics {
    pub fn record(&self, action: &ClosedCandleAction) {
        match action {
            ClosedCandleAction::NoSignal => {
                self.no_signal.fetch_add(1, Ordering::Relaxed);
            }
            ClosedCandleAction::AuditFailed => {
                self.audit_failed.fetch_add(1, Ordering::Relaxed);
            }
            ClosedCandleAction::Filtered => {
                self.filtered.fetch_add(1, Ordering::Relaxed);
            }
            ClosedCandleAction::DuplicateSignal => {
                self.duplicate_signal.fetch_add(1, Ordering::Relaxed);
            }
            ClosedCandleAction::MarketResolveFailed => {
                self.market_resolve_failed.fetch_add(1, Ordering::Relaxed);
            }
            ClosedCandleAction::OrderFailed => {
                self.order_failed.fetch_add(1, Ordering::Relaxed);
            }
            ClosedCandleAction::OrderPlaced { .. } => {
                self.order_placed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self) -> RuntimeMetricsSnapshot {
        RuntimeMetricsSnapshot {
            no_signal: self.no_signal.load(Ordering::Relaxed),
            audit_failed: self.audit_failed.load(Ordering::Relaxed),
            filtered: self.filtered.load(Ordering::Relaxed),
            duplicate_signal: self.duplicate_signal.load(Ordering::Relaxed),
            market_resolve_failed: self.market_resolve_failed.load(Ordering::Relaxed),
            order_failed: self.order_failed.load(Ordering::Relaxed),
            order_placed: self.order_placed.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeMetrics;
    use crate::trading_runtime::ClosedCandleAction;

    #[test]
    fn records_actions() {
        let metrics = RuntimeMetrics::default();
        metrics.record(&ClosedCandleAction::NoSignal);
        metrics.record(&ClosedCandleAction::OrderPlaced {
            trade_id: "t".to_string(),
            signal_key: "s".to_string(),
        });

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.no_signal, 1);
        assert_eq!(snapshot.order_placed, 1);
    }
}
