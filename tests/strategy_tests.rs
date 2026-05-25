use chrono::Utc;
use rusty_poly_signal_runner::binance::Candle;
use rusty_poly_signal_runner::strategies::three_candle_rsi7_reversal::ThreeCandleRsi7Reversal;
use rusty_poly_signal_runner::strategy::{Prediction, Strategy};

fn make_candle(open: f64, close: f64) -> Candle {
    // Mèche = 10% du body pour body_ratio ≈ 0.83 (> seuil 0.60)
    let wick = (close - open).abs().max(0.01) * 0.1;
    Candle {
        open_time: Utc::now(),
        close_time: Utc::now(),
        open,
        high: open.max(close) + wick,
        low: open.min(close) - wick,
        close,
        volume: 1.0,
        is_closed: true,
    }
}

fn feed(
    strategy: &mut ThreeCandleRsi7Reversal,
    candles: &[(f64, f64)],
) -> Option<rusty_poly_signal_runner::strategy::Signal> {
    let mut last = None;
    for &(open, close) in candles {
        last = strategy.on_closed_candle(&make_candle(open, close));
    }
    last
}

// ============================================================
// last_three_same_color
// ============================================================

#[test]
fn test_color_none_before_three_candles() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    s.on_closed_candle(&make_candle(100.0, 101.0));
    assert!(s.last_three_same_color().is_none());
    s.on_closed_candle(&make_candle(101.0, 102.0));
    assert!(s.last_three_same_color().is_none());
}

#[test]
fn test_color_three_green() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    s.on_closed_candle(&make_candle(100.0, 101.0));
    s.on_closed_candle(&make_candle(101.0, 102.0));
    s.on_closed_candle(&make_candle(102.0, 103.0));
    assert_eq!(s.last_three_same_color(), Some(true));
}

#[test]
fn test_color_three_red() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    s.on_closed_candle(&make_candle(103.0, 100.0));
    s.on_closed_candle(&make_candle(100.0, 98.0));
    s.on_closed_candle(&make_candle(98.0, 95.0));
    assert_eq!(s.last_three_same_color(), Some(false));
}

#[test]
fn test_color_mixed_is_none() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    s.on_closed_candle(&make_candle(100.0, 101.0)); // vert
    s.on_closed_candle(&make_candle(101.0, 99.0)); // rouge
    s.on_closed_candle(&make_candle(99.0, 100.0)); // vert
    assert!(s.last_three_same_color().is_none());
}

// ============================================================
// compute_rsi
// ============================================================

#[test]
fn test_rsi_none_with_fewer_than_eight_candles() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    // RSI_PERIOD=7 → besoin de RSI_PERIOD+1=8 bougies (7 deltas)
    for i in 0..7 {
        s.on_closed_candle(&make_candle(100.0 + i as f64, 101.0 + i as f64));
    }
    assert!(
        s.compute_rsi().is_none(),
        "7 bougies = 6 deltas : RSI impossible"
    );
    s.on_closed_candle(&make_candle(107.0, 108.0));
    assert!(
        s.compute_rsi().is_some(),
        "8 bougies = 7 deltas : RSI calculable"
    );
}

#[test]
fn test_rsi_all_gains_gives_100() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    for i in 0..=7 {
        s.on_closed_candle(&make_candle(100.0 + i as f64, 101.0 + i as f64));
    }
    assert_eq!(
        s.compute_rsi().unwrap(),
        100.0,
        "Toutes les hausses → RSI = 100"
    );
}

/// Marché plat (tous doji) : avg_loss=0 → RSI=100, comportement identique Python.
#[test]
fn test_rsi_flat_market_gives_100() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    for _ in 0..=7 {
        s.on_closed_candle(&make_candle(100.0, 100.0));
    }
    assert_eq!(
        s.compute_rsi().unwrap(),
        100.0,
        "Marché plat (avg_gain=0, avg_loss=0) → avg_loss==0 → RSI=100 (comportement Wilder/Python)"
    );
}

#[test]
fn test_rsi_all_losses_gives_zero() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    for i in 0..=7 {
        s.on_closed_candle(&make_candle(108.0 - i as f64, 107.0 - i as f64));
    }
    assert_eq!(
        s.compute_rsi().unwrap(),
        0.0,
        "Toutes les baisses → RSI = 0"
    );
}

#[test]
fn test_rsi_value_in_valid_range() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    // Alternance : 4 hausses, 3 baisses
    let candles = [
        (100., 102.),
        (102., 100.),
        (100., 102.),
        (102., 100.),
        (100., 102.),
        (102., 100.),
        (100., 102.),
        (102., 104.),
    ];
    for (o, c) in candles {
        s.on_closed_candle(&make_candle(o, c));
    }
    let rsi = s.compute_rsi().unwrap();
    assert!(
        (0.0..=100.0).contains(&rsi),
        "RSI doit être dans [0, 100], got {}",
        rsi
    );
}

// ============================================================
// on_closed_candle — signaux
// ============================================================

#[test]
fn test_no_signal_before_rsi_warmup() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    for i in 0..7 {
        let sig = s.on_closed_candle(&make_candle(100.0 + i as f64, 110.0 + i as f64));
        assert!(sig.is_none(), "Pas de signal avant le warmup RSI");
    }
}

#[test]
fn test_no_signal_without_three_same_color() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    let candles: Vec<(f64, f64)> = (0..11)
        .map(|i| {
            if i % 2 == 0 {
                (100.0, 101.0)
            } else {
                (101.0, 100.0)
            }
        })
        .collect();
    assert!(feed(&mut s, &candles).is_none());
}

/// RSI ≈ 42.86 (neutre) + 3 bougies vertes → pas de signal DOWN (RSI < 65)
#[test]
fn test_no_signal_rsi_neutral_with_green_series() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    // Seed 5 bougies rouges : closes 110, 108, 106, 104, 102
    for &close in &[110.0f64, 108.0, 106.0, 104.0, 102.0] {
        s.on_closed_candle(&make_candle(close + 3.0, close));
    }
    // 3 bougies vertes : closes 104, 106, 108
    // Fenêtre RSI = [110,108,106,104,102,104,106,108] → deltas: -2,-2,-2,-2,+2,+2,+2
    // gains=6, pertes=8 → RSI≈42.86 → < 65 → pas de signal DOWN
    let mut result = None;
    for &close in &[104.0f64, 106.0, 108.0] {
        result = s.on_closed_candle(&make_candle(close - 1.0, close));
    }
    assert!(
        result.is_none(),
        "RSI≈42.86 : aucun signal attendu malgré 3 bougies vertes"
    );
}

/// RSI Wilder ~57 (neutre) + 3 bougies vertes → pas de signal (RSI < 65)
#[test]
fn test_no_signal_rsi_in_neutral_zone() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    // Closes: 100,100,100,100,95,100,95,99,100,101,102
    // Wilder RSI après 11 bougies ≈ 57 (entre 35 et 65) → pas de signal
    let candles = [
        (100.0, 100.0),
        (100.0, 100.0),
        (100.0, 100.0), // fill
        (100.0, 100.0),
        (100.0, 95.0),
        (95.0, 100.0),
        (100.0, 95.0),
        (95.0, 99.0),
        (99.0, 100.0),
        (100.0, 101.0),
        (101.0, 102.0),
    ];
    assert!(
        feed(&mut s, &candles).is_none(),
        "RSI Wilder ~57 : aucun signal attendu"
    );
}

#[test]
fn test_signal_down_on_three_green_high_rsi() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    // 15 bougies continues à la hausse → RSI=100 ≥ 65, ATR prêt, 3 dernières vertes → DOWN
    // open[i+1] = close[i] (pas de gap → TR = high-low)
    let candles: Vec<(f64, f64)> = (0..15)
        .map(|i| (100.0 + i as f64 * 2.0, 102.0 + i as f64 * 2.0))
        .collect();
    let sig = feed(&mut s, &candles).expect("Signal DOWN attendu");
    assert_eq!(sig.prediction, Prediction::Down);
    assert!(sig.rsi >= 65.0, "RSI doit être ≥ 65, got {}", sig.rsi);
}

#[test]
fn test_signal_up_on_three_red_low_rsi() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    // 15 bougies continues à la baisse → RSI=0 ≤ 35, ATR prêt, 3 dernières rouges → UP
    let candles: Vec<(f64, f64)> = (0..15)
        .map(|i| (300.0 - i as f64 * 2.0, 298.0 - i as f64 * 2.0))
        .collect();
    let sig = feed(&mut s, &candles).expect("Signal UP attendu");
    assert_eq!(sig.prediction, Prediction::Up);
    assert!(sig.rsi <= 35.0, "RSI doit être ≤ 35, got {}", sig.rsi);
}

#[test]
fn test_signal_contains_strategy_name() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    let candles: Vec<(f64, f64)> = (0..15)
        .map(|i| (100.0 + i as f64 * 2.0, 102.0 + i as f64 * 2.0))
        .collect();
    let sig = feed(&mut s, &candles).unwrap();
    assert_eq!(sig.strategy_name, "three_candle_rsi7_reversal");
}

#[test]
fn test_signal_rsi_in_valid_range() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    let candles: Vec<(f64, f64)> = (0..15)
        .map(|i| (100.0 + i as f64 * 2.0, 102.0 + i as f64 * 2.0))
        .collect();
    let sig = feed(&mut s, &candles).unwrap();
    assert!((0.0..=100.0).contains(&sig.rsi));
}

#[test]
fn test_signal_close_time_is_not_epoch() {
    let mut s = ThreeCandleRsi7Reversal::new(65.0, 35.0);
    let candles: Vec<(f64, f64)> = (0..15)
        .map(|i| (100.0 + i as f64 * 2.0, 102.0 + i as f64 * 2.0))
        .collect();
    let sig = feed(&mut s, &candles).unwrap();
    assert!(sig.signal_candle_close_time.timestamp() > 0);
}

// ============================================================
// Prediction Display
// ============================================================

#[test]
fn test_prediction_display_up() {
    assert_eq!(Prediction::Up.to_string(), "UP");
}

#[test]
fn test_prediction_display_down() {
    assert_eq!(Prediction::Down.to_string(), "DOWN");
}
