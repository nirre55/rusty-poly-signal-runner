use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategies::indicators::{AtrState, MacdState, RsiState};
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 100;
const STRATEGY_NAME: &str = "btc_1h_rules_15_min_votes_1";

fn fmean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

fn fstd_s(v: &[f64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let m = fmean(v);
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt()
}

fn vol_z(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let v: Vec<f64> = buf.iter().rev().take(n).map(|c| c.volume).collect();
    let s = fstd_s(&v);
    Some(if s == 0.0 {
        0.0
    } else {
        (v[0] - fmean(&v)) / s
    })
}

fn stoch_k(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let min_l = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    let max_h = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let range = max_h - min_l;
    Some(if range == 0.0 {
        50.0
    } else {
        (close - min_l) / range * 100.0
    })
}

fn donch_high(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let max_high = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    if max_high <= 0.0 {
        return None;
    }
    Some(close / max_high - 1.0)
}

fn ret_n(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    let needed = n + 1;
    if buf.len() < needed {
        return None;
    }
    let cur = buf[buf.len() - 1].close;
    let past = buf[buf.len() - 1 - n].close;
    if past == 0.0 {
        None
    } else {
        Some(cur / past - 1.0)
    }
}

fn mfi(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n + 1 {
        return None;
    }
    let start = buf.len() - n - 1;
    let (mut pos, mut neg) = (0.0f64, 0.0f64);
    for i in (start + 1)..buf.len() {
        let prev_tp = (buf[i - 1].high + buf[i - 1].low + buf[i - 1].close) / 3.0;
        let curr_tp = (buf[i].high + buf[i].low + buf[i].close) / 3.0;
        let rmf = curr_tp * buf[i].volume;
        if curr_tp > prev_tp {
            pos += rmf;
        } else if curr_tp < prev_tp {
            neg += rmf;
        }
    }
    Some(if neg == 0.0 {
        if pos == 0.0 {
            50.0
        } else {
            100.0
        }
    } else {
        100.0 - 100.0 / (1.0 + pos / neg)
    })
}

fn volume_ratio(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let sma = buf.iter().rev().take(n).map(|c| c.volume).sum::<f64>() / n as f64;
    if sma < 1e-12 {
        return None;
    }
    Some(buf.back()?.volume / sma)
}

fn red_streak(buf: &VecDeque<Candle>) -> f64 {
    let mut count = 0u32;
    for c in buf.iter().rev() {
        if c.close < c.open {
            count += 1;
        } else {
            break;
        }
    }
    count as f64
}

fn green_streak(buf: &VecDeque<Candle>) -> f64 {
    let mut count = 0u32;
    for c in buf.iter().rev() {
        if c.close >= c.open {
            count += 1;
        } else {
            break;
        }
    }
    count as f64
}

// 0=stoch_k12, 1=hour, 2=body_abs_pct, 3=volume_z96, 4=lower_wick_body,
// 5=stoch_k24, 6=donch_high12, 7=atr14_pct, 8=stoch_k72, 9=rsi7,
// 10=rsi21, 11=rsi14, 12=red_streak, 13=macd_hist_pct, 14=mfi8,
// 15=ret24, 16=body, 17=volume_ratio20, 18=range_atr14, 19=body_ratio,
// 20=weekday, 21=green_streak
struct Feats {
    f: [Option<f64>; 22],
}

impl Feats {
    fn get(&self, id: u8) -> Option<f64> {
        self.f[id as usize]
    }
}

fn compute_feats(
    buf: &VecDeque<Candle>,
    rsi7: &RsiState,
    rsi14: &RsiState,
    rsi21: &RsiState,
    atr14: &AtrState,
    macd: &MacdState,
) -> Feats {
    let cur = match buf.back() {
        Some(c) => c,
        None => return Feats { f: [None; 22] },
    };
    let close = cur.close;
    let range = cur.high - cur.low;
    let body_size = (cur.close - cur.open).abs();
    let body = if close < 1e-12 {
        0.0
    } else {
        (cur.close - cur.open) / close
    };
    let body_abs_pct = body.abs();
    let body_ratio = if range < 1e-12 {
        0.0
    } else {
        body_size / range
    };
    let lower_wick_body = if body_size < 1e-10 {
        None
    } else {
        Some((cur.open.min(cur.close) - cur.low) / body_size)
    };

    let mut f: [Option<f64>; 22] = [None; 22];
    f[0] = stoch_k(buf, 12, close);
    f[1] = Some(cur.close_time.hour() as f64);
    f[2] = Some(body_abs_pct);
    f[3] = vol_z(buf, 96);
    f[4] = lower_wick_body;
    f[5] = stoch_k(buf, 24, close);
    f[6] = donch_high(buf, 12, close);
    f[7] = atr14.pct(close);
    f[8] = stoch_k(buf, 72, close);
    f[9] = rsi7.get();
    f[10] = rsi21.get();
    f[11] = rsi14.get();
    f[12] = Some(red_streak(buf));
    f[13] = macd.hist_pct(close);
    f[14] = mfi(buf, 8);
    f[15] = ret_n(buf, 24);
    f[16] = Some(body);
    f[17] = volume_ratio(buf, 20);
    f[18] = atr14.raw().map(|a| if a < 1e-12 { 0.0 } else { range / a });
    f[19] = Some(body_ratio);
    f[20] = Some(cur.close_time.weekday().num_days_from_monday() as f64);
    f[21] = Some(green_streak(buf));
    Feats { f }
}

// cmp: 0=GE, 1=LE, 2=EQ, 3=BETWEEN_INCLUSIVE
type Cond = (u8, u8, f64, f64);
type Rule = (bool, &'static [Cond]);

fn cmp_ok(val: f64, op: u8, a: f64, b: f64) -> bool {
    match op {
        0 => val >= a,
        1 => val <= a,
        2 => (val - a).abs() < 1e-9,
        _ => val >= a && val <= b,
    }
}

fn rule_fires(feats: &Feats, rule: &Rule) -> Option<bool> {
    for &(id, op, a, b) in rule.1 {
        let v = feats.get(id)?;
        if !cmp_ok(v, op, a, b) {
            return None;
        }
    }
    Some(rule.0)
}

static RULES: &[Rule] = &[
    (
        true,
        &[
            (0, 1, 13.40911567, 0.0),
            (1, 2, 19.0, 0.0),
            (2, 1, 0.007181889216, 0.0),
        ],
    ),
    (
        true,
        &[
            (0, 1, 13.40911567, 0.0),
            (3, 1, -0.8641092984, 0.0),
            (4, 0, 0.1106917761, 0.0),
        ],
    ),
    (
        true,
        &[
            (5, 1, 7.662631226, 0.0),
            (6, 1, -0.04360964035, 0.0),
            (7, 1, 0.01278625059, 0.0),
        ],
    ),
    (
        true,
        &[
            (8, 1, 3.661654758, 0.0),
            (9, 1, 19.79139536, 0.0),
            (10, 0, 26.77185052, 0.0),
        ],
    ),
    (
        true,
        &[
            (11, 1, 27.69290095, 0.0),
            (5, 1, 3.478803314, 0.0),
            (12, 1, 3.0, 0.0),
        ],
    ),
    (
        true,
        &[
            (13, 1, -0.002650404386, 0.0),
            (0, 1, 5.152344313, 0.0),
            (5, 1, 3.478803314, 0.0),
        ],
    ),
    (
        true,
        &[
            (8, 1, 3.661654758, 0.0),
            (14, 1, 15.33077261, 0.0),
            (15, 0, -0.05828397287, 0.0),
        ],
    ),
    (
        true,
        &[
            (16, 1, 0.0, 0.0),
            (9, 1, 35.0, 0.0),
            (4, 0, 1.25, 0.0),
            (17, 0, 1.0, 0.0),
            (1, 3, 19.0, 23.0),
        ],
    ),
    (
        true,
        &[
            (12, 0, 2.0, 0.0),
            (9, 1, 35.0, 0.0),
            (18, 0, 0.8, 0.0),
            (19, 0, 0.6, 0.0),
            (1, 3, 19.0, 23.0),
        ],
    ),
    (
        true,
        &[
            (12, 0, 3.0, 0.0),
            (9, 1, 25.0, 0.0),
            (18, 0, 0.8, 0.0),
            (19, 0, 0.45, 0.0),
            (20, 2, 3.0, 0.0),
        ],
    ),
    (
        false,
        &[
            (21, 0, 2.0, 0.0),
            (9, 0, 60.0, 0.0),
            (18, 0, 1.2, 0.0),
            (19, 0, 0.6, 0.0),
            (20, 2, 6.0, 0.0),
        ],
    ),
    (
        true,
        &[
            (12, 0, 2.0, 0.0),
            (9, 1, 25.0, 0.0),
            (18, 0, 1.2, 0.0),
            (19, 0, 0.45, 0.0),
            (20, 2, 6.0, 0.0),
        ],
    ),
    (
        false,
        &[
            (21, 0, 2.0, 0.0),
            (9, 0, 65.0, 0.0),
            (18, 0, 0.8, 0.0),
            (19, 0, 0.6, 0.0),
            (20, 2, 3.0, 0.0),
        ],
    ),
    (
        true,
        &[
            (12, 0, 2.0, 0.0),
            (9, 1, 35.0, 0.0),
            (18, 0, 1.5, 0.0),
            (19, 0, 0.6, 0.0),
            (1, 3, 0.0, 7.0),
        ],
    ),
    (
        false,
        &[
            (21, 0, 2.0, 0.0),
            (9, 0, 70.0, 0.0),
            (18, 0, 0.8, 0.0),
            (19, 0, 0.75, 0.0),
            (1, 3, 7.0, 12.0),
        ],
    ),
];

pub struct BtcH1Rules15 {
    buffer: VecDeque<Candle>,
    min_votes: u32,
    rsi7: RsiState,
    rsi14: RsiState,
    rsi21: RsiState,
    atr14: AtrState,
    macd: MacdState,
    last_votes: (u32, u32),
}

impl BtcH1Rules15 {
    pub fn new(min_votes: u32) -> Self {
        Self {
            buffer: VecDeque::with_capacity(MAX_WINDOW + 1),
            min_votes,
            rsi7: RsiState::new(7),
            rsi14: RsiState::new(14),
            rsi21: RsiState::new(21),
            atr14: AtrState::new(14),
            macd: MacdState::new(),
            last_votes: (0, 0),
        }
    }

    fn feed(&mut self, candle: &Candle) {
        self.rsi7.update(candle.close);
        self.rsi14.update(candle.close);
        self.rsi21.update(candle.close);
        self.atr14.update(candle);
        self.macd.update(candle.close);
        self.buffer.push_back(candle.clone());
        if self.buffer.len() > MAX_WINDOW {
            self.buffer.pop_front();
        }
    }

    fn vote(&mut self) -> (u32, u32) {
        let feats = compute_feats(
            &self.buffer,
            &self.rsi7,
            &self.rsi14,
            &self.rsi21,
            &self.atr14,
            &self.macd,
        );
        let (mut gv, mut rv) = (0u32, 0u32);
        for rule in RULES {
            if let Some(green) = rule_fires(&feats, rule) {
                if green {
                    gv += 1;
                } else {
                    rv += 1;
                }
            }
        }
        self.last_votes = (gv, rv);
        (gv, rv)
    }
}

impl Strategy for BtcH1Rules15 {
    fn name(&self) -> &str {
        STRATEGY_NAME
    }

    fn warmup(&mut self, candle: &Candle) {
        self.feed(candle);
    }

    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal> {
        self.feed(candle);
        if candle.open == candle.close {
            self.last_votes = (0, 0);
            return None;
        }

        let (gv, rv) = self.vote();
        let total = gv + rv;
        debug!(
            "[ENSEMBLE] green_votes={} red_votes={} total={} min_votes={}",
            gv, rv, total, self.min_votes
        );

        if total < self.min_votes || gv == rv {
            return None;
        }

        let prediction = if gv > rv {
            Prediction::Up
        } else {
            Prediction::Down
        };
        let vote_pct = gv.max(rv) as f64 / total as f64 * 100.0;

        Some(Signal {
            prediction,
            signal_candle_close_time: candle.close_time,
            rsi: vote_pct,
            strategy_name: self.name().to_string(),
        })
    }

    fn current_rsi(&self) -> Option<f64> {
        self.rsi7.get()
    }
    fn current_series(&self) -> Option<bool> {
        None
    }
    fn current_atr(&self) -> Option<f64> {
        self.atr14.raw()
    }
    fn candle_log_extras(&self) -> String {
        let (gv, rv) = self.last_votes;
        let total = gv + rv;
        if total == 0 {
            return format!("green=0 | red=0 | total=0 | min_votes={}", self.min_votes);
        }
        let dominant = if gv > rv { gv } else { rv };
        let pct = dominant as f64 / total as f64 * 100.0;
        format!(
            "green={} | red={} | total={} | pct={:.1}% | min_votes={}",
            gv, rv, total, pct, self.min_votes
        )
    }
}
