use chrono::{TimeZone, Utc};
use rusty_poly_signal_runner::binance::Candle;
use rusty_poly_signal_runner::strategies::btc_15m_rules_18_min_votes_1::BtcRules18;
use rusty_poly_signal_runner::strategies::btc_5m_rules_90_min_votes_1::BtcRules90;
use rusty_poly_signal_runner::strategies::eth_15m_rules_24_min_votes_1::EthRules24;
use rusty_poly_signal_runner::strategies::eth_5m_rules_25_min_votes_1::EthRules25;
use rusty_poly_signal_runner::strategy::Strategy;

fn flat_fixture(len: usize) -> Vec<Candle> {
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    (0..len)
        .map(|i| {
            let open_time = start + chrono::Duration::minutes(i as i64 * 5);
            Candle {
                open_time,
                close_time: open_time + chrono::Duration::minutes(5),
                open: 100.0,
                high: 100.1,
                low: 99.9,
                close: 100.0,
                volume: 1_000.0,
                is_closed: true,
            }
        })
        .collect()
}

fn wave_fixture(len: usize, base: f64, step: f64) -> Vec<Candle> {
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let mut price = base;

    (0..len)
        .map(|i| {
            let open_time = start + chrono::Duration::minutes(i as i64 * 5);
            let pullback = if i % 17 < 5 { -step * 1.8 } else { step };
            let impulse = if i % 43 == 0 { step * 4.0 } else { 0.0 };
            let open = price;
            let close = (open + pullback + impulse).max(1.0);
            let range = (close - open).abs().max(step) * 0.35;
            price = close;

            Candle {
                open_time,
                close_time: open_time + chrono::Duration::minutes(5),
                open,
                high: open.max(close) + range,
                low: open.min(close) - range,
                close,
                volume: 1_000.0 + (i % 29) as f64 * 37.0,
                is_closed: true,
            }
        })
        .collect()
}

fn collect_signal_count(strategy: &mut dyn Strategy, candles: &[Candle]) -> usize {
    candles
        .iter()
        .filter_map(|candle| strategy.on_closed_candle(candle))
        .count()
}

#[test]
fn generated_strategies_do_not_signal_on_flat_fixture() {
    let candles = flat_fixture(180);
    let mut strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(BtcRules18::new(1)),
        Box::new(BtcRules90::new(1)),
        Box::new(EthRules24::new(1)),
        Box::new(EthRules25::new(1)),
    ];

    let summaries: Vec<_> = strategies
        .iter_mut()
        .map(|strategy| {
            (
                strategy.name().to_string(),
                collect_signal_count(strategy.as_mut(), &candles),
            )
        })
        .collect();

    assert_eq!(
        summaries,
        vec![
            ("btc_15m_rules_18_min_votes_1".to_string(), 0),
            ("btc_5m_rules_90_min_votes_1".to_string(), 0),
            ("eth_15m_rules_24_min_votes_1".to_string(), 0),
            ("eth_5m_rules_25_min_votes_1".to_string(), 0),
        ]
    );
}

#[test]
fn generated_strategies_signal_snapshot_on_btc_wave_fixture() {
    let candles = wave_fixture(220, 42_000.0, 42.0);
    let mut strategies: Vec<Box<dyn Strategy>> =
        vec![Box::new(BtcRules18::new(1)), Box::new(BtcRules90::new(1))];

    let summaries: Vec<_> = strategies
        .iter_mut()
        .map(|strategy| {
            (
                strategy.name().to_string(),
                collect_signal_count(strategy.as_mut(), &candles),
            )
        })
        .collect();

    assert_eq!(
        summaries,
        vec![
            ("btc_15m_rules_18_min_votes_1".to_string(), 0),
            ("btc_5m_rules_90_min_votes_1".to_string(), 0),
        ]
    );
}

#[test]
fn generated_strategies_signal_snapshot_on_eth_wave_fixture() {
    let candles = wave_fixture(220, 3_000.0, 3.5);
    let mut strategies: Vec<Box<dyn Strategy>> =
        vec![Box::new(EthRules24::new(1)), Box::new(EthRules25::new(1))];

    let summaries: Vec<_> = strategies
        .iter_mut()
        .map(|strategy| {
            (
                strategy.name().to_string(),
                collect_signal_count(strategy.as_mut(), &candles),
            )
        })
        .collect();

    assert_eq!(
        summaries,
        vec![
            ("eth_15m_rules_24_min_votes_1".to_string(), 0),
            ("eth_5m_rules_25_min_votes_1".to_string(), 0),
        ]
    );
}
