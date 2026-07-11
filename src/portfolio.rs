//! Shared portfolio primitives for the Mèche 0,50 runner.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::strategy::Prediction;

const CENTS_PER_USDC: f64 = 100.0;

/// One of the four Binance/Polymarket feeds managed by the portfolio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MarketSlot {
    Btc5m,
    Eth5m,
    Btc15m,
    Eth15m,
}

impl MarketSlot {
    pub const ALL: [Self; 4] = [Self::Btc5m, Self::Eth5m, Self::Btc15m, Self::Eth15m];

    pub fn key(self) -> &'static str {
        match self {
            Self::Btc5m => "btc_5m",
            Self::Eth5m => "eth_5m",
            Self::Btc15m => "btc_15m",
            Self::Eth15m => "eth_15m",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Btc5m | Self::Btc15m => "btcusdt",
            Self::Eth5m | Self::Eth15m => "ethusdt",
        }
    }

    pub fn interval(self) -> &'static str {
        match self {
            Self::Btc5m | Self::Eth5m => "5m",
            Self::Btc15m | Self::Eth15m => "15m",
        }
    }

    pub fn slug_prefix(self) -> &'static str {
        match self {
            Self::Btc5m => "btc-updown-5m",
            Self::Eth5m => "eth-updown-5m",
            Self::Btc15m => "btc-updown-15m",
            Self::Eth15m => "eth-updown-15m",
        }
    }

    pub fn interval_millis(self) -> i64 {
        match self {
            Self::Btc5m | Self::Eth5m => 5 * 60 * 1_000,
            Self::Btc15m | Self::Eth15m => 15 * 60 * 1_000,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "btc_5m" => Some(Self::Btc5m),
            "eth_5m" => Some(Self::Eth5m),
            "btc_15m" => Some(Self::Btc15m),
            "eth_15m" => Some(Self::Eth15m),
            _ => None,
        }
    }
}

/// A final strategy output that may be toggled independently for each feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PortfolioStrategy {
    BollFade,
    StreakRsi,
    TrioVote2,
    ReversalPro,
}

impl PortfolioStrategy {
    pub const ALL: [Self; 4] = [
        Self::BollFade,
        Self::StreakRsi,
        Self::TrioVote2,
        Self::ReversalPro,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::BollFade => "boll_fade",
            Self::StreakRsi => "streak_rsi",
            Self::TrioVote2 => "trio_vote2",
            Self::ReversalPro => "reversal_pro",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "boll_fade" => Some(Self::BollFade),
            "streak_rsi" => Some(Self::StreakRsi),
            "trio_vote2" => Some(Self::TrioVote2),
            "reversal_pro" => Some(Self::ReversalPro),
            _ => None,
        }
    }
}

/// The final signal submitted to the shared portfolio allocator.
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioSignal {
    pub strategy: PortfolioStrategy,
    pub market: MarketSlot,
    pub prediction: Prediction,
    pub signal_close_time: DateTime<Utc>,
}

/// Signals that refer to the same Polymarket market side become one combined order.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderGroup {
    pub market: MarketSlot,
    pub prediction: Prediction,
    pub contributors: Vec<PortfolioSignal>,
}

/// An order amount after per-signal allocation and the mandatory market minimum.
#[derive(Debug, Clone, PartialEq)]
pub struct SizedOrder {
    pub group: OrderGroup,
    pub allocation_usdc: f64,
    pub minimum_usdc: f64,
    pub amount_usdc: f64,
}

/// Exact sizing parameters for the shared portfolio window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizingRule {
    pub window_budget_pct: f64,
    pub signal_cap_pct: f64,
}

impl SizingRule {
    pub fn validate(self) -> Result<()> {
        if !(0.0 < self.window_budget_pct && self.window_budget_pct <= 100.0) {
            return Err(anyhow!(
                "PORTFOLIO_WINDOW_BUDGET_PCT={} invalide",
                self.window_budget_pct
            ));
        }
        if !(0.0 < self.signal_cap_pct && self.signal_cap_pct <= self.window_budget_pct) {
            return Err(anyhow!(
                "PORTFOLIO_SIGNAL_CAP_PCT={} invalide pour budget {}",
                self.signal_cap_pct,
                self.window_budget_pct
            ));
        }
        Ok(())
    }
}

/// The atomically decided result for one common entry window.
#[derive(Debug, Clone, PartialEq)]
pub enum SizingDecision {
    Empty,
    Submit {
        capital_usdc: f64,
        window_budget_usdc: f64,
        per_signal_usdc: f64,
        orders: Vec<SizedOrder>,
        total_usdc: f64,
    },
    SkipMinimumsExceedBudget {
        window_budget_usdc: f64,
        total_usdc: f64,
    },
}

/// Runtime configuration specific to the shared Mèche portfolio.
#[derive(Debug, Clone)]
pub struct PortfolioSettings {
    pub sizing: SizingRule,
    pub sync_grace: Duration,
    pub enabled_path: PathBuf,
    pub dry_run_capital_usdc: f64,
}

impl PortfolioSettings {
    /// Loads the coordinator-only environment variables after `Config::from_env` loaded the file.
    pub fn from_env() -> Result<Self> {
        let sizing = SizingRule {
            window_budget_pct: parse_env_f64("PORTFOLIO_WINDOW_BUDGET_PCT", 3.5)?,
            signal_cap_pct: parse_env_f64("PORTFOLIO_SIGNAL_CAP_PCT", 1.2)?,
        };
        sizing.validate()?;

        let sync_grace_ms = parse_env_u64("PORTFOLIO_SYNC_GRACE_MS", 1_250)?;
        if sync_grace_ms == 0 {
            return Err(anyhow!("PORTFOLIO_SYNC_GRACE_MS doit être > 0"));
        }

        let dry_run_capital_usdc = parse_env_f64("PORTFOLIO_DRY_RUN_CAPITAL_USDC", 1_000.0)?;
        if dry_run_capital_usdc <= 0.0 {
            return Err(anyhow!("PORTFOLIO_DRY_RUN_CAPITAL_USDC doit être > 0"));
        }

        let enabled_path = env::var("PORTFOLIO_ENABLED_CONFIG")
            .unwrap_or_else(|_| "configs/meche050_enabled.env".to_string());
        Ok(Self {
            sizing,
            sync_grace: Duration::from_millis(sync_grace_ms),
            enabled_path: PathBuf::from(enabled_path),
            dry_run_capital_usdc,
        })
    }
}

/// Persistent, explicit activation matrix for the sixteen final outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnabledStrategies {
    states: BTreeMap<(PortfolioStrategy, MarketSlot), bool>,
}

impl EnabledStrategies {
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("lecture de la grille d'activation {}", path.display()))?;
        let assignments = parse_env_assignments(&content);
        let mut states = BTreeMap::new();

        for strategy in PortfolioStrategy::ALL {
            for market in MarketSlot::ALL {
                let key = enabled_env_key(strategy, market);
                let value = assignments.get(&key).ok_or_else(|| {
                    anyhow!(
                        "{} absent de la grille d'activation {}",
                        key,
                        path.display()
                    )
                })?;
                states.insert((strategy, market), parse_bool(value, &key)?);
            }
        }

        Ok(Self { states })
    }

    pub fn is_enabled(&self, strategy: PortfolioStrategy, market: MarketSlot) -> bool {
        self.states
            .get(&(strategy, market))
            .copied()
            .unwrap_or(false)
    }
}

pub fn enabled_env_key(strategy: PortfolioStrategy, market: MarketSlot) -> String {
    format!(
        "MECHE050_ENABLED_{}_{}",
        strategy.key().to_ascii_uppercase(),
        market.key().to_ascii_uppercase()
    )
}

/// Merges every same-market, same-direction signal into one order candidate.
pub fn group_signals(signals: &[PortfolioSignal]) -> Vec<OrderGroup> {
    let mut groups: Vec<OrderGroup> = Vec::new();
    for signal in signals {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.market == signal.market && group.prediction == signal.prediction)
        {
            group.contributors.push(signal.clone());
        } else {
            groups.push(OrderGroup {
                market: signal.market,
                prediction: signal.prediction.clone(),
                contributors: vec![signal.clone()],
            });
        }
    }
    groups
}

/// Applies W/f sizing to resolved market groups without ever exceeding W.
///
/// `minimums_usdc` is expected to contain one price-fixed minimum per group in the same order.
pub fn size_window(
    rule: SizingRule,
    capital_usdc: f64,
    groups: Vec<OrderGroup>,
    minimums_usdc: &[f64],
) -> Result<SizingDecision> {
    rule.validate()?;
    if capital_usdc <= 0.0 {
        return Err(anyhow!(
            "capital de portefeuille invalide: {}",
            capital_usdc
        ));
    }
    if groups.len() != minimums_usdc.len() {
        return Err(anyhow!(
            "{} groupes mais {} minimums fournis",
            groups.len(),
            minimums_usdc.len()
        ));
    }
    if groups.is_empty() {
        return Ok(SizingDecision::Empty);
    }

    let signal_count = groups
        .iter()
        .map(|group| group.contributors.len())
        .sum::<usize>();
    if signal_count == 0 {
        return Ok(SizingDecision::Empty);
    }

    let window_budget_cents = floor_to_cents(capital_usdc * rule.window_budget_pct / 100.0);
    let per_signal_cents = floor_to_cents(
        capital_usdc
            * (rule.signal_cap_pct / 100.0)
                .min(rule.window_budget_pct / 100.0 / signal_count as f64),
    );
    let mut total_cents = 0_i64;
    let mut orders = Vec::with_capacity(groups.len());

    for (group, minimum_usdc) in groups.into_iter().zip(minimums_usdc.iter().copied()) {
        if minimum_usdc <= 0.0 {
            return Err(anyhow!("minimum d'ordre invalide: {}", minimum_usdc));
        }
        let allocation_cents = per_signal_cents * group.contributors.len() as i64;
        let minimum_cents = round_up_to_cents(minimum_usdc);
        let amount_cents = allocation_cents.max(minimum_cents);
        total_cents += amount_cents;
        orders.push(SizedOrder {
            group,
            allocation_usdc: cents_to_usdc(allocation_cents),
            minimum_usdc: cents_to_usdc(minimum_cents),
            amount_usdc: cents_to_usdc(amount_cents),
        });
    }

    if total_cents > window_budget_cents {
        return Ok(SizingDecision::SkipMinimumsExceedBudget {
            window_budget_usdc: cents_to_usdc(window_budget_cents),
            total_usdc: cents_to_usdc(total_cents),
        });
    }

    Ok(SizingDecision::Submit {
        capital_usdc,
        window_budget_usdc: cents_to_usdc(window_budget_cents),
        per_signal_usdc: cents_to_usdc(per_signal_cents),
        orders,
        total_usdc: cents_to_usdc(total_cents),
    })
}

fn floor_to_cents(value: f64) -> i64 {
    ((value + 1e-9) * CENTS_PER_USDC).floor() as i64
}

fn round_up_to_cents(value: f64) -> i64 {
    ((value - 1e-9) * CENTS_PER_USDC).ceil() as i64
}

fn cents_to_usdc(cents: i64) -> f64 {
    cents as f64 / CENTS_PER_USDC
}

fn parse_env_f64(name: &str, default: f64) -> Result<f64> {
    match env::var(name) {
        Ok(value) => value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("{} doit être un nombre", name)),
        Err(_) => Ok(default),
    }
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .with_context(|| format!("{} doit être un entier", name)),
        Err(_) => Ok(default),
    }
}

fn parse_env_assignments(content: &str) -> BTreeMap<String, String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn parse_bool(value: &str, key: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(anyhow!("{} doit être true ou false", key)),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::fs;

    use super::{
        enabled_env_key, group_signals, size_window, EnabledStrategies, MarketSlot,
        PortfolioSignal, PortfolioStrategy, SizingDecision, SizingRule,
    };
    use crate::strategy::Prediction;

    fn signal(strategy: PortfolioStrategy, market: MarketSlot) -> PortfolioSignal {
        PortfolioSignal {
            strategy,
            market,
            prediction: Prediction::Up,
            signal_close_time: Utc::now(),
        }
    }

    fn rule() -> SizingRule {
        SizingRule {
            window_budget_pct: 3.5,
            signal_cap_pct: 1.2,
        }
    }

    #[test]
    fn isolated_signal_uses_individual_cap() {
        let groups = group_signals(&[signal(PortfolioStrategy::BollFade, MarketSlot::Btc5m)]);
        let decision = size_window(rule(), 1_000.0, groups, &[2.50]).expect("valid sizing");

        match decision {
            SizingDecision::Submit {
                window_budget_usdc,
                per_signal_usdc,
                total_usdc,
                ..
            } => {
                assert_eq!(window_budget_usdc, 35.0);
                assert_eq!(per_signal_usdc, 12.0);
                assert_eq!(total_usdc, 12.0);
            }
            other => panic!("unexpected sizing decision: {other:?}"),
        }
    }

    #[test]
    fn agreeing_signals_share_one_combined_order() {
        let groups = group_signals(&[
            signal(PortfolioStrategy::BollFade, MarketSlot::Btc5m),
            signal(PortfolioStrategy::StreakRsi, MarketSlot::Btc5m),
            signal(PortfolioStrategy::TrioVote2, MarketSlot::Btc5m),
        ]);
        let decision = size_window(rule(), 1_000.0, groups, &[2.50]).expect("valid sizing");

        match decision {
            SizingDecision::Submit {
                orders, total_usdc, ..
            } => {
                assert_eq!(orders.len(), 1);
                assert_eq!(orders[0].group.contributors.len(), 3);
                assert_eq!(total_usdc, 34.98);
            }
            other => panic!("unexpected sizing decision: {other:?}"),
        }
    }

    #[test]
    fn signals_on_two_markets_split_window_budget() {
        let groups = group_signals(&[
            signal(PortfolioStrategy::BollFade, MarketSlot::Btc15m),
            signal(PortfolioStrategy::StreakRsi, MarketSlot::Btc15m),
            signal(PortfolioStrategy::TrioVote2, MarketSlot::Eth15m),
            signal(PortfolioStrategy::ReversalPro, MarketSlot::Eth15m),
        ]);
        let decision = size_window(rule(), 1_000.0, groups, &[2.50, 2.50]).expect("valid sizing");

        match decision {
            SizingDecision::Submit {
                orders, total_usdc, ..
            } => {
                assert_eq!(orders.len(), 2);
                assert_eq!(orders[0].amount_usdc, 17.50);
                assert_eq!(orders[1].amount_usdc, 17.50);
                assert_eq!(total_usdc, 35.0);
            }
            other => panic!("unexpected sizing decision: {other:?}"),
        }
    }

    #[test]
    fn mandatory_minimum_can_exceed_individual_allocation() {
        let groups = group_signals(&[signal(PortfolioStrategy::BollFade, MarketSlot::Btc5m)]);
        let decision = size_window(rule(), 100.0, groups, &[2.50]).expect("valid sizing");

        match decision {
            SizingDecision::Submit {
                orders, total_usdc, ..
            } => {
                assert_eq!(orders[0].allocation_usdc, 1.20);
                assert_eq!(orders[0].amount_usdc, 2.50);
                assert_eq!(total_usdc, 2.50);
            }
            other => panic!("unexpected sizing decision: {other:?}"),
        }
    }

    #[test]
    fn minimums_that_exceed_window_skip_entire_window() {
        let groups = group_signals(&[
            signal(PortfolioStrategy::BollFade, MarketSlot::Btc5m),
            signal(PortfolioStrategy::BollFade, MarketSlot::Eth5m),
        ]);
        let decision = size_window(rule(), 100.0, groups, &[2.50, 2.50]).expect("valid sizing");

        match decision {
            SizingDecision::SkipMinimumsExceedBudget {
                window_budget_usdc,
                total_usdc,
            } => {
                assert_eq!(window_budget_usdc, 3.50);
                assert_eq!(total_usdc, 5.0);
            }
            other => panic!("unexpected sizing decision: {other:?}"),
        }
    }

    #[test]
    fn activation_matrix_keeps_market_specific_disable() {
        let path = std::env::temp_dir().join(format!(
            "meche050-enabled-{}-{}.env",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut body = String::new();
        for strategy in PortfolioStrategy::ALL {
            for market in MarketSlot::ALL {
                let value =
                    if strategy == PortfolioStrategy::TrioVote2 && market == MarketSlot::Eth15m {
                        "false"
                    } else {
                        "true"
                    };
                body.push_str(&format!(
                    "{}={}\n",
                    enabled_env_key(strategy, market),
                    value
                ));
            }
        }
        fs::write(&path, body).expect("write activation matrix");

        let enabled = EnabledStrategies::load(&path).expect("load activation matrix");
        assert!(!enabled.is_enabled(PortfolioStrategy::TrioVote2, MarketSlot::Eth15m));
        assert!(enabled.is_enabled(PortfolioStrategy::TrioVote2, MarketSlot::Btc15m));
        fs::remove_file(path).expect("remove activation matrix");
    }
}
