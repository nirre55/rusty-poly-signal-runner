//! Aggregate temporal and risk reports built from durable trajectories.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::trajectory_analysis::{
    analyze_outcome, load_trajectory_events, ExitCheckpoint, ExitCostAssumptions,
    OutcomeTrajectoryAnalysis, ThresholdPath, TimedPrice, ABOVE_THRESHOLDS, BELOW_THRESHOLDS,
    RISK_HORIZONS_MS,
};

const ENTRY_DEADLINES_MS: [(&str, i64); 6] = [
    ("within_15s", 15_000),
    ("within_30s", 30_000),
    ("within_60s", 60_000),
    ("within_120s", 120_000),
    ("within_180s", 180_000),
    ("within_300s", 300_000),
];

#[derive(Debug, Clone)]
pub struct ReportSignal {
    pub signal_id: String,
    pub strategy: String,
    pub prediction: String,
}

#[derive(Debug, Clone)]
pub struct ReportOutcome {
    pub outcome: String,
    pub signal_at_unix_ms: i64,
    pub winning_outcome: Option<bool>,
    pub signals: Vec<ReportSignal>,
}

#[derive(Debug, Clone)]
pub struct ReportSession {
    pub session_id: String,
    pub market_slot: String,
    pub target_close_time_ms: i64,
    pub completion_status: String,
    pub gap_count: u64,
    pub trajectory_path: Option<PathBuf>,
    pub outcomes: Vec<ReportOutcome>,
}

#[derive(Debug, Clone, Copy)]
pub struct ReportSettings {
    pub costs: ExitCostAssumptions,
    pub minimum_sample_size: u64,
}

#[derive(Debug, Serialize)]
pub struct TrajectoryReportBundle {
    pub temporal: BTreeMap<String, TemporalReport>,
    pub risk: BTreeMap<String, RiskReport>,
}

#[derive(Debug, Serialize)]
pub struct TemporalReport {
    schema_version: u32,
    generated_at: DateTime<Utc>,
    scope: String,
    minimum_sample_size: u64,
    trades_ignored_tie: u64,
    overall: TemporalStats,
    by_market: BTreeMap<String, TemporalStats>,
    by_direction: BTreeMap<String, TemporalStats>,
    by_result: BTreeMap<String, TemporalStats>,
}

#[derive(Debug, Serialize)]
pub struct RiskReport {
    schema_version: u32,
    generated_at: DateTime<Utc>,
    scope: String,
    minimum_sample_size: u64,
    exit_fee_bps: f64,
    exit_slippage_bps: f64,
    trades_ignored_tie: u64,
    overall: RiskStats,
    by_market: BTreeMap<String, RiskStats>,
    by_direction: BTreeMap<String, RiskStats>,
    by_result: BTreeMap<String, RiskStats>,
    by_concordant_votes: BTreeMap<String, RiskStats>,
}

#[derive(Debug, Default, Serialize)]
pub struct DistributionStats {
    count: u64,
    mean: Option<f64>,
    median: Option<f64>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    p25: Option<f64>,
    p75: Option<f64>,
    p90: Option<f64>,
    p95: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
struct QualityStats {
    excluded_data_gaps: u64,
    missing_trajectory: u64,
    invalid_trajectory: u64,
    missing_quote_data: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct TemporalStats {
    total_signals: u64,
    analyzable_signals: u64,
    quality: QualityStats,
    crossed_below_0_50: u64,
    never_crossed_below_0_50: u64,
    immediate_crossings: u64,
    crossing_rate_pct: f64,
    time_to_cross_seconds: DistributionStats,
    crossings_by_deadline: BTreeMap<String, u64>,
    ask_at_signal: DistributionStats,
    spread_at_signal: DistributionStats,
    ask_depth_at_or_below_0_50: DistributionStats,
    distance_to_0_50: DistributionStats,
}

#[derive(Debug, Default, Serialize)]
pub struct RiskStats {
    total_signals: u64,
    analyzable_signals: u64,
    quality: QualityStats,
    strict_fills: u64,
    no_strict_fill: u64,
    resolved_fills: u64,
    wins: u64,
    losses: u64,
    loss_rate_pct: f64,
    loss_rate_confidence_95_pct: ConfidenceInterval,
    sample_warning: bool,
    winning_trade_adverse_excursion: WinningAdverseExcursionStats,
    mae_best_bid: DistributionStats,
    mae_pnl_5_usdc: DistributionStats,
    mae_time_seconds: DistributionStats,
    mfe_best_bid: DistributionStats,
    mfe_pnl_5_usdc: DistributionStats,
    mfe_time_seconds: DistributionStats,
    maximum_drawdown: DistributionStats,
    maximum_drawdown_5_usdc: DistributionStats,
    drawdown_peak_time_seconds: DistributionStats,
    drawdown_trough_time_seconds: DistributionStats,
    horizons: BTreeMap<String, RiskHorizonStats>,
    adverse_thresholds: BTreeMap<String, ThresholdStats>,
    favorable_thresholds: BTreeMap<String, ThresholdStats>,
}

#[derive(Debug, Default, Serialize)]
struct WinningAdverseExcursionStats {
    winning_trades: u64,
    lowest_best_bid: DistributionStats,
    drop_from_0_50: DistributionStats,
    drop_pct_from_0_50: DistributionStats,
    unrealized_pnl_5_shares_usdc: DistributionStats,
    time_to_low_seconds: DistributionStats,
    sample_warning: bool,
}

#[derive(Debug, Default, Serialize)]
struct ConfidenceInterval {
    lower: Option<f64>,
    upper: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
struct RiskHorizonStats {
    samples: u64,
    unavailable_after_resolution: u64,
    missing_quote: u64,
    executable_exits: u64,
    wins: u64,
    losses: u64,
    loss_rate_pct: f64,
    loss_rate_confidence_95_pct: ConfidenceInterval,
    sample_warning: bool,
    best_bid: DistributionStats,
    best_bid_size: DistributionStats,
    bid_shares_available: DistributionStats,
    best_ask: DistributionStats,
    spread: DistributionStats,
    last_trade_price: DistributionStats,
    sell_vwap_5: DistributionStats,
    sell_vwap_candidate: DistributionStats,
    binance_signed_move_bps: DistributionStats,
    time_remaining_seconds: DistributionStats,
    gross_exit_ev_usdc: Option<f64>,
    net_exit_ev_usdc: Option<f64>,
    hold_ev_usdc: Option<f64>,
    net_exit_minus_hold_ev_usdc: Option<f64>,
    losses_avoided: u64,
    wins_sacrificed: u64,
}

#[derive(Debug, Default, Serialize)]
struct ThresholdStats {
    triggered: u64,
    executable_exits: u64,
    wins: u64,
    losses: u64,
    recovered_to_0_50: u64,
    loss_rate_pct: f64,
    loss_rate_confidence_95_pct: ConfidenceInterval,
    sample_warning: bool,
    first_touch_seconds: DistributionStats,
    duration_seconds: DistributionStats,
    first_best_bid: DistributionStats,
    first_sell_vwap_5: DistributionStats,
    gross_exit_ev_usdc: Option<f64>,
    net_exit_ev_usdc: Option<f64>,
    hold_ev_usdc: Option<f64>,
    net_exit_minus_hold_ev_usdc: Option<f64>,
    losses_avoided: u64,
    wins_sacrificed: u64,
}

#[derive(Debug, Clone, Copy)]
enum ExclusionReason {
    DataGap,
    MissingTrajectory,
    InvalidTrajectory,
}

#[derive(Debug, Clone)]
struct AnalyzedTrade {
    market: String,
    direction: String,
    winning: Option<bool>,
    concordant_votes: usize,
    exclusion: Option<ExclusionReason>,
    analysis: Option<OutcomeTrajectoryAnalysis>,
}

#[derive(Default)]
struct Population {
    all_signals: Vec<AnalyzedTrade>,
    majority: Vec<AnalyzedTrade>,
    by_strategy: BTreeMap<String, Vec<AnalyzedTrade>>,
    ignored_ties: u64,
}

pub fn build_trajectory_reports(
    sessions: &[ReportSession],
    strategy_names: &[&str],
    settings: ReportSettings,
) -> TrajectoryReportBundle {
    let population = build_population(sessions, strategy_names, settings.costs);
    let mut temporal = BTreeMap::new();
    let mut risk = BTreeMap::new();
    insert_reports(
        &mut temporal,
        &mut risk,
        "global_all_signals",
        &population.all_signals,
        0,
        settings,
    );
    insert_reports(
        &mut temporal,
        &mut risk,
        "global_majority",
        &population.majority,
        population.ignored_ties,
        settings,
    );
    for strategy in strategy_names {
        let trades = population
            .by_strategy
            .get(*strategy)
            .map(Vec::as_slice)
            .unwrap_or_default();
        insert_reports(&mut temporal, &mut risk, strategy, trades, 0, settings);
    }
    TrajectoryReportBundle { temporal, risk }
}

fn build_population(
    sessions: &[ReportSession],
    strategy_names: &[&str],
    costs: ExitCostAssumptions,
) -> Population {
    let mut population = Population {
        by_strategy: strategy_names
            .iter()
            .map(|strategy| ((*strategy).to_string(), Vec::new()))
            .collect(),
        ..Population::default()
    };
    for session in sessions {
        let (events, path_exclusion) = load_session_events(session);
        let has_data_gap = session.gap_count > 0
            || !session
                .completion_status
                .eq_ignore_ascii_case("RESOLVED_COMPLETE");
        let mut analyzed_outcomes = BTreeMap::new();
        for outcome in &session.outcomes {
            let exclusion = path_exclusion.or(has_data_gap.then_some(ExclusionReason::DataGap));
            let analysis = events.as_ref().map(|events| {
                analyze_outcome(
                    events,
                    &outcome.outcome,
                    outcome.signal_at_unix_ms,
                    session.target_close_time_ms,
                    costs,
                )
            });
            analyzed_outcomes.insert(outcome.outcome.to_ascii_uppercase(), analysis.clone());
            for signal in &outcome.signals {
                let trade = AnalyzedTrade {
                    market: session.market_slot.clone(),
                    direction: signal.prediction.to_ascii_uppercase(),
                    winning: outcome.winning_outcome,
                    concordant_votes: outcome.signals.len(),
                    exclusion,
                    analysis: analysis.clone(),
                };
                population.all_signals.push(trade.clone());
                population
                    .by_strategy
                    .entry(signal.strategy.clone())
                    .or_default()
                    .push(trade);
            }
        }

        let up = session
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome.eq_ignore_ascii_case("UP"));
        let down = session
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome.eq_ignore_ascii_case("DOWN"));
        let up_votes = up.map_or(0, |outcome| outcome.signals.len());
        let down_votes = down.map_or(0, |outcome| outcome.signals.len());
        if up_votes == down_votes {
            if up_votes > 0 {
                population.ignored_ties += 1;
            }
            continue;
        }
        let selected = if up_votes > down_votes { up } else { down };
        let Some(outcome) = selected else {
            continue;
        };
        let exclusion = path_exclusion.or(has_data_gap.then_some(ExclusionReason::DataGap));
        population.majority.push(AnalyzedTrade {
            market: session.market_slot.clone(),
            direction: outcome.outcome.to_ascii_uppercase(),
            winning: outcome.winning_outcome,
            concordant_votes: outcome.signals.len(),
            exclusion,
            analysis: analyzed_outcomes
                .get(&outcome.outcome.to_ascii_uppercase())
                .cloned()
                .flatten(),
        });
    }
    population
}

fn load_session_events(
    session: &ReportSession,
) -> (
    Option<Vec<crate::trajectory_analysis::TrajectoryEvent>>,
    Option<ExclusionReason>,
) {
    let Some(path) = session.trajectory_path.as_ref() else {
        return (None, Some(ExclusionReason::MissingTrajectory));
    };
    if !path.exists() {
        return (None, Some(ExclusionReason::MissingTrajectory));
    }
    match load_trajectory_events(path) {
        Ok(events) => (Some(events), None),
        Err(_) => (None, Some(ExclusionReason::InvalidTrajectory)),
    }
}

fn insert_reports(
    temporal: &mut BTreeMap<String, TemporalReport>,
    risk: &mut BTreeMap<String, RiskReport>,
    scope: &str,
    trades: &[AnalyzedTrade],
    ignored_ties: u64,
    settings: ReportSettings,
) {
    temporal.insert(
        scope.to_string(),
        build_temporal_report(scope, trades, ignored_ties, settings.minimum_sample_size),
    );
    risk.insert(
        scope.to_string(),
        build_risk_report(scope, trades, ignored_ties, settings),
    );
}

fn build_temporal_report(
    scope: &str,
    trades: &[AnalyzedTrade],
    ignored_ties: u64,
    minimum_sample_size: u64,
) -> TemporalReport {
    TemporalReport {
        schema_version: 1,
        generated_at: Utc::now(),
        scope: scope.to_string(),
        minimum_sample_size,
        trades_ignored_tie: ignored_ties,
        overall: TemporalAccumulator::from_trades(trades).finish(),
        by_market: grouped_temporal(trades, |trade| trade.market.clone()),
        by_direction: grouped_temporal(trades, |trade| trade.direction.clone()),
        by_result: grouped_temporal(trades, result_key),
    }
}

fn build_risk_report(
    scope: &str,
    trades: &[AnalyzedTrade],
    ignored_ties: u64,
    settings: ReportSettings,
) -> RiskReport {
    RiskReport {
        schema_version: 2,
        generated_at: Utc::now(),
        scope: scope.to_string(),
        minimum_sample_size: settings.minimum_sample_size,
        exit_fee_bps: settings.costs.fee_bps,
        exit_slippage_bps: settings.costs.slippage_bps,
        trades_ignored_tie: ignored_ties,
        overall: RiskAccumulator::from_trades(trades).finish(settings.minimum_sample_size),
        by_market: grouped_risk(
            trades,
            |trade| trade.market.clone(),
            settings.minimum_sample_size,
        ),
        by_direction: grouped_risk(
            trades,
            |trade| trade.direction.clone(),
            settings.minimum_sample_size,
        ),
        by_result: grouped_risk(trades, result_key, settings.minimum_sample_size),
        by_concordant_votes: grouped_risk(
            trades,
            |trade| format!("votes_{}", trade.concordant_votes),
            settings.minimum_sample_size,
        ),
    }
}

fn result_key(trade: &AnalyzedTrade) -> String {
    match trade.winning {
        Some(true) => "WIN".to_string(),
        Some(false) => "LOSS".to_string(),
        None => "PENDING".to_string(),
    }
}

fn grouped_temporal(
    trades: &[AnalyzedTrade],
    key: impl Fn(&AnalyzedTrade) -> String,
) -> BTreeMap<String, TemporalStats> {
    let mut groups = BTreeMap::<String, TemporalAccumulator>::new();
    for trade in trades {
        groups.entry(key(trade)).or_default().add(trade);
    }
    groups
        .into_iter()
        .map(|(key, accumulator)| (key, accumulator.finish()))
        .collect()
}

fn grouped_risk(
    trades: &[AnalyzedTrade],
    key: impl Fn(&AnalyzedTrade) -> String,
    minimum_sample_size: u64,
) -> BTreeMap<String, RiskStats> {
    let mut groups = BTreeMap::<String, RiskAccumulator>::new();
    for trade in trades {
        groups.entry(key(trade)).or_default().add(trade);
    }
    groups
        .into_iter()
        .map(|(key, accumulator)| (key, accumulator.finish(minimum_sample_size)))
        .collect()
}

#[derive(Default)]
struct TemporalAccumulator {
    total_signals: u64,
    analyzable_signals: u64,
    quality: QualityStats,
    crossed: u64,
    not_crossed: u64,
    immediate: u64,
    times_seconds: Vec<f64>,
    deadlines: BTreeMap<String, u64>,
    asks: Vec<f64>,
    spreads: Vec<f64>,
    depths: Vec<f64>,
    distances: Vec<f64>,
}

impl TemporalAccumulator {
    fn from_trades(trades: &[AnalyzedTrade]) -> Self {
        let mut accumulator = Self::default();
        for trade in trades {
            accumulator.add(trade);
        }
        accumulator
    }

    fn add(&mut self, trade: &AnalyzedTrade) {
        self.total_signals += 1;
        if add_exclusion(&mut self.quality, trade.exclusion) {
            return;
        }
        let Some(analysis) = trade.analysis.as_ref() else {
            self.quality.missing_quote_data += 1;
            return;
        };
        self.analyzable_signals += 1;
        if let Some(quote) = analysis.quote_at_signal.as_ref() {
            if let Some(ask) = quote.best_ask {
                self.asks.push(ask);
                self.distances.push(ask - 0.50);
                if let Some(bid) = quote.best_bid {
                    self.spreads.push(ask - bid);
                }
            }
            if let Some(depth) = quote.ask_shares_at_or_below_limit {
                self.depths.push(depth);
            }
        } else {
            self.quality.missing_quote_data += 1;
        }
        if let Some(fill) = analysis.strict_fill.as_ref() {
            self.crossed += 1;
            if fill.elapsed_from_signal_ms == 0 {
                self.immediate += 1;
            }
            self.times_seconds
                .push(fill.elapsed_from_signal_ms as f64 / 1_000.0);
            for (name, deadline_ms) in ENTRY_DEADLINES_MS {
                if fill.elapsed_from_signal_ms <= deadline_ms {
                    *self.deadlines.entry(name.to_string()).or_default() += 1;
                }
            }
        } else {
            self.not_crossed += 1;
        }
    }

    fn finish(self) -> TemporalStats {
        TemporalStats {
            total_signals: self.total_signals,
            analyzable_signals: self.analyzable_signals,
            quality: self.quality,
            crossed_below_0_50: self.crossed,
            never_crossed_below_0_50: self.not_crossed,
            immediate_crossings: self.immediate,
            crossing_rate_pct: percentage(self.crossed, self.analyzable_signals),
            time_to_cross_seconds: distribution(self.times_seconds),
            crossings_by_deadline: ENTRY_DEADLINES_MS
                .into_iter()
                .map(|(name, _)| (name.to_string(), *self.deadlines.get(name).unwrap_or(&0)))
                .collect(),
            ask_at_signal: distribution(self.asks),
            spread_at_signal: distribution(self.spreads),
            ask_depth_at_or_below_0_50: distribution(self.depths),
            distance_to_0_50: distribution(self.distances),
        }
    }
}

#[derive(Default)]
struct RiskAccumulator {
    total_signals: u64,
    analyzable_signals: u64,
    quality: QualityStats,
    strict_fills: u64,
    no_fill: u64,
    wins: u64,
    losses: u64,
    winning_adverse_excursion: WinningAdverseExcursionAccumulator,
    mae: Vec<f64>,
    mae_pnl: Vec<f64>,
    mae_times: Vec<f64>,
    mfe: Vec<f64>,
    mfe_pnl: Vec<f64>,
    mfe_times: Vec<f64>,
    maximum_drawdowns: Vec<f64>,
    maximum_drawdown_pnl: Vec<f64>,
    drawdown_peak_times: Vec<f64>,
    drawdown_trough_times: Vec<f64>,
    horizons: BTreeMap<String, HorizonAccumulator>,
    below: BTreeMap<String, ThresholdAccumulator>,
    above: BTreeMap<String, ThresholdAccumulator>,
}

impl RiskAccumulator {
    fn from_trades(trades: &[AnalyzedTrade]) -> Self {
        let mut accumulator = Self::default();
        for trade in trades {
            accumulator.add(trade);
        }
        accumulator
    }

    fn add(&mut self, trade: &AnalyzedTrade) {
        self.total_signals += 1;
        if add_exclusion(&mut self.quality, trade.exclusion) {
            return;
        }
        let Some(analysis) = trade.analysis.as_ref() else {
            self.quality.missing_quote_data += 1;
            return;
        };
        self.analyzable_signals += 1;
        let Some(risk) = analysis.risk.as_ref() else {
            self.no_fill += 1;
            return;
        };
        self.strict_fills += 1;
        match trade.winning {
            Some(true) => {
                self.wins += 1;
                self.winning_adverse_excursion
                    .add(risk.mae_best_bid.as_ref());
            }
            Some(false) => self.losses += 1,
            None => {}
        }
        if let Some(mae) = risk.mae_best_bid.as_ref() {
            self.mae.push(mae.value);
            self.mae_pnl.push((mae.value - 0.50) * 5.0);
            self.mae_times
                .push(mae.elapsed_from_fill_ms as f64 / 1_000.0);
        }
        if let Some(mfe) = risk.mfe_best_bid.as_ref() {
            self.mfe.push(mfe.value);
            self.mfe_pnl.push((mfe.value - 0.50) * 5.0);
            self.mfe_times
                .push(mfe.elapsed_from_fill_ms as f64 / 1_000.0);
        }
        if let Some(drawdown) = risk.maximum_drawdown.as_ref() {
            self.maximum_drawdowns.push(drawdown.drawdown);
            self.maximum_drawdown_pnl.push(drawdown.drawdown * 5.0);
            self.drawdown_peak_times
                .push(drawdown.peak_elapsed_from_fill_ms as f64 / 1_000.0);
            self.drawdown_trough_times
                .push(drawdown.trough_elapsed_from_fill_ms as f64 / 1_000.0);
        }
        for (name, checkpoint) in &risk.checkpoints {
            self.horizons
                .entry(name.clone())
                .or_default()
                .add(checkpoint, trade.winning);
        }
        for (name, threshold) in &risk.below_thresholds {
            self.below
                .entry(name.clone())
                .or_default()
                .add(threshold, trade.winning);
        }
        for (name, threshold) in &risk.above_thresholds {
            self.above
                .entry(name.clone())
                .or_default()
                .add(threshold, trade.winning);
        }
    }

    fn finish(self, minimum_sample_size: u64) -> RiskStats {
        let resolved = self.wins + self.losses;
        RiskStats {
            total_signals: self.total_signals,
            analyzable_signals: self.analyzable_signals,
            quality: self.quality,
            strict_fills: self.strict_fills,
            no_strict_fill: self.no_fill,
            resolved_fills: resolved,
            wins: self.wins,
            losses: self.losses,
            loss_rate_pct: percentage(self.losses, resolved),
            loss_rate_confidence_95_pct: wilson_interval(self.losses, resolved),
            sample_warning: resolved < minimum_sample_size,
            winning_trade_adverse_excursion: self
                .winning_adverse_excursion
                .finish(minimum_sample_size),
            mae_best_bid: distribution(self.mae),
            mae_pnl_5_usdc: distribution(self.mae_pnl),
            mae_time_seconds: distribution(self.mae_times),
            mfe_best_bid: distribution(self.mfe),
            mfe_pnl_5_usdc: distribution(self.mfe_pnl),
            mfe_time_seconds: distribution(self.mfe_times),
            maximum_drawdown: distribution(self.maximum_drawdowns),
            maximum_drawdown_5_usdc: distribution(self.maximum_drawdown_pnl),
            drawdown_peak_time_seconds: distribution(self.drawdown_peak_times),
            drawdown_trough_time_seconds: distribution(self.drawdown_trough_times),
            horizons: RISK_HORIZONS_MS
                .into_iter()
                .map(|(name, _)| {
                    let accumulator = self.horizons.get(name).cloned().unwrap_or_default();
                    (name.to_string(), accumulator.finish(minimum_sample_size))
                })
                .collect(),
            adverse_thresholds: BELOW_THRESHOLDS
                .into_iter()
                .map(|(name, _)| {
                    let accumulator = self.below.get(name).cloned().unwrap_or_default();
                    (name.to_string(), accumulator.finish(minimum_sample_size))
                })
                .collect(),
            favorable_thresholds: ABOVE_THRESHOLDS
                .into_iter()
                .map(|(name, _)| {
                    let accumulator = self.above.get(name).cloned().unwrap_or_default();
                    (name.to_string(), accumulator.finish(minimum_sample_size))
                })
                .collect(),
        }
    }
}

#[derive(Default)]
struct WinningAdverseExcursionAccumulator {
    winning_trades: u64,
    lowest_best_bids: Vec<f64>,
    drops_from_half: Vec<f64>,
    drop_percentages: Vec<f64>,
    unrealized_pnls: Vec<f64>,
    times_to_low: Vec<f64>,
}

impl WinningAdverseExcursionAccumulator {
    fn add(&mut self, mae: Option<&TimedPrice>) {
        let Some(mae) = mae else {
            return;
        };
        self.winning_trades += 1;
        let drop = (0.50 - mae.value).max(0.0);
        self.lowest_best_bids.push(mae.value);
        self.drops_from_half.push(drop);
        self.drop_percentages.push(drop / 0.50 * 100.0);
        self.unrealized_pnls.push(-drop * 5.0);
        self.times_to_low
            .push(mae.elapsed_from_fill_ms as f64 / 1_000.0);
    }

    fn finish(self, minimum_sample_size: u64) -> WinningAdverseExcursionStats {
        WinningAdverseExcursionStats {
            winning_trades: self.winning_trades,
            lowest_best_bid: distribution(self.lowest_best_bids),
            drop_from_0_50: distribution(self.drops_from_half),
            drop_pct_from_0_50: distribution(self.drop_percentages),
            unrealized_pnl_5_shares_usdc: distribution(self.unrealized_pnls),
            time_to_low_seconds: distribution(self.times_to_low),
            sample_warning: self.winning_trades < minimum_sample_size,
        }
    }
}

#[derive(Default, Clone)]
struct HorizonAccumulator {
    samples: u64,
    unavailable: u64,
    missing_quote: u64,
    executable: u64,
    wins: u64,
    losses: u64,
    bids: Vec<f64>,
    best_bid_sizes: Vec<f64>,
    bid_shares: Vec<f64>,
    asks: Vec<f64>,
    spreads: Vec<f64>,
    last_trade_prices: Vec<f64>,
    vwaps: Vec<f64>,
    candidate_vwaps: Vec<f64>,
    binance_moves: Vec<f64>,
    remaining_seconds: Vec<f64>,
    gross_exit_total: f64,
    net_exit_total: f64,
    hold_total: f64,
    comparison_samples: u64,
    losses_avoided: u64,
    wins_sacrificed: u64,
}

impl HorizonAccumulator {
    fn add(&mut self, checkpoint: &ExitCheckpoint, winning: Option<bool>) {
        self.samples += 1;
        if checkpoint.unavailable_after_resolution {
            self.unavailable += 1;
            return;
        }
        let Some(bid) = checkpoint.best_bid else {
            self.missing_quote += 1;
            return;
        };
        self.bids.push(bid);
        if let Some(value) = checkpoint.best_bid_size {
            self.best_bid_sizes.push(value);
        }
        if let Some(value) = checkpoint.bid_shares_available {
            self.bid_shares.push(value);
        }
        if let Some(value) = checkpoint.best_ask {
            self.asks.push(value);
        }
        if let Some(value) = checkpoint.spread {
            self.spreads.push(value);
        }
        if let Some(value) = checkpoint.last_trade_price {
            self.last_trade_prices.push(value);
        }
        if let Some(value) = checkpoint.sell_vwap_candidate {
            self.candidate_vwaps.push(value);
        }
        self.remaining_seconds
            .push(checkpoint.time_remaining_ms as f64 / 1_000.0);
        if let Some(move_bps) = checkpoint.binance_signed_move_bps {
            self.binance_moves.push(move_bps);
        }
        match winning {
            Some(true) => self.wins += 1,
            Some(false) => self.losses += 1,
            None => {}
        }
        if let (Some(vwap), Some(gross), Some(net), Some(winning)) = (
            checkpoint.sell_vwap_5,
            checkpoint.gross_pnl_5_usdc,
            checkpoint.net_pnl_5_usdc,
            winning,
        ) {
            self.executable += 1;
            self.vwaps.push(vwap);
            self.gross_exit_total += gross;
            self.net_exit_total += net;
            let hold = hold_pnl_5(winning);
            self.hold_total += hold;
            self.comparison_samples += 1;
            if winning && net < hold {
                self.wins_sacrificed += 1;
            } else if !winning && net > hold {
                self.losses_avoided += 1;
            }
        }
    }

    fn finish(self, minimum_sample_size: u64) -> RiskHorizonStats {
        let resolved = self.wins + self.losses;
        let gross_ev = average(self.gross_exit_total, self.comparison_samples);
        let net_ev = average(self.net_exit_total, self.comparison_samples);
        let hold_ev = average(self.hold_total, self.comparison_samples);
        RiskHorizonStats {
            samples: self.samples,
            unavailable_after_resolution: self.unavailable,
            missing_quote: self.missing_quote,
            executable_exits: self.executable,
            wins: self.wins,
            losses: self.losses,
            loss_rate_pct: percentage(self.losses, resolved),
            loss_rate_confidence_95_pct: wilson_interval(self.losses, resolved),
            sample_warning: resolved < minimum_sample_size,
            best_bid: distribution(self.bids),
            best_bid_size: distribution(self.best_bid_sizes),
            bid_shares_available: distribution(self.bid_shares),
            best_ask: distribution(self.asks),
            spread: distribution(self.spreads),
            last_trade_price: distribution(self.last_trade_prices),
            sell_vwap_5: distribution(self.vwaps),
            sell_vwap_candidate: distribution(self.candidate_vwaps),
            binance_signed_move_bps: distribution(self.binance_moves),
            time_remaining_seconds: distribution(self.remaining_seconds),
            gross_exit_ev_usdc: gross_ev,
            net_exit_ev_usdc: net_ev,
            hold_ev_usdc: hold_ev,
            net_exit_minus_hold_ev_usdc: net_ev.zip(hold_ev).map(|(exit, hold)| exit - hold),
            losses_avoided: self.losses_avoided,
            wins_sacrificed: self.wins_sacrificed,
        }
    }
}

#[derive(Default, Clone)]
struct ThresholdAccumulator {
    triggered: u64,
    executable: u64,
    wins: u64,
    losses: u64,
    recovered: u64,
    first_times: Vec<f64>,
    durations: Vec<f64>,
    first_bids: Vec<f64>,
    first_vwaps: Vec<f64>,
    gross_exit_total: f64,
    net_exit_total: f64,
    hold_total: f64,
    comparison_samples: u64,
    losses_avoided: u64,
    wins_sacrificed: u64,
}

impl ThresholdAccumulator {
    fn add(&mut self, threshold: &ThresholdPath, winning: Option<bool>) {
        let Some(first_ms) = threshold.first_elapsed_from_fill_ms else {
            return;
        };
        self.triggered += 1;
        self.first_times.push(first_ms as f64 / 1_000.0);
        self.durations.push(threshold.duration_ms as f64 / 1_000.0);
        if let Some(bid) = threshold.first_best_bid {
            self.first_bids.push(bid);
        }
        if let Some(vwap) = threshold.first_sell_vwap_5 {
            self.first_vwaps.push(vwap);
        }
        if threshold.recovered_to_half {
            self.recovered += 1;
        }
        match winning {
            Some(true) => self.wins += 1,
            Some(false) => self.losses += 1,
            None => {}
        }
        if let (Some(gross), Some(net), Some(winning)) = (
            threshold.first_gross_pnl_5_usdc,
            threshold.first_net_pnl_5_usdc,
            winning,
        ) {
            self.executable += 1;
            self.gross_exit_total += gross;
            self.net_exit_total += net;
            let hold = hold_pnl_5(winning);
            self.hold_total += hold;
            self.comparison_samples += 1;
            if winning && net < hold {
                self.wins_sacrificed += 1;
            } else if !winning && net > hold {
                self.losses_avoided += 1;
            }
        }
    }

    fn finish(self, minimum_sample_size: u64) -> ThresholdStats {
        let resolved = self.wins + self.losses;
        let gross_ev = average(self.gross_exit_total, self.comparison_samples);
        let net_ev = average(self.net_exit_total, self.comparison_samples);
        let hold_ev = average(self.hold_total, self.comparison_samples);
        ThresholdStats {
            triggered: self.triggered,
            executable_exits: self.executable,
            wins: self.wins,
            losses: self.losses,
            recovered_to_0_50: self.recovered,
            loss_rate_pct: percentage(self.losses, resolved),
            loss_rate_confidence_95_pct: wilson_interval(self.losses, resolved),
            sample_warning: resolved < minimum_sample_size,
            first_touch_seconds: distribution(self.first_times),
            duration_seconds: distribution(self.durations),
            first_best_bid: distribution(self.first_bids),
            first_sell_vwap_5: distribution(self.first_vwaps),
            gross_exit_ev_usdc: gross_ev,
            net_exit_ev_usdc: net_ev,
            hold_ev_usdc: hold_ev,
            net_exit_minus_hold_ev_usdc: net_ev.zip(hold_ev).map(|(exit, hold)| exit - hold),
            losses_avoided: self.losses_avoided,
            wins_sacrificed: self.wins_sacrificed,
        }
    }
}

fn add_exclusion(quality: &mut QualityStats, exclusion: Option<ExclusionReason>) -> bool {
    match exclusion {
        Some(ExclusionReason::DataGap) => quality.excluded_data_gaps += 1,
        Some(ExclusionReason::MissingTrajectory) => quality.missing_trajectory += 1,
        Some(ExclusionReason::InvalidTrajectory) => quality.invalid_trajectory += 1,
        None => return false,
    }
    true
}

fn distribution(mut values: Vec<f64>) -> DistributionStats {
    values.retain(|value| value.is_finite());
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return DistributionStats::default();
    }
    DistributionStats {
        count: values.len() as u64,
        mean: Some(values.iter().sum::<f64>() / values.len() as f64),
        median: percentile(&values, 0.50),
        minimum: values.first().copied(),
        maximum: values.last().copied(),
        p25: percentile(&values, 0.25),
        p75: percentile(&values, 0.75),
        p90: percentile(&values, 0.90),
        p95: percentile(&values, 0.95),
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let position = (sorted.len() - 1) as f64 * percentile.clamp(0.0, 1.0);
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        Some(sorted[lower])
    } else {
        let weight = position - lower as f64;
        Some(sorted[lower] * (1.0 - weight) + sorted[upper] * weight)
    }
}

fn wilson_interval(losses: u64, total: u64) -> ConfidenceInterval {
    if total == 0 {
        return ConfidenceInterval::default();
    }
    let z = 1.959_963_984_540_054_f64;
    let n = total as f64;
    let p = losses as f64 / n;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let margin = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denominator;
    ConfidenceInterval {
        lower: Some((center - margin).max(0.0) * 100.0),
        upper: Some((center + margin).min(1.0) * 100.0),
    }
}

fn percentage(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 * 100.0 / total as f64
    }
}

fn average(total: f64, count: u64) -> Option<f64> {
    (count > 0).then_some(total / count as f64)
}

fn hold_pnl_5(winning: bool) -> f64 {
    if winning {
        2.50
    } else {
        -2.50
    }
}

#[cfg(test)]
mod tests {
    use super::{distribution, percentile, wilson_interval, WinningAdverseExcursionAccumulator};

    #[test]
    fn percentile_interpolates_even_sample_median() {
        let values = [1.0, 2.0, 3.0, 4.0];

        assert_eq!(percentile(&values, 0.50), Some(2.5));
    }

    #[test]
    fn distribution_reports_requested_percentiles() {
        let stats = distribution((1..=100).map(f64::from).collect());

        assert_eq!(stats.p95, Some(95.05));
    }

    #[test]
    fn wilson_interval_is_empty_without_samples() {
        let interval = wilson_interval(0, 0);

        assert!(interval.lower.is_none());
    }

    #[test]
    fn winning_adverse_excursion_warns_and_stays_empty_without_measurable_winner() {
        let mut accumulator = WinningAdverseExcursionAccumulator::default();
        accumulator.add(None);

        let stats = accumulator.finish(30);

        assert_eq!(
            (
                stats.winning_trades,
                stats.lowest_best_bid.count,
                stats.sample_warning,
            ),
            (0, 0, true)
        );
    }
}
