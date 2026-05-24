use chrono::Datelike;
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategies::indicators::{MacdState, RsiState};
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 145;
const STRATEGY_NAME: &str = "eth_1h_rules_17_min_votes_1";

struct HaState {
    ha_open: Option<f64>,
    ha_close: Option<f64>,
}

impl HaState {
    fn new() -> Self {
        Self {
            ha_open: None,
            ha_close: None,
        }
    }

    fn update(&mut self, c: &Candle) {
        let new_hc = (c.open + c.high + c.low + c.close) / 4.0;
        let new_ho = match (self.ha_open, self.ha_close) {
            (Some(ho), Some(hc)) => (ho + hc) / 2.0,
            _ => (c.open + c.close) / 2.0,
        };
        self.ha_open = Some(new_ho);
        self.ha_close = Some(new_hc);
    }

    fn body(&self, close: f64) -> Option<f64> {
        let (ho, hc) = (self.ha_open?, self.ha_close?);
        if close < 1e-12 {
            Some(0.0)
        } else {
            Some((hc - ho) / close)
        }
    }

    fn ratio(&self, c: &Candle) -> Option<f64> {
        let (ho, hc) = (self.ha_open?, self.ha_close?);
        let hh = c.high.max(ho).max(hc);
        let hl = c.low.min(ho).min(hc);
        let range = hh - hl;
        if range < 1e-12 {
            return Some(0.0);
        }
        Some((hc - ho).abs() / range)
    }

    fn close_position(&self, c: &Candle) -> Option<f64> {
        let (ho, hc) = (self.ha_open?, self.ha_close?);
        let hh = c.high.max(ho).max(hc);
        let hl = c.low.min(ho).min(hc);
        let range = hh - hl;
        if range < 1e-12 {
            return Some(0.5);
        }
        Some((hc - hl) / range)
    }
}

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

fn close_z(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let v: Vec<f64> = buf.iter().rev().take(n).map(|c| c.close).collect();
    let s = fstd_s(&v);
    Some(if s == 0.0 {
        0.0
    } else {
        (v[0] - fmean(&v)) / s
    })
}

fn donch_low(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let min_low = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    if min_low <= 0.0 {
        return None;
    }
    Some(close / min_low - 1.0)
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

fn bb_pctb(buf: &VecDeque<Candle>) -> Option<f64> {
    if buf.len() < 20 {
        return None;
    }
    let v: Vec<f64> = buf.iter().rev().take(20).map(|c| c.close).collect();
    let m = fmean(&v);
    let s = fstd_s(&v);
    if s == 0.0 {
        return Some(0.5);
    }
    let upper = m + 2.0 * s;
    let lower = m - 2.0 * s;
    let band = upper - lower;
    if band == 0.0 {
        return Some(0.5);
    }
    Some((v[0] - lower) / band)
}

fn body_sum(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    Some(
        buf.iter()
            .rev()
            .take(n)
            .map(|c| {
                if c.close != 0.0 {
                    (c.close - c.open) / c.close
                } else {
                    0.0
                }
            })
            .sum::<f64>(),
    )
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

fn cci(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let tps: Vec<f64> = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| (c.high + c.low + c.close) / 3.0)
        .collect();
    let m = fmean(&tps);
    let md = tps.iter().map(|x| (x - m).abs()).sum::<f64>() / n as f64;
    if md == 0.0 {
        return Some(0.0);
    }
    Some((tps[0] - m) / (0.015 * md))
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

// 0=donch_low72, 1=macd_hist_pct, 2=bb_pctb, 3=rsi7, 4=ha_body,
// 5=mfi21, 6=weekday, 7=donch_low144, 8=body_sum12, 9=mfi14,
// 10=stoch_k72, 11=donch_high12, 12=rsi21, 13=close_position,
// 14=ha_close_position, 15=cci12, 16=stoch_k24, 17=mfi8, 18=rsi8,
// 19=macd_pct, 20=close_z48, 21=lower_wick_body, 22=close_z24,
// 23=ha_body_ratio, 24=donch_high72
struct Feats {
    f: [Option<f64>; 25],
}

impl Feats {
    fn get(&self, id: u8) -> Option<f64> {
        self.f[id as usize]
    }
}

fn compute_feats(
    buf: &VecDeque<Candle>,
    rsi7: &RsiState,
    rsi8: &RsiState,
    rsi21: &RsiState,
    macd: &MacdState,
    ha: &HaState,
) -> Feats {
    let cur = match buf.back() {
        Some(c) => c,
        None => return Feats { f: [None; 25] },
    };
    let close = cur.close;
    let range = cur.high - cur.low;
    let body_size = (cur.close - cur.open).abs();
    let close_position = if range < 1e-12 {
        0.5
    } else {
        (cur.close - cur.low) / range
    };
    let lower_wick_body = if body_size < 1e-10 {
        None
    } else {
        Some((cur.open.min(cur.close) - cur.low) / body_size)
    };

    let mut f: [Option<f64>; 25] = [None; 25];
    f[0] = donch_low(buf, 72, close);
    f[1] = macd.hist_pct(close);
    f[2] = bb_pctb(buf);
    f[3] = rsi7.get();
    f[4] = ha.body(close);
    f[5] = mfi(buf, 21);
    f[6] = Some(cur.close_time.weekday().num_days_from_monday() as f64);
    f[7] = donch_low(buf, 144, close);
    f[8] = body_sum(buf, 12);
    f[9] = mfi(buf, 14);
    f[10] = stoch_k(buf, 72, close);
    f[11] = donch_high(buf, 12, close);
    f[12] = rsi21.get();
    f[13] = Some(close_position);
    f[14] = ha.close_position(cur);
    f[15] = cci(buf, 12);
    f[16] = stoch_k(buf, 24, close);
    f[17] = mfi(buf, 8);
    f[18] = rsi8.get();
    f[19] = macd.line_pct(close);
    f[20] = close_z(buf, 48);
    f[21] = lower_wick_body;
    f[22] = close_z(buf, 24);
    f[23] = ha.ratio(cur);
    f[24] = donch_high(buf, 72, close);
    Feats { f }
}

type Rule = (bool, &'static [(u8, u8, f64)]);

fn cmp_ok(val: f64, op: u8, thr: f64) -> bool {
    match op {
        0 => val >= thr,
        1 => val <= thr,
        _ => (val - thr).abs() < 1e-9,
    }
}

fn rule_fires(feats: &Feats, rule: &Rule) -> Option<bool> {
    for &(id, op, thr) in rule.1 {
        let v = feats.get(id)?;
        if !cmp_ok(v, op, thr) {
            return None;
        }
    }
    Some(rule.0)
}

static RULES: &[Rule] = &[
    (
        true,
        &[
            (0, 1, 0.005558113318),
            (1, 1, -0.002259446914),
            (2, 0, 0.04033662233),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.006837778067),
            (3, 1, 17.03206178),
            (4, 0, -0.01017634652),
        ],
    ),
    (
        true,
        &[(5, 1, 20.65922058), (6, 2, 2.0), (3, 1, 33.60114385)],
    ),
    (
        true,
        &[
            (7, 1, 0.005548156292),
            (8, 1, -0.03657074613),
            (9, 1, 21.44107346),
        ],
    ),
    (
        true,
        &[
            (5, 1, 20.65922058),
            (10, 1, 8.138635),
            (11, 0, -0.04766910986),
        ],
    ),
    (
        true,
        &[
            (12, 1, 29.1932855),
            (13, 1, 0.1724014402),
            (14, 0, 0.3840913291),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.006837778067),
            (9, 1, 16.05947179),
            (15, 0, -112.7701187),
        ],
    ),
    (
        true,
        &[
            (16, 1, 7.980198437),
            (17, 1, 18.38030246),
            (18, 1, 12.50217521),
        ],
    ),
    (
        true,
        &[
            (16, 1, 7.980198437),
            (19, 1, -0.01229544773),
            (9, 1, 18.18947095),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.005558113318),
            (1, 1, -0.003023911405),
            (20, 0, -2.500795018),
        ],
    ),
    (
        true,
        &[
            (9, 1, 21.44107346),
            (13, 1, 0.1223925466),
            (21, 0, 0.06465758156),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.003842653373),
            (18, 1, 24.91006406),
            (22, 0, -2.099858976),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.006837778067),
            (18, 1, 18.79011376),
            (22, 0, -2.099858976),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.006837778067),
            (3, 1, 23.29241515),
            (2, 0, 0.08661248075),
        ],
    ),
    (
        true,
        &[
            (7, 1, 0.005548156292),
            (3, 1, 17.03206178),
            (23, 1, 0.6571549533),
        ],
    ),
    (
        true,
        &[
            (0, 1, 0.006837778067),
            (3, 1, 13.5929322),
            (22, 0, -2.584346456),
        ],
    ),
    (
        true,
        &[
            (5, 1, 20.65922058),
            (10, 1, 8.138635),
            (24, 1, -0.1118632156),
        ],
    ),
];

pub struct EthH1Rules17 {
    buffer: VecDeque<Candle>,
    min_votes: u32,
    rsi7: RsiState,
    rsi8: RsiState,
    rsi21: RsiState,
    macd: MacdState,
    ha: HaState,
    last_votes: (u32, u32),
}

impl EthH1Rules17 {
    pub fn new(min_votes: u32) -> Self {
        Self {
            buffer: VecDeque::with_capacity(MAX_WINDOW + 1),
            min_votes,
            rsi7: RsiState::new(7),
            rsi8: RsiState::new(8),
            rsi21: RsiState::new(21),
            macd: MacdState::new(),
            ha: HaState::new(),
            last_votes: (0, 0),
        }
    }

    fn feed(&mut self, candle: &Candle) {
        self.rsi7.update(candle.close);
        self.rsi8.update(candle.close);
        self.rsi21.update(candle.close);
        self.macd.update(candle.close);
        self.ha.update(candle);
        self.buffer.push_back(candle.clone());
        if self.buffer.len() > MAX_WINDOW {
            self.buffer.pop_front();
        }
    }

    fn vote(&mut self) -> (u32, u32) {
        let feats = compute_feats(
            &self.buffer,
            &self.rsi7,
            &self.rsi8,
            &self.rsi21,
            &self.macd,
            &self.ha,
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

impl Strategy for EthH1Rules17 {
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
        None
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
