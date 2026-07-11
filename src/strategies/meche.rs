use std::collections::VecDeque;

use crate::binance::Candle;
use crate::strategies::indicators::{AtrState, RsiState};
use crate::strategy::{Prediction, Signal, Strategy};

const EPSILON: f64 = 1e-12;
const BOLL_PERIOD: usize = 20;
const DONCH_PERIOD: usize = 12;
const Z_PERIOD: usize = 24;

fn push_candle(candles: &mut VecDeque<Candle>, candle: &Candle, max_len: usize) {
    candles.push_back(candle.clone());
    if candles.len() > max_len {
        candles.pop_front();
    }
}

fn body_ratio(candle: &Candle) -> Option<f64> {
    let range = candle.high - candle.low;
    (range.abs() > EPSILON).then(|| (candle.close - candle.open).abs() / range)
}

fn strict_green(candle: &Candle) -> Option<bool> {
    if candle.close > candle.open {
        Some(true)
    } else if candle.close < candle.open {
        Some(false)
    } else {
        None
    }
}

fn same_color_streak(candles: &VecDeque<Candle>, minimum: usize) -> Option<bool> {
    if candles.len() < minimum {
        return None;
    }

    let first = strict_green(candles.back()?)?;
    candles
        .iter()
        .rev()
        .take(minimum)
        .all(|candle| strict_green(candle) == Some(first))
        .then_some(first)
}

fn sample_close_z(candles: &VecDeque<Candle>, period: usize) -> Option<f64> {
    if candles.len() < period {
        return None;
    }

    let closes: Vec<f64> = candles
        .iter()
        .rev()
        .take(period)
        .map(|candle| candle.close)
        .collect();
    let mean = closes.iter().sum::<f64>() / closes.len() as f64;
    let variance = closes
        .iter()
        .map(|close| (close - mean).powi(2))
        .sum::<f64>()
        / (closes.len() - 1) as f64;
    let stddev = variance.sqrt();
    (stddev > EPSILON).then(|| (closes[0] - mean) / stddev)
}

fn signed_body_sum(candles: &VecDeque<Candle>, period: usize) -> Option<f64> {
    if candles.len() < period {
        return None;
    }

    candles
        .iter()
        .rev()
        .take(period)
        .try_fold(0.0, |sum, candle| {
            (candle.close.abs() > EPSILON)
                .then(|| sum + (candle.close - candle.open) / candle.close)
        })
}

/// Contrarian Bollinger fade: capitulation hors bande puis pari sur le retour.
pub struct BollFade {
    candles: VecDeque<Candle>,
}

impl BollFade {
    pub fn new() -> Self {
        Self {
            candles: VecDeque::with_capacity(BOLL_PERIOD),
        }
    }

    fn evaluate(&self, candle: &Candle) -> Option<Prediction> {
        if self.candles.len() < BOLL_PERIOD {
            return None;
        }

        let mean = self.candles.iter().map(|item| item.close).sum::<f64>() / BOLL_PERIOD as f64;
        let variance = self
            .candles
            .iter()
            .map(|item| (item.close - mean).powi(2))
            .sum::<f64>()
            / BOLL_PERIOD as f64;
        let stddev = variance.sqrt();
        let ratio = body_ratio(candle)?;

        if candle.close < mean - 2.2 * stddev && candle.close < candle.open && ratio >= 0.50 {
            Some(Prediction::Up)
        } else if candle.close > mean + 2.2 * stddev && candle.close > candle.open && ratio >= 0.50
        {
            Some(Prediction::Down)
        } else {
            None
        }
    }
}

impl Default for BollFade {
    fn default() -> Self {
        Self::new()
    }
}

impl Strategy for BollFade {
    fn name(&self) -> &str {
        "boll_fade"
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        push_candle(&mut self.candles, candle, BOLL_PERIOD);
        let prediction = self.evaluate(candle)?;
        Some(Signal {
            prediction,
            signal_candle_close_time: candle.close_time,
            rsi: 50.0,
            strategy_name: self.name().to_string(),
        })
    }

    fn warmup(&mut self, candle: &Candle) {
        push_candle(&mut self.candles, candle, BOLL_PERIOD);
    }

    fn current_rsi(&self) -> Option<f64> {
        None
    }

    fn current_series(&self) -> Option<bool> {
        self.candles.back().and_then(strict_green)
    }

    fn current_atr(&self) -> Option<f64> {
        None
    }

    fn candle_log_extras(&self) -> String {
        format!(
            "boll_period={} | ready={}",
            BOLL_PERIOD,
            self.candles.len() == BOLL_PERIOD
        )
    }
}

/// Reversal après une série de trois bougies, un RSI7 extrême et une bougie large.
pub struct StreakRsi {
    candles: VecDeque<Candle>,
    rsi: RsiState,
    atr: AtrState,
}

impl StreakRsi {
    pub fn new() -> Self {
        Self {
            candles: VecDeque::with_capacity(3),
            rsi: RsiState::new(7),
            atr: AtrState::new(14),
        }
    }

    fn feed(&mut self, candle: &Candle) {
        self.rsi.update(candle.close);
        self.atr.update(candle);
        push_candle(&mut self.candles, candle, 3);
    }

    fn evaluate(&self, candle: &Candle) -> Option<Prediction> {
        let series_is_green = same_color_streak(&self.candles, 3)?;
        let rsi = self.rsi.get()?;
        let atr = self.atr.raw()?;
        let ratio = body_ratio(candle)?;
        let range_ok = candle.high - candle.low >= atr;
        if !range_ok || ratio < 0.60 {
            return None;
        }

        if series_is_green && rsi >= 65.0 {
            Some(Prediction::Down)
        } else if !series_is_green && rsi <= 35.0 {
            Some(Prediction::Up)
        } else {
            None
        }
    }
}

impl Default for StreakRsi {
    fn default() -> Self {
        Self::new()
    }
}

impl Strategy for StreakRsi {
    fn name(&self) -> &str {
        "streak_rsi"
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        self.feed(candle);
        let prediction = self.evaluate(candle)?;
        Some(Signal {
            prediction,
            signal_candle_close_time: candle.close_time,
            rsi: self.rsi.get()?,
            strategy_name: self.name().to_string(),
        })
    }

    fn warmup(&mut self, candle: &Candle) {
        self.feed(candle);
    }

    fn current_rsi(&self) -> Option<f64> {
        self.rsi.get()
    }

    fn current_series(&self) -> Option<bool> {
        same_color_streak(&self.candles, 3)
    }

    fn current_atr(&self) -> Option<f64> {
        self.atr.raw()
    }

    fn candle_log_extras(&self) -> String {
        format!(
            "rsi7={} | atr14={} | streak={}",
            self.rsi
                .get()
                .map_or_else(|| "N/A".to_string(), |value| format!("{value:.2}")),
            self.atr
                .raw()
                .map_or_else(|| "N/A".to_string(), |value| format!("{value:.4}")),
            match same_color_streak(&self.candles, 3) {
                Some(true) => "green",
                Some(false) => "red",
                None => "mixed",
            }
        )
    }
}

/// Troisième votant de `trio_vote2`: Donchian 12, pression des corps et z-score 24.
pub struct DonchZscore {
    candles: VecDeque<Candle>,
    last_z: Option<f64>,
}

impl DonchZscore {
    pub fn new() -> Self {
        Self {
            candles: VecDeque::with_capacity(Z_PERIOD),
            last_z: None,
        }
    }

    fn feed(&mut self, candle: &Candle) {
        push_candle(&mut self.candles, candle, Z_PERIOD);
        self.last_z = sample_close_z(&self.candles, Z_PERIOD);
    }

    fn evaluate(&self, candle: &Candle) -> Option<Prediction> {
        if self.candles.len() < Z_PERIOD || candle.close.abs() <= EPSILON {
            return None;
        }

        let min_low = self
            .candles
            .iter()
            .rev()
            .take(DONCH_PERIOD)
            .map(|item| item.low)
            .fold(f64::INFINITY, f64::min);
        let max_high = self
            .candles
            .iter()
            .rev()
            .take(DONCH_PERIOD)
            .map(|item| item.high)
            .fold(f64::NEG_INFINITY, f64::max);
        let body_sum = signed_body_sum(&self.candles, 6)?;
        let z = self.last_z?;
        let near_low = (candle.close - min_low) / candle.close <= 0.00035;
        let near_high = (candle.close - max_high) / candle.close >= -0.00035;

        if near_low && body_sum <= -0.0045 && z <= -2.1 {
            Some(Prediction::Up)
        } else if near_high && body_sum >= 0.0045 && z >= 2.1 {
            Some(Prediction::Down)
        } else {
            None
        }
    }
}

impl Default for DonchZscore {
    fn default() -> Self {
        Self::new()
    }
}

impl Strategy for DonchZscore {
    fn name(&self) -> &str {
        "donch_zscore"
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        self.feed(candle);
        let prediction = self.evaluate(candle)?;
        Some(Signal {
            prediction,
            signal_candle_close_time: candle.close_time,
            rsi: (self.last_z?.abs() * 10.0).min(100.0),
            strategy_name: self.name().to_string(),
        })
    }

    fn warmup(&mut self, candle: &Candle) {
        self.feed(candle);
    }

    fn current_rsi(&self) -> Option<f64> {
        None
    }

    fn current_series(&self) -> Option<bool> {
        None
    }

    fn current_atr(&self) -> Option<f64> {
        None
    }

    fn candle_log_extras(&self) -> String {
        self.last_z
            .map_or_else(|| "z24=N/A".to_string(), |value| format!("z24={value:.3}"))
    }
}

/// Fade de capitulation très sélectif: série, RSI/z-score extrêmes et bougie violente.
pub struct ReversalPro {
    candles: VecDeque<Candle>,
    rsi: RsiState,
    atr: AtrState,
    last_z: Option<f64>,
}

impl ReversalPro {
    pub fn new() -> Self {
        Self {
            candles: VecDeque::with_capacity(Z_PERIOD),
            rsi: RsiState::new(7),
            atr: AtrState::new(14),
            last_z: None,
        }
    }

    fn feed(&mut self, candle: &Candle) {
        self.rsi.update(candle.close);
        self.atr.update(candle);
        push_candle(&mut self.candles, candle, Z_PERIOD);
        self.last_z = sample_close_z(&self.candles, Z_PERIOD);
    }

    fn evaluate(&self, candle: &Candle) -> Option<Prediction> {
        let rsi = self.rsi.get()?;
        let atr = self.atr.raw()?;
        let z = self.last_z?;
        let series_is_green = same_color_streak(&self.candles, 3)?;
        let ratio = body_ratio(candle)?;
        let range_ok = candle.high - candle.low >= 1.5 * atr;
        if !range_ok || ratio < 0.60 {
            return None;
        }

        if series_is_green && rsi >= 75.0 && z >= 2.0 {
            Some(Prediction::Down)
        } else if !series_is_green && rsi <= 25.0 && z <= -2.0 {
            Some(Prediction::Up)
        } else {
            None
        }
    }
}

impl Default for ReversalPro {
    fn default() -> Self {
        Self::new()
    }
}

impl Strategy for ReversalPro {
    fn name(&self) -> &str {
        "reversal_pro"
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        self.feed(candle);
        let prediction = self.evaluate(candle)?;
        Some(Signal {
            prediction,
            signal_candle_close_time: candle.close_time,
            rsi: self.rsi.get()?,
            strategy_name: self.name().to_string(),
        })
    }

    fn warmup(&mut self, candle: &Candle) {
        self.feed(candle);
    }

    fn current_rsi(&self) -> Option<f64> {
        self.rsi.get()
    }

    fn current_series(&self) -> Option<bool> {
        same_color_streak(&self.candles, 3)
    }

    fn current_atr(&self) -> Option<f64> {
        self.atr.raw()
    }

    fn candle_log_extras(&self) -> String {
        format!(
            "rsi7={} | atr14={} | z24={}",
            self.rsi
                .get()
                .map_or_else(|| "N/A".to_string(), |value| format!("{value:.2}")),
            self.atr
                .raw()
                .map_or_else(|| "N/A".to_string(), |value| format!("{value:.4}")),
            self.last_z
                .map_or_else(|| "N/A".to_string(), |value| format!("{value:.3}")),
        )
    }
}

/// Composite qui ne signale que lorsque au moins deux des trois fades indépendants concordent.
pub struct TrioVote2 {
    boll: BollFade,
    streak: StreakRsi,
    donch: DonchZscore,
    last_vote_pct: Option<f64>,
}

impl TrioVote2 {
    pub fn new() -> Self {
        Self {
            boll: BollFade::new(),
            streak: StreakRsi::new(),
            donch: DonchZscore::new(),
            last_vote_pct: None,
        }
    }

    fn vote(&mut self, candle: &Candle, warmup: bool) -> Option<Signal> {
        if warmup {
            self.boll.warmup(candle);
            self.streak.warmup(candle);
            self.donch.warmup(candle);
            return None;
        }

        let votes = [
            self.boll.on_closed_candle(candle),
            self.streak.on_closed_candle(candle),
            self.donch.on_closed_candle(candle),
        ];
        let total = votes.iter().flatten().count();
        let up = votes
            .iter()
            .flatten()
            .filter(|signal| signal.prediction == Prediction::Up)
            .count();
        let down = total - up;
        let dominant = up.max(down);
        self.last_vote_pct = (total > 0).then(|| dominant as f64 * 100.0 / total as f64);

        if total < 2 || up == down {
            return None;
        }

        Some(Signal {
            prediction: if up > down {
                Prediction::Up
            } else {
                Prediction::Down
            },
            signal_candle_close_time: candle.close_time,
            rsi: self.last_vote_pct.unwrap_or(0.0),
            strategy_name: self.name().to_string(),
        })
    }
}

impl Default for TrioVote2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Strategy for TrioVote2 {
    fn name(&self) -> &str {
        "trio_vote2"
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        self.vote(candle, false)
    }

    fn warmup(&mut self, candle: &Candle) {
        let _ = self.vote(candle, true);
    }

    fn current_rsi(&self) -> Option<f64> {
        self.last_vote_pct
    }

    fn current_series(&self) -> Option<bool> {
        self.streak.current_series()
    }

    fn current_atr(&self) -> Option<f64> {
        self.streak.current_atr()
    }

    fn candle_log_extras(&self) -> String {
        self.last_vote_pct.map_or_else(
            || "votes=N/A".to_string(),
            |value| format!("votes={value:.1}%"),
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::{BollFade, DonchZscore, ReversalPro, StreakRsi, TrioVote2};
    use crate::binance::Candle;
    use crate::strategy::{Prediction, Strategy};

    fn candle(open: f64, high: f64, low: f64, close: f64, offset: i64) -> Candle {
        let open_time = Utc::now() + Duration::minutes(offset);
        Candle {
            open_time,
            close_time: open_time + Duration::minutes(5) - Duration::milliseconds(1),
            open,
            high,
            low,
            close,
            volume: 1.0,
            is_closed: true,
        }
    }

    #[test]
    fn boll_fade_signals_up_after_red_close_below_lower_band() {
        let mut strategy = BollFade::new();
        for offset in 0..20 {
            strategy.warmup(&candle(100.0, 101.0, 99.0, 100.0, offset));
        }

        let signal = strategy
            .on_closed_candle(&candle(100.0, 101.0, 89.0, 90.0, 21))
            .expect("capitulation below the band should signal");

        assert_eq!(signal.prediction, Prediction::Up);
    }

    #[test]
    fn streak_rsi_signals_up_after_red_capitulation() {
        let mut strategy = StreakRsi::new();
        for offset in 0..16 {
            let close = 120.0 - offset as f64;
            strategy.warmup(&candle(
                close + 1.0,
                close + 1.2,
                close - 0.2,
                close,
                offset,
            ));
        }

        let signal = strategy
            .on_closed_candle(&candle(105.0, 105.5, 100.5, 101.0, 17))
            .expect("three red candles, low RSI and wide body should signal");

        assert_eq!(signal.prediction, Prediction::Up);
    }

    #[test]
    fn donch_zscore_signals_up_at_recent_low_with_negative_pressure() {
        let mut strategy = DonchZscore::new();
        for offset in 0..23 {
            strategy.warmup(&candle(100.0, 100.2, 99.8, 100.0, offset));
        }

        let signal = strategy
            .on_closed_candle(&candle(100.0, 100.5, 90.0, 90.0, 24))
            .expect("recent low with strong negative z-score should signal");

        assert_eq!(signal.prediction, Prediction::Up);
    }

    #[test]
    fn reversal_pro_requires_all_capitulation_filters() {
        let mut strategy = ReversalPro::new();
        for offset in 0..21 {
            strategy.warmup(&candle(100.0, 100.2, 99.8, 100.0, offset));
        }
        strategy.warmup(&candle(100.0, 100.5, 98.5, 99.0, 22));
        strategy.warmup(&candle(99.0, 99.5, 97.5, 98.0, 23));

        let signal = strategy
            .on_closed_candle(&candle(98.0, 98.5, 88.5, 90.0, 24))
            .expect("full reversal-pro capitulation should signal");

        assert_eq!(signal.prediction, Prediction::Up);
    }

    #[test]
    fn trio_vote2_emits_when_multiple_voters_agree() {
        let mut strategy = TrioVote2::new();
        for offset in 0..20 {
            strategy.warmup(&candle(100.0, 100.2, 99.8, 100.0, offset));
        }
        strategy.warmup(&candle(100.0, 100.5, 98.5, 99.0, 21));
        strategy.warmup(&candle(99.0, 99.5, 97.5, 98.0, 22));

        let signal = strategy
            .on_closed_candle(&candle(98.0, 98.5, 88.5, 90.0, 23))
            .expect("at least two contrarian voters should agree");

        assert_eq!(signal.prediction, Prediction::Up);
    }
}
