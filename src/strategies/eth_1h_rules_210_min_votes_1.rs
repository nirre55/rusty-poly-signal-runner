#![allow(dead_code, unused_variables)]

use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "eth_1h_rules_210_min_votes_1";
const FEATURE_COUNT: usize = 74;

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

fn true_range_at(buf: &VecDeque<Candle>, idx: usize) -> Option<f64> {
    let c = buf.get(idx)?;
    let prev_close = if idx == 0 {
        c.close
    } else {
        buf.get(idx - 1)?.close
    };
    Some(
        (c.high - c.low)
            .max((c.high - prev_close).abs())
            .max((c.low - prev_close).abs()),
    )
}

fn atr_pct_sma(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n || close == 0.0 {
        return None;
    }
    let start = buf.len() - n;
    let mut sum = 0.0;
    for i in start..buf.len() {
        sum += true_range_at(buf, i)?;
    }
    Some(sum / n as f64 / close)
}

fn range_atr14(buf: &VecDeque<Candle>, cur_atr14_ewm: Option<f64>) -> Option<f64> {
    let c = buf.back()?;
    let atr = cur_atr14_ewm?;
    if atr == 0.0 {
        None
    } else {
        Some((c.high - c.low) / atr)
    }
}

struct PyRsiState {
    alpha: f64,
    last_close: Option<f64>,
    avg_gain: Option<f64>,
    avg_loss: Option<f64>,
}

impl PyRsiState {
    fn new(period: usize) -> Self {
        Self {
            alpha: 1.0 / period as f64,
            last_close: None,
            avg_gain: None,
            avg_loss: None,
        }
    }

    fn update(&mut self, close: f64) {
        if let Some(prev) = self.last_close {
            let delta = close - prev;
            let gain = delta.max(0.0);
            let loss = (-delta.min(0.0)).max(0.0);
            self.avg_gain = Some(match self.avg_gain {
                Some(prev_avg) => prev_avg + self.alpha * (gain - prev_avg),
                None => gain,
            });
            self.avg_loss = Some(match self.avg_loss {
                Some(prev_avg) => prev_avg + self.alpha * (loss - prev_avg),
                None => loss,
            });
        }
        self.last_close = Some(close);
    }

    fn get(&self) -> Option<f64> {
        let gain = self.avg_gain?;
        let loss = self.avg_loss?;
        if loss == 0.0 {
            None
        } else {
            Some(100.0 - 100.0 / (1.0 + gain / loss))
        }
    }
}

struct PyMacdState {
    ema12: Option<f64>,
    ema26: Option<f64>,
    signal: Option<f64>,
    line: Option<f64>,
    hist: Option<f64>,
}

impl PyMacdState {
    fn new() -> Self {
        Self {
            ema12: None,
            ema26: None,
            signal: None,
            line: None,
            hist: None,
        }
    }

    fn update(&mut self, close: f64) {
        const A12: f64 = 2.0 / 13.0;
        const A26: f64 = 2.0 / 27.0;
        const A9: f64 = 2.0 / 10.0;
        self.ema12 = Some(match self.ema12 {
            Some(prev) => prev + A12 * (close - prev),
            None => close,
        });
        self.ema26 = Some(match self.ema26 {
            Some(prev) => prev + A26 * (close - prev),
            None => close,
        });
        let line = self.ema12.unwrap_or(close) - self.ema26.unwrap_or(close);
        self.signal = Some(match self.signal {
            Some(prev) => prev + A9 * (line - prev),
            None => line,
        });
        self.line = Some(line);
        self.hist = Some(line - self.signal.unwrap_or(line));
    }

    fn line_pct(&self, close: f64) -> Option<f64> {
        if close == 0.0 {
            None
        } else {
            Some(self.line? / close)
        }
    }

    fn hist_pct(&self, close: f64) -> Option<f64> {
        if close == 0.0 {
            None
        } else {
            Some(self.hist? / close)
        }
    }
}

struct PyAtrEwmState {
    alpha: f64,
    last_close: Option<f64>,
    atr: Option<f64>,
}

impl PyAtrEwmState {
    fn new(period: usize) -> Self {
        Self {
            alpha: 1.0 / period as f64,
            last_close: None,
            atr: None,
        }
    }

    fn update(&mut self, candle: &Candle) {
        let prev_close = self.last_close.unwrap_or(candle.close);
        let tr = (candle.high - candle.low)
            .max((candle.high - prev_close).abs())
            .max((candle.low - prev_close).abs());
        self.atr = Some(match self.atr {
            Some(prev) => prev + self.alpha * (tr - prev),
            None => tr,
        });
        self.last_close = Some(candle.close);
    }

    fn raw(&self) -> Option<f64> {
        self.atr
    }
}

fn close_z(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let v: Vec<f64> = buf.iter().rev().take(n).map(|c| c.close).collect();
    let s = fstd_s(&v);
    if s == 0.0 {
        None
    } else {
        Some((v[0] - fmean(&v)) / s)
    }
}

fn vol_z(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let v: Vec<f64> = buf.iter().rev().take(n).map(|c| c.volume).collect();
    let s = fstd_s(&v);
    if s == 0.0 {
        None
    } else {
        Some((v[0] - fmean(&v)) / s)
    }
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
    let r = max_h - min_l;
    if r == 0.0 {
        None
    } else {
        Some((close - min_l) / r * 100.0)
    }
}

fn donch_low(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let min_l = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    if min_l <= 0.0 {
        None
    } else {
        Some(close / min_l - 1.0)
    }
}

fn donch_high(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let max_h = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    if max_h <= 0.0 {
        None
    } else {
        Some(close / max_h - 1.0)
    }
}

fn ret_n(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n + 1 {
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

fn mfi_n(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let start = buf.len() - n;
    let (mut pos, mut neg) = (0.0f64, 0.0f64);
    for i in start..buf.len() {
        let curr_tp = (buf[i].high + buf[i].low + buf[i].close) / 3.0;
        let rmf = curr_tp * buf[i].volume;
        if i == 0 {
            continue;
        }
        let prev_tp = (buf[i - 1].high + buf[i - 1].low + buf[i - 1].close) / 3.0;
        if curr_tp > prev_tp {
            pos += rmf;
        } else if curr_tp < prev_tp {
            neg += rmf;
        }
    }
    if neg == 0.0 {
        None
    } else {
        Some(100.0 - 100.0 / (1.0 + pos / neg))
    }
}

fn volume_ratio(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let sma = buf.iter().rev().take(n).map(|c| c.volume).sum::<f64>() / n as f64;
    if sma < 1e-12 {
        None
    } else {
        Some(buf.back()?.volume / sma)
    }
}

fn dist_sma(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let sma = buf.iter().rev().take(n).map(|c| c.close).sum::<f64>() / n as f64;
    if sma == 0.0 {
        None
    } else {
        Some(close / sma - 1.0)
    }
}

fn typical_price_mean_ending_at(buf: &VecDeque<Candle>, end: usize, n: usize) -> Option<f64> {
    if end + 1 < n {
        return None;
    }
    let start = end + 1 - n;
    let sum = (start..=end)
        .map(|idx| {
            let c = &buf[idx];
            (c.high + c.low + c.close) / 3.0
        })
        .sum::<f64>();
    Some(sum / n as f64)
}

fn cci_n(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < (2 * n).saturating_sub(1) {
        return None;
    }
    let cur_idx = buf.len() - 1;
    let current_tp = (buf[cur_idx].high + buf[cur_idx].low + buf[cur_idx].close) / 3.0;
    let current_mean = typical_price_mean_ending_at(buf, cur_idx, n)?;
    let start = cur_idx + 1 - n;
    let mut deviation_sum = 0.0;
    for idx in start..=cur_idx {
        let tp = (buf[idx].high + buf[idx].low + buf[idx].close) / 3.0;
        let mean = typical_price_mean_ending_at(buf, idx, n)?;
        deviation_sum += (tp - mean).abs();
    }
    let mean_deviation = deviation_sum / n as f64;
    if mean_deviation == 0.0 {
        None
    } else {
        Some((current_tp - current_mean) / (0.015 * mean_deviation))
    }
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

fn bb_pctb(buf: &VecDeque<Candle>) -> Option<f64> {
    if buf.len() < 20 {
        return None;
    }
    let v: Vec<f64> = buf.iter().rev().take(20).map(|c| c.close).collect();
    let m = fmean(&v);
    let s = fstd_s(&v);
    if s == 0.0 {
        return None;
    }
    Some((v[0] - (m - 2.0 * s)) / (4.0 * s))
}

fn williams_r(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let highest = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let lowest = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    let range = highest - lowest;
    if range == 0.0 {
        None
    } else {
        let close = buf.back()?.close;
        Some((highest - close) / range * -100.0)
    }
}

fn red_streak(buf: &VecDeque<Candle>) -> f64 {
    let mut n = 0u32;
    for c in buf.iter().rev() {
        if c.close < c.open {
            n += 1;
        } else {
            break;
        }
    }
    n as f64
}

fn green_streak(buf: &VecDeque<Candle>) -> f64 {
    let mut n = 0u32;
    for c in buf.iter().rev() {
        if c.close > c.open {
            n += 1;
        } else {
            break;
        }
    }
    n as f64
}

fn count_color(buf: &VecDeque<Candle>, n: usize, green: bool) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    Some(
        buf.iter()
            .rev()
            .take(n)
            .filter(|c| {
                if green {
                    c.close > c.open
                } else {
                    c.close < c.open
                }
            })
            .count() as f64,
    )
}

fn same_color_ratio(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let mut green = 0usize;
    let mut red = 0usize;
    for c in buf.iter().rev().take(n) {
        if c.close > c.open {
            green += 1;
        } else if c.close < c.open {
            red += 1;
        }
    }
    Some(green.max(red) as f64 / n as f64)
}

fn session_asia(minute_of_day: f64) -> f64 {
    if (0.0..480.0).contains(&minute_of_day) {
        1.0
    } else {
        0.0
    }
}

fn session_london(minute_of_day: f64) -> f64 {
    if (420.0..960.0).contains(&minute_of_day) {
        1.0
    } else {
        0.0
    }
}

fn session_us(minute_of_day: f64) -> f64 {
    if (780.0..1260.0).contains(&minute_of_day) {
        1.0
    } else {
        0.0
    }
}

fn session_overlap_london_us(minute_of_day: f64) -> f64 {
    if (780.0..960.0).contains(&minute_of_day) {
        1.0
    } else {
        0.0
    }
}

fn flip_count(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let start = buf.len() - n;
    let flips = (start..buf.len())
        .filter(|&i| {
            if i == 0 {
                return true;
            }
            let cur = buf[i].close.partial_cmp(&buf[i].open);
            let prev = buf[i - 1].close.partial_cmp(&buf[i - 1].open);
            cur != prev
        })
        .count();
    Some(flips as f64)
}

fn vwap_n(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let (mut tpv, mut vol) = (0.0f64, 0.0f64);
    for c in buf.iter().rev().take(n) {
        let tp = (c.high + c.low + c.close) / 3.0;
        tpv += tp * c.volume;
        vol += c.volume;
    }
    if vol < 1e-12 {
        None
    } else {
        Some(tpv / vol)
    }
}

fn dist_vwap(buf: &VecDeque<Candle>, n: usize, close: f64) -> Option<f64> {
    let v = vwap_n(buf, n)?;
    if v == 0.0 {
        None
    } else {
        Some(close / v - 1.0)
    }
}

fn vwap_slope(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    let shift = (n / 4).max(1);
    if buf.len() < n + shift {
        return None;
    }
    let current = vwap_n(buf, n)?;
    let (mut tpv, mut vol) = (0.0f64, 0.0f64);
    let start = buf.len() - shift - n;
    for c in buf.iter().skip(start).take(n) {
        let tp = (c.high + c.low + c.close) / 3.0;
        tpv += tp * c.volume;
        vol += c.volume;
    }
    if vol < 1e-12 {
        return None;
    }
    let shifted = tpv / vol;
    if shifted == 0.0 {
        None
    } else {
        Some(current / shifted - 1.0)
    }
}

fn range_pct_z(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let v: Vec<f64> = buf
        .iter()
        .rev()
        .take(n)
        .map(|c| {
            if c.close == 0.0 {
                None
            } else {
                Some((c.high - c.low) / c.close)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let s = fstd_s(&v);
    if s == 0.0 {
        None
    } else {
        Some((v[0] - fmean(&v)) / s)
    }
}

fn compression_ratio(buf: &VecDeque<Candle>, n1: usize, n2: usize) -> Option<f64> {
    if buf.len() < n2 {
        return None;
    }
    let range_pct = |c: &Candle| {
        if c.close == 0.0 {
            None
        } else {
            Some((c.high - c.low) / c.close)
        }
    };
    let r1 = buf
        .iter()
        .rev()
        .take(n1)
        .map(range_pct)
        .sum::<Option<f64>>()?
        / n1 as f64;
    let r2 = buf
        .iter()
        .rev()
        .take(n2)
        .map(range_pct)
        .sum::<Option<f64>>()?
        / n2 as f64;
    if r2 < 1e-12 {
        None
    } else {
        Some(r1 / r2)
    }
}

fn signed_vol_ratio(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n {
        return None;
    }
    let mean_v = buf.iter().rev().take(n).map(|c| c.volume).sum::<f64>() / n as f64;
    if mean_v < 1e-12 {
        return None;
    }
    let c = buf.back()?;
    let sign = if c.close > c.open {
        1.0
    } else if c.close < c.open {
        -1.0
    } else {
        0.0
    };
    Some(c.volume * sign / mean_v)
}

fn failed_low(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n + 1 {
        return Some(0.0);
    }
    let cur_idx = buf.len() - 1;
    let start = cur_idx - n;
    let prior_min = buf
        .iter()
        .skip(start)
        .take(n)
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    let cur = buf.back()?;
    Some(if cur.low < prior_min && cur.close > prior_min {
        1.0
    } else {
        0.0
    })
}

fn failed_high(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n + 1 {
        return Some(0.0);
    }
    let cur_idx = buf.len() - 1;
    let start = cur_idx - n;
    let prior_max = buf
        .iter()
        .skip(start)
        .take(n)
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let cur = buf.back()?;
    Some(if cur.high > prior_max && cur.close < prior_max {
        1.0
    } else {
        0.0
    })
}

fn absorption(buf: &VecDeque<Candle>, up: bool) -> Option<f64> {
    let c = buf.back()?;
    let vol_z24 = vol_z(buf, 24)?;
    let body_abs = (c.close - c.open).abs();
    if body_abs == 0.0 {
        return None;
    }
    let wick_body = if up {
        (c.high - c.open.max(c.close)) / body_abs
    } else {
        (c.open.min(c.close) - c.low) / body_abs
    };
    Some(vol_z24 * wick_body)
}

fn breakout_energy_f(buf: &VecDeque<Candle>) -> Option<f64> {
    Some(range_pct_z(buf, 24)? * vol_z(buf, 24)?)
}

fn vol_range_eff(buf: &VecDeque<Candle>) -> Option<f64> {
    let c = buf.back()?;
    let volume_ratio20 = volume_ratio(buf, 20)?;
    if c.close == 0.0 || volume_ratio20 == 0.0 {
        return None;
    }
    Some(((c.high - c.low) / c.close) / volume_ratio20)
}

fn vol_body_eff(buf: &VecDeque<Candle>) -> Option<f64> {
    let c = buf.back()?;
    let volume_ratio20 = volume_ratio(buf, 20)?;
    if c.close == 0.0 || volume_ratio20 == 0.0 {
        return None;
    }
    Some(((c.close - c.open).abs() / c.close) / volume_ratio20)
}

struct HaState {
    prev_open: Option<f64>,
    prev_close: Option<f64>,
    ha_body: Option<f64>,
    ha_body_ratio: Option<f64>,
    ha_close_pos: Option<f64>,
}

impl HaState {
    fn new() -> Self {
        Self {
            prev_open: None,
            prev_close: None,
            ha_body: None,
            ha_body_ratio: None,
            ha_close_pos: None,
        }
    }

    fn update(&mut self, c: &Candle) {
        let ha_close = (c.open + c.high + c.low + c.close) / 4.0;
        let ha_open = match (self.prev_open, self.prev_close) {
            (Some(po), Some(pc)) => (po + pc) / 2.0,
            _ => (c.open + c.close) / 2.0,
        };
        let ha_high = c.high.max(ha_open).max(ha_close);
        let ha_low = c.low.min(ha_open).min(ha_close);
        let ha_range = ha_high - ha_low;
        self.ha_body = if c.close == 0.0 {
            None
        } else {
            Some((ha_close - ha_open) / c.close)
        };
        self.ha_body_ratio = if ha_range < 1e-12 {
            None
        } else {
            Some((ha_close - ha_open).abs() / ha_range)
        };
        self.ha_close_pos = if ha_range < 1e-12 {
            None
        } else {
            Some((ha_close - ha_low) / ha_range)
        };
        self.prev_open = Some(ha_open);
        self.prev_close = Some(ha_close);
    }
}

// 0=absorption_down
// 1=absorption_up
// 2=atr14_pct
// 3=bb_pctb
// 4=body
// 5=body_abs_pct
// 6=body_ratio
// 7=body_sum12
// 8=body_sum3
// 9=body_sum6
// 10=breakout_energy
// 11=cci12
// 12=cci24
// 13=close_position
// 14=close_z12
// 15=close_z24
// 16=compression_12_72
// 17=dist_sma12
// 18=dist_sma24
// 19=dist_vwap24
// 20=dist_vwap72
// 21=donch_high12
// 22=donch_high24
// 23=donch_high72
// 24=donch_low12
// 25=donch_low144
// 26=donch_low24
// 27=donch_low72
// 28=flip_count12
// 29=flip_count6
// 30=green_count6
// 31=green_streak
// 32=ha_body
// 33=ha_body_ratio
// 34=ha_close_position
// 35=hour
// 36=lower_wick
// 37=lower_wick_body
// 38=macd_hist_pct
// 39=macd_pct
// 40=mfi14
// 41=mfi21
// 42=mfi8
// 43=minute_of_day
// 44=range_atr14
// 45=range_pct_z24
// 46=red_count3
// 47=red_count6
// 48=red_streak
// 49=ret1
// 50=ret12
// 51=ret3
// 52=ret6
// 53=rsi14
// 54=rsi7
// 55=rsi8
// 56=session_asia
// 57=session_london
// 58=session_overlap_london_us
// 59=session_us
// 60=signed_volume_ratio20
// 61=stoch_k12
// 62=stoch_k24
// 63=stoch_k72
// 64=upper_wick
// 65=upper_wick_body
// 66=volume_body_efficiency
// 67=volume_range_efficiency
// 68=volume_ratio20
// 69=volume_z24
// 70=vwap_slope24
// 71=vwap_slope72
// 72=weekday
// 73=williams_r12
struct Feats {
    f: [Option<f64>; FEATURE_COUNT],
}

impl Feats {
    fn get(&self, id: usize) -> Option<f64> {
        self.f[id]
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_feats(
    buf: &VecDeque<Candle>,
    rsi7: &PyRsiState,
    rsi8: &PyRsiState,
    rsi14: &PyRsiState,
    rsi21: &PyRsiState,
    atr14_ewm: &PyAtrEwmState,
    macd: &PyMacdState,
    ha: &HaState,
) -> Feats {
    let cur = match buf.back() {
        Some(c) => c,
        None => {
            return Feats {
                f: [None; FEATURE_COUNT],
            }
        }
    };
    let close = cur.close;
    let range = cur.high - cur.low;
    let body_size = cur.close - cur.open;
    let body_abs = body_size.abs();
    let body = if close == 0.0 {
        None
    } else {
        Some(body_size / close)
    };
    let body_abs_pct = if close == 0.0 {
        None
    } else {
        Some(body_abs / close)
    };
    let body_ratio = if range == 0.0 {
        None
    } else {
        Some(body_abs / range)
    };
    let close_position = if range == 0.0 {
        None
    } else {
        Some((close - cur.low) / range)
    };
    let lower_wick = if close == 0.0 {
        None
    } else {
        Some((cur.open.min(cur.close) - cur.low) / close)
    };
    let upper_wick = if close == 0.0 {
        None
    } else {
        Some((cur.high - cur.open.max(cur.close)) / close)
    };
    let lower_wick_body = if body_abs < 1e-10 {
        None
    } else {
        Some((cur.open.min(cur.close) - cur.low) / body_abs)
    };
    let upper_wick_body = if body_abs < 1e-10 {
        None
    } else {
        Some((cur.high - cur.open.max(cur.close)) / body_abs)
    };
    let hour = cur.close_time.hour() as f64;
    let minute_of_day = hour * 60.0 + cur.close_time.minute() as f64;
    let weekday = cur.close_time.weekday().num_days_from_monday() as f64;
    let mut f: [Option<f64>; FEATURE_COUNT] = [None; FEATURE_COUNT];
    f[0] = absorption(buf, false);
    f[1] = absorption(buf, true);
    f[2] = atr_pct_sma(buf, 14, close);
    f[3] = bb_pctb(buf);
    f[4] = body;
    f[5] = body_abs_pct;
    f[6] = body_ratio;
    f[7] = body_sum(buf, 12);
    f[8] = body_sum(buf, 3);
    f[9] = body_sum(buf, 6);
    f[10] = breakout_energy_f(buf);
    f[11] = cci_n(buf, 12);
    f[12] = cci_n(buf, 24);
    f[13] = close_position;
    f[14] = close_z(buf, 12);
    f[15] = close_z(buf, 24);
    f[16] = compression_ratio(buf, 12, 72);
    f[17] = dist_sma(buf, 12, close);
    f[18] = dist_sma(buf, 24, close);
    f[19] = dist_vwap(buf, 24, close);
    f[20] = dist_vwap(buf, 72, close);
    f[21] = donch_high(buf, 12, close);
    f[22] = donch_high(buf, 24, close);
    f[23] = donch_high(buf, 72, close);
    f[24] = donch_low(buf, 12, close);
    f[25] = donch_low(buf, 144, close);
    f[26] = donch_low(buf, 24, close);
    f[27] = donch_low(buf, 72, close);
    f[28] = flip_count(buf, 12);
    f[29] = flip_count(buf, 6);
    f[30] = count_color(buf, 6, true);
    f[31] = Some(green_streak(buf));
    f[32] = ha.ha_body;
    f[33] = ha.ha_body_ratio;
    f[34] = ha.ha_close_pos;
    f[35] = Some(hour);
    f[36] = lower_wick;
    f[37] = lower_wick_body;
    f[38] = macd.hist_pct(close);
    f[39] = macd.line_pct(close);
    f[40] = mfi_n(buf, 14);
    f[41] = mfi_n(buf, 21);
    f[42] = mfi_n(buf, 8);
    f[43] = Some(minute_of_day);
    f[44] = range_atr14(buf, atr14_ewm.raw());
    f[45] = range_pct_z(buf, 24);
    f[46] = count_color(buf, 3, false);
    f[47] = count_color(buf, 6, false);
    f[48] = Some(red_streak(buf));
    f[49] = ret_n(buf, 1);
    f[50] = ret_n(buf, 12);
    f[51] = ret_n(buf, 3);
    f[52] = ret_n(buf, 6);
    f[53] = rsi14.get();
    f[54] = rsi7.get();
    f[55] = rsi8.get();
    f[56] = Some(session_asia(minute_of_day));
    f[57] = Some(session_london(minute_of_day));
    f[58] = Some(session_overlap_london_us(minute_of_day));
    f[59] = Some(session_us(minute_of_day));
    f[60] = signed_vol_ratio(buf, 20);
    f[61] = stoch_k(buf, 12, close);
    f[62] = stoch_k(buf, 24, close);
    f[63] = stoch_k(buf, 72, close);
    f[64] = upper_wick;
    f[65] = upper_wick_body;
    f[66] = vol_body_eff(buf);
    f[67] = vol_range_eff(buf);
    f[68] = volume_ratio(buf, 20);
    f[69] = vol_z(buf, 24);
    f[70] = vwap_slope(buf, 24);
    f[71] = vwap_slope(buf, 72);
    f[72] = Some(weekday);
    f[73] = williams_r(buf, 12);
    Feats { f }
}

enum Cond {
    Ge(usize, f64),
    Le(usize, f64),
    Eq(usize, f64),
    Between(usize, f64, f64),
    In(usize, &'static [f64]),
}

type Rule = (bool, &'static [Cond]);

fn cmp_ok(val: f64, cond: &Cond) -> bool {
    match cond {
        Cond::Ge(_, a) => val >= *a,
        Cond::Le(_, a) => val <= *a,
        Cond::Eq(_, a) => (val - *a).abs() < 1e-9,
        Cond::Between(_, a, b) => val >= *a && val <= *b,
        Cond::In(_, xs) => xs.iter().any(|x| (val - *x).abs() < 1e-9),
    }
}

fn cond_feature_id(cond: &Cond) -> usize {
    match cond {
        Cond::Ge(id, _)
        | Cond::Le(id, _)
        | Cond::Eq(id, _)
        | Cond::Between(id, _, _)
        | Cond::In(id, _) => *id,
    }
}

fn rule_fires(feats: &Feats, rule: &Rule) -> Option<bool> {
    for cond in rule.1 {
        let v = feats.get(cond_feature_id(cond))?;
        if !cmp_ok(v, cond) {
            return None;
        }
    }
    Some(rule.0)
}

static RULES: &[Rule] = &[
    // 1 ethusdt_1h_rules_1: GREEN
    (
        true,
        &[
            Cond::Le(61, 2.062588143616_f64),
            Cond::Le(70, -0.028555905248_f64),
        ],
    ),
    // 2 ethusdt_1h_rules_2: GREEN
    (
        true,
        &[
            Cond::Ge(37, 99.803555555201_f64),
            Cond::Between(68, 0.697599259542_f64, 0.930270598004_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 3 ethusdt_1h_rules_3: RED
    (
        false,
        &[
            Cond::Ge(9, 0.061916514734_f64),
            Cond::Le(10, -0.070623222976_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 4 ethusdt_1h_rules_4: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.025147324264_f64),
            Cond::Ge(11, -90.37920447042_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 5 ethusdt_1h_rules_5: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.289885513984_f64),
            Cond::Le(16, 0.442037442949_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 6 ethusdt_1h_rules_6: RED
    (
        false,
        &[
            Cond::Ge(22, -0.004419256154_f64),
            Cond::Le(0, -41.794890628583_f64),
        ],
    ),
    // 7 ethusdt_1h_rules_7: RED
    (
        false,
        &[
            Cond::Ge(61, 97.251019949653_f64),
            Cond::Ge(64, 0.001007899334_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 8 ethusdt_1h_rules_8: RED
    (
        false,
        &[
            Cond::Ge(54, 87.21365662096_f64),
            Cond::Le(2, 0.005354469061_f64),
        ],
    ),
    // 9 ethusdt_1h_rules_9: GREEN
    (
        true,
        &[
            Cond::Le(55, 27.937723759111_f64),
            Cond::Ge(67, 0.046027537425_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 10 ethusdt_1h_rules_10: GREEN
    (
        true,
        &[
            Cond::Le(40, 15.30340668771_f64),
            Cond::Le(33, 0.063903626236_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 11 ethusdt_1h_rules_11: RED
    (
        false,
        &[
            Cond::Ge(31, 5.0_f64),
            Cond::Le(69, -1.159346030872_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 12 ethusdt_1h_rules_12: RED
    (
        false,
        &[
            Cond::Ge(62, 83.706222662312_f64),
            Cond::Le(24, 0.004099778821_f64),
        ],
    ),
    // 13 ethusdt_1h_rules_13: GREEN
    (
        true,
        &[
            Cond::Le(62, 2.417455072988_f64),
            Cond::Between(45, -0.614786474045_f64, 0.207539773281_f64),
            Cond::Eq(72, 0.0_f64),
        ],
    ),
    // 14 ethusdt_1h_rules_14: RED
    (
        false,
        &[
            Cond::Ge(62, 92.398919564528_f64),
            Cond::Ge(66, 0.021833334011_f64),
            Cond::Eq(72, 6.0_f64),
        ],
    ),
    // 15 ethusdt_1h_rules_15: GREEN
    (
        true,
        &[
            Cond::Le(61, 2.325581395_f64),
            Cond::Eq(35, 11.0_f64),
            Cond::Ge(73, -99.31370042_f64),
        ],
    ),
    // 16 ethusdt_1h_rules_16: GREEN
    (
        true,
        &[
            Cond::Le(55, 29.828518892658_f64),
            Cond::Le(16, 0.517302629848_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 17 ethusdt_1h_rules_17: GREEN
    (
        true,
        &[
            Cond::Le(13, 0.100215016184_f64),
            Cond::Ge(52, 0.065040574296_f64),
        ],
    ),
    // 18 ethusdt_1h_rules_18: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.002650404386_f64),
            Cond::Le(61, 5.152344313_f64),
            Cond::Le(62, 3.478803314_f64),
            Cond::Eq(35, 20.0_f64),
        ],
    ),
    // 19 ethusdt_1h_rules_19: GREEN
    (
        true,
        &[
            Cond::Le(62, 3.695232436063_f64),
            Cond::Le(70, -0.028555905248_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 20 ethusdt_1h_rules_20: GREEN
    (
        true,
        &[
            Cond::Le(61, 5.418823304347_f64),
            Cond::Le(50, -0.090923229416_f64),
            Cond::Eq(72, 0.0_f64),
        ],
    ),
    // 21 ethusdt_1h_rules_21: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.187038250278_f64),
            Cond::Ge(17, -0.004060754253_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 22 ethusdt_1h_rules_22: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.025147324264_f64),
            Cond::Ge(11, -90.37920447042_f64),
            Cond::Eq(58, 1.0_f64),
        ],
    ),
    // 23 ethusdt_1h_rules_23: GREEN
    (
        true,
        &[
            Cond::Le(54, 28.488182057264_f64),
            Cond::Ge(40, 60.696816665125_f64),
        ],
    ),
    // 24 ethusdt_1h_rules_24: GREEN
    (
        true,
        &[
            Cond::Le(40, 21.44107346_f64),
            Cond::Le(13, 0.1223925466_f64),
            Cond::Ge(37, 0.06465758156_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 25 ethusdt_1h_rules_25: GREEN
    (
        true,
        &[Cond::Le(52, -0.065293293674_f64), Cond::Le(28, 2.0_f64)],
    ),
    // 26 ethusdt_1h_rules_26: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.006837778067_f64),
            Cond::Le(54, 13.5929322_f64),
            Cond::Ge(15, -2.584346456_f64),
            Cond::Eq(35, 19.0_f64),
        ],
    ),
    // 27 ethusdt_1h_rules_27: RED
    (
        false,
        &[
            Cond::Ge(20, 0.098883002663_f64),
            Cond::Le(43, 60.0_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 28 ethusdt_1h_rules_28: GREEN
    (
        true,
        &[
            Cond::Le(54, 22.509371455114_f64),
            Cond::Ge(15, -1.189485911056_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 29 ethusdt_1h_rules_29: GREEN
    (
        true,
        &[
            Cond::Le(49, -0.012975327157_f64),
            Cond::Ge(40, 73.639202986603_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 30 ethusdt_1h_rules_30: RED
    (
        false,
        &[
            Cond::Ge(42, 90.971006267369_f64),
            Cond::Ge(67, 0.028499754838_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 31 ethusdt_1h_rules_31: GREEN
    (
        true,
        &[
            Cond::Le(40, 12.370381786246_f64),
            Cond::Le(10, -0.134109392588_f64),
        ],
    ),
    // 32 ethusdt_1h_rules_32: RED
    (
        false,
        &[
            Cond::Ge(61, 95.36043284_f64),
            Cond::Ge(7, 0.02432417868_f64),
            Cond::Le(36, 0.001638794532_f64),
            Cond::Eq(35, 20.0_f64),
        ],
    ),
    // 33 ethusdt_1h_rules_33: RED
    (
        false,
        &[
            Cond::Ge(62, 96.167908771068_f64),
            Cond::Le(22, -0.001936103113_f64),
            Cond::Eq(35, 19.0_f64),
        ],
    ),
    // 34 ethusdt_1h_rules_34: GREEN
    (
        true,
        &[
            Cond::Le(55, 12.039352357177_f64),
            Cond::Ge(12, -123.693327474426_f64),
        ],
    ),
    // 35 ethusdt_1h_rules_35: GREEN
    (
        true,
        &[Cond::Le(15, -2.404114354323_f64), Cond::Ge(29, 6.0_f64)],
    ),
    // 36 ethusdt_1h_rules_36: RED
    (
        false,
        &[
            Cond::Ge(62, 89.551972381464_f64),
            Cond::Le(13, 0.17007379085_f64),
            Cond::Eq(72, 3.0_f64),
        ],
    ),
    // 37 ethusdt_1h_rules_37: GREEN
    (
        true,
        &[
            Cond::Le(25, 0.005548156292_f64),
            Cond::Le(7, -0.03657074613_f64),
            Cond::Le(40, 21.44107346_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 38 ethusdt_1h_rules_38: RED
    (
        false,
        &[
            Cond::Ge(62, 96.060395753965_f64),
            Cond::Le(44, 0.470794215282_f64),
        ],
    ),
    // 39 ethusdt_1h_rules_39: GREEN
    (
        true,
        &[
            Cond::Le(55, 23.997196098696_f64),
            Cond::Ge(67, 0.032227797354_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 40 ethusdt_1h_rules_40: RED
    (
        false,
        &[
            Cond::Ge(21, -0.00152070846_f64),
            Cond::Le(40, 33.487434256627_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 41 ethusdt_1h_rules_41: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.187038250278_f64),
            Cond::Ge(65, 1.27376961958_f64),
        ],
    ),
    // 42 ethusdt_1h_rules_42: GREEN
    (
        true,
        &[Cond::Le(61, 3.503757061587_f64), Cond::In(35, &[20.0_f64])],
    ),
    // 43 ethusdt_1h_rules_43: GREEN
    (
        true,
        &[
            Cond::Le(24, 0.005311716219_f64),
            Cond::Ge(39, 0.013210383149_f64),
        ],
    ),
    // 44 ethusdt_1h_rules_44: GREEN
    (
        true,
        &[
            Cond::Le(14, -1.703514870312_f64),
            Cond::Ge(18, 0.002274079813_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 45 ethusdt_1h_rules_45: GREEN
    (
        true,
        &[
            Cond::Le(61, 7.081893123604_f64),
            Cond::Le(45, -0.975669526392_f64),
        ],
    ),
    // 46 ethusdt_1h_rules_46: RED
    (
        false,
        &[
            Cond::Ge(19, 0.0494856271_f64),
            Cond::Le(6, 0.073463020111_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 47 ethusdt_1h_rules_47: GREEN
    (
        true,
        &[
            Cond::Le(24, 0.002650174002_f64),
            Cond::Ge(44, 3.036678151198_f64),
        ],
    ),
    // 48 ethusdt_1h_rules_48: RED
    (
        false,
        &[
            Cond::Ge(61, 88.470757284927_f64),
            Cond::Ge(64, 0.005420124579_f64),
            Cond::Eq(35, 0.0_f64),
        ],
    ),
    // 49 ethusdt_1h_rules_49: RED
    (
        false,
        &[
            Cond::Ge(15, 1.806249404164_f64),
            Cond::Le(71, -0.014146164354_f64),
            Cond::Eq(35, 23.0_f64),
        ],
    ),
    // 50 ethusdt_1h_rules_50: RED
    (
        false,
        &[Cond::Le(47, 0.0_f64), Cond::Le(40, 39.31642972534_f64)],
    ),
    // 51 ethusdt_1h_rules_51: RED
    (
        false,
        &[
            Cond::Ge(62, 97.684471745776_f64),
            Cond::Le(13, 0.848302105456_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 52 ethusdt_1h_rules_52: GREEN
    (
        true,
        &[
            Cond::Le(40, 21.44107346_f64),
            Cond::Le(13, 0.1223925466_f64),
            Cond::Ge(37, 0.06465758156_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 53 ethusdt_1h_rules_53: RED
    (
        false,
        &[
            Cond::Ge(19, 0.0494856271_f64),
            Cond::Between(9, -0.002440276996_f64, 0.002598746782_f64),
        ],
    ),
    // 54 ethusdt_1h_rules_54: RED
    (
        false,
        &[
            Cond::Ge(62, 94.958449012797_f64),
            Cond::Ge(64, 0.003815742964_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 55 ethusdt_1h_rules_55: GREEN
    (
        true,
        &[Cond::Ge(48, 5.0_f64), Cond::Ge(71, 0.033759209683_f64)],
    ),
    // 56 ethusdt_1h_rules_56: GREEN
    (
        true,
        &[
            Cond::Le(54, 20.625140058973_f64),
            Cond::Ge(71, 0.009956278533_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 57 ethusdt_1h_rules_57: RED
    (
        false,
        &[
            Cond::Ge(21, -0.001486921254_f64),
            Cond::Ge(20, 0.069654406809_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 58 ethusdt_1h_rules_58: GREEN
    (
        true,
        &[
            Cond::Le(32, -0.0088103787_f64),
            Cond::Ge(17, 0.00316609386_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 59 ethusdt_1h_rules_59: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.336010955575_f64),
            Cond::Le(68, 0.247637271351_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 60 ethusdt_1h_rules_60: RED
    (
        false,
        &[
            Cond::Ge(22, -0.0023840774_f64),
            Cond::In(35, &[0.0_f64]),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 61 ethusdt_1h_rules_61: RED
    (
        false,
        &[
            Cond::Ge(42, 90.21492658429_f64),
            Cond::Le(10, -0.479441712554_f64),
        ],
    ),
    // 62 ethusdt_1h_rules_62: GREEN
    (
        true,
        &[
            Cond::Le(13, 0.000886273409_f64),
            Cond::Le(16, 0.639508691164_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 63 ethusdt_1h_rules_63: GREEN
    (
        true,
        &[
            Cond::Le(13, 0.056247011082_f64),
            Cond::Ge(51, 0.019257684619_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 64 ethusdt_1h_rules_64: GREEN
    (
        true,
        &[
            Cond::Le(26, 0.008074882103_f64),
            Cond::Ge(70, 0.011708309601_f64),
        ],
    ),
    // 65 ethusdt_1h_rules_65: RED
    (
        false,
        &[
            Cond::Ge(38, 0.001865948161_f64),
            Cond::Ge(61, 95.36043284_f64),
            Cond::Le(62, 96.70846245_f64),
            Cond::Eq(35, 8.0_f64),
        ],
    ),
    // 66 ethusdt_1h_rules_66: GREEN
    (
        true,
        &[
            Cond::Le(49, -0.008267982551_f64),
            Cond::Le(68, 0.536537521914_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 67 ethusdt_1h_rules_67: GREEN
    (
        true,
        &[
            Cond::Le(14, -1.342144035663_f64),
            Cond::Ge(7, 0.006843139792_f64),
            Cond::Eq(72, 4.0_f64),
        ],
    ),
    // 68 ethusdt_1h_rules_68: RED
    (
        false,
        &[
            Cond::Ge(38, 0.00186594868_f64),
            Cond::Ge(61, 95.36034773_f64),
            Cond::Ge(55, 86.81303658_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 69 ethusdt_1h_rules_69: RED
    (
        false,
        &[
            Cond::Ge(54, 87.21365662096_f64),
            Cond::Le(45, -0.770081671424_f64),
        ],
    ),
    // 70 ethusdt_1h_rules_70: GREEN
    (
        true,
        &[
            Cond::Le(30, 1.0_f64),
            Cond::Le(69, -1.457487435051_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 71 ethusdt_1h_rules_71: GREEN
    (
        true,
        &[
            Cond::Le(13, 0.000886273409_f64),
            Cond::Le(45, -1.129440954302_f64),
        ],
    ),
    // 72 ethusdt_1h_rules_72: GREEN
    (
        true,
        &[
            Cond::Le(61, 3.503757061587_f64),
            Cond::Le(2, 0.003674435615_f64),
        ],
    ),
    // 73 ethusdt_1h_rules_73: RED
    (
        false,
        &[
            Cond::Ge(31, 6.0_f64),
            Cond::Ge(39, 0.013873233781_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 74 ethusdt_1h_rules_74: GREEN
    (
        true,
        &[
            Cond::Le(15, -2.112495107_f64),
            Cond::Le(21, -0.09779320525_f64),
            Cond::Ge(11, -175.5808578_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 75 ethusdt_1h_rules_75: RED
    (
        false,
        &[
            Cond::Ge(31, 2.0_f64),
            Cond::Ge(54, 60.0_f64),
            Cond::Ge(44, 1.2_f64),
            Cond::Ge(6, 0.6_f64),
            Cond::Eq(72, 6.0_f64),
            Cond::Eq(35, 2.0_f64),
        ],
    ),
    // 76 ethusdt_1h_rules_76: RED
    (
        false,
        &[
            Cond::Ge(14, 2.218106766884_f64),
            Cond::Le(70, -0.003994395698_f64),
            Cond::Eq(35, 16.0_f64),
        ],
    ),
    // 77 ethusdt_1h_rules_77: GREEN
    (
        true,
        &[
            Cond::Le(53, 21.149731261368_f64),
            Cond::Le(1, -0.376852964393_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 78 ethusdt_1h_rules_78: RED
    (
        false,
        &[
            Cond::Ge(61, 96.022441207446_f64),
            Cond::Le(68, 0.34972482193_f64),
        ],
    ),
    // 79 ethusdt_1h_rules_79: RED
    (
        false,
        &[
            Cond::Ge(38, 0.001865948161_f64),
            Cond::Ge(61, 95.36043284_f64),
            Cond::Le(62, 96.70846245_f64),
            Cond::Eq(35, 17.0_f64),
        ],
    ),
    // 80 ethusdt_1h_rules_80: GREEN
    (
        true,
        &[
            Cond::Le(13, 0.008177319388_f64),
            Cond::Ge(17, 0.014620999612_f64),
        ],
    ),
    // 81 ethusdt_1h_rules_81: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.365308166414_f64),
            Cond::Le(16, 0.400893813069_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 82 ethusdt_1h_rules_82: RED
    (
        false,
        &[
            Cond::Ge(62, 89.373499384241_f64),
            Cond::Le(51, -0.002741635191_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 83 ethusdt_1h_rules_83: GREEN
    (
        true,
        &[
            Cond::Le(61, 5.418823304347_f64),
            Cond::Le(10, -0.422866719649_f64),
        ],
    ),
    // 84 ethusdt_1h_rules_84: RED
    (
        false,
        &[
            Cond::Ge(30, 5.0_f64),
            Cond::Le(69, -1.159346030872_f64),
            Cond::Eq(35, 22.0_f64),
        ],
    ),
    // 85 ethusdt_1h_rules_85: RED
    (
        false,
        &[
            Cond::Ge(19, 0.041798655327_f64),
            Cond::Ge(0, 6.734015313961_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 86 ethusdt_1h_rules_86: GREEN
    (
        true,
        &[
            Cond::Le(15, -2.60338060349_f64),
            Cond::Le(33, 0.238168112276_f64),
        ],
    ),
    // 87 ethusdt_1h_rules_87: GREEN
    (
        true,
        &[
            Cond::Le(61, 13.57158226389_f64),
            Cond::Le(2, 0.001924913661_f64),
        ],
    ),
    // 88 ethusdt_1h_rules_88: RED
    (
        false,
        &[
            Cond::Ge(14, 1.714252214364_f64),
            Cond::Le(45, -1.129440954302_f64),
            Cond::Eq(72, 4.0_f64),
        ],
    ),
    // 89 ethusdt_1h_rules_89: GREEN
    (
        true,
        &[
            Cond::Le(17, -0.051248070032_f64),
            Cond::Ge(22, -0.083808938102_f64),
        ],
    ),
    // 90 ethusdt_1h_rules_90: GREEN
    (
        true,
        &[
            Cond::Le(54, 22.509371455114_f64),
            Cond::Ge(39, 0.002976402628_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 91 ethusdt_1h_rules_91: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.344526637064_f64),
            Cond::Ge(20, 0.040068069939_f64),
        ],
    ),
    // 92 ethusdt_1h_rules_92: GREEN
    (
        true,
        &[
            Cond::Le(3, 0.083867390324_f64),
            Cond::Le(69, -1.100475105203_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 93 ethusdt_1h_rules_93: GREEN
    (
        true,
        &[
            Cond::Ge(65, 92.030000000006_f64),
            Cond::Ge(60, 1.504870560108_f64),
        ],
    ),
    // 94 ethusdt_1h_rules_94: RED
    (
        false,
        &[
            Cond::Ge(42, 88.952653717272_f64),
            Cond::Le(2, 0.0031654888_f64),
        ],
    ),
    // 95 ethusdt_1h_rules_95: RED
    (
        false,
        &[
            Cond::Ge(54, 84.181207583347_f64),
            Cond::Le(18, 0.012838900259_f64),
        ],
    ),
    // 96 ethusdt_1h_rules_96: GREEN
    (
        true,
        &[
            Cond::Le(45, -1.536658910705_f64),
            Cond::Le(22, -0.022318725425_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 97 ethusdt_1h_rules_97: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.006837778067_f64),
            Cond::Le(54, 23.29241515_f64),
            Cond::Ge(3, 0.08661248075_f64),
            Cond::Eq(35, 6.0_f64),
        ],
    ),
    // 98 ethusdt_1h_rules_98: GREEN
    (
        true,
        &[
            Cond::Le(62, 2.417455072988_f64),
            Cond::Between(45, -0.614786474045_f64, 0.207539773281_f64),
            Cond::Eq(72, 3.0_f64),
        ],
    ),
    // 99 ethusdt_1h_rules_99: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.336010955575_f64),
            Cond::Ge(52, 0.019270109235_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 100 ethusdt_1h_rules_100: RED
    (
        false,
        &[
            Cond::Ge(61, 95.36043284_f64),
            Cond::Ge(38, 0.002366750995_f64),
            Cond::Ge(15, 2.046146229_f64),
            Cond::Eq(35, 3.0_f64),
        ],
    ),
    // 101 ethusdt_1h_rules_101: RED
    (
        false,
        &[
            Cond::Ge(62, 89.373499384241_f64),
            Cond::Ge(2, 0.027526795523_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 102 ethusdt_1h_rules_102: GREEN
    (
        true,
        &[
            Cond::Le(14, -1.977917225442_f64),
            Cond::Ge(71, 0.039861770193_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 103 ethusdt_1h_rules_103: RED
    (
        false,
        &[
            Cond::Ge(21, -0.000717425385_f64),
            Cond::Le(17, 0.002014123904_f64),
        ],
    ),
    // 104 ethusdt_1h_rules_104: RED
    (
        false,
        &[
            Cond::Ge(62, 98.632249177409_f64),
            Cond::Ge(64, 0.000972276508_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 105 ethusdt_1h_rules_105: GREEN
    (
        true,
        &[
            Cond::Le(21, -0.103167040398_f64),
            Cond::Ge(58, 1.0_f64),
            Cond::Eq(35, 13.0_f64),
        ],
    ),
    // 106 ethusdt_1h_rules_106: GREEN
    (
        true,
        &[
            Cond::Le(25, 0.005548156292_f64),
            Cond::Le(7, -0.03657074613_f64),
            Cond::Le(40, 21.44107346_f64),
            Cond::Eq(35, 18.0_f64),
        ],
    ),
    // 107 ethusdt_1h_rules_107: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.286381306679_f64),
            Cond::Ge(53, 37.712563201862_f64),
        ],
    ),
    // 108 ethusdt_1h_rules_108: RED
    (
        false,
        &[
            Cond::Ge(38, 0.008650326859_f64),
            Cond::Le(2, 0.014794336648_f64),
        ],
    ),
    // 109 ethusdt_1h_rules_109: RED
    (
        false,
        &[
            Cond::Ge(55, 82.454573967079_f64),
            Cond::In(35, &[19.0_f64]),
            Cond::Eq(72, 0.0_f64),
        ],
    ),
    // 110 ethusdt_1h_rules_110: RED
    (
        false,
        &[
            Cond::Ge(55, 82.454573967079_f64),
            Cond::In(35, &[19.0_f64]),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 111 ethusdt_1h_rules_111: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.006837778067_f64),
            Cond::Le(40, 16.05947179_f64),
            Cond::Ge(11, -112.7701187_f64),
            Cond::Eq(72, 6.0_f64),
        ],
    ),
    // 112 ethusdt_1h_rules_112: RED
    (
        false,
        &[
            Cond::Ge(54, 78.115236912005_f64),
            Cond::Le(24, 0.011447152483_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 113 ethusdt_1h_rules_113: RED
    (
        false,
        &[
            Cond::Ge(30, 5.0_f64),
            Cond::Le(45, -1.422984964535_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 114 ethusdt_1h_rules_114: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.599250472473_f64),
            Cond::Ge(32, 0.002306019819_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 115 ethusdt_1h_rules_115: RED
    (
        false,
        &[
            Cond::Ge(13, 0.981752292899_f64),
            Cond::Ge(70, 0.014461038157_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 116 ethusdt_1h_rules_116: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.273882464262_f64),
            Cond::Ge(26, 0.030155709967_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 117 ethusdt_1h_rules_117: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.006837778067_f64),
            Cond::Le(54, 17.03206178_f64),
            Cond::Ge(32, -0.01017634652_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 118 ethusdt_1h_rules_118: RED
    (
        false,
        &[
            Cond::Ge(54, 67.899626426361_f64),
            Cond::Le(40, 33.487434256627_f64),
        ],
    ),
    // 119 ethusdt_1h_rules_119: RED
    (
        false,
        &[
            Cond::Ge(42, 92.869319882182_f64),
            Cond::Le(17, 0.004404610817_f64),
        ],
    ),
    // 120 ethusdt_1h_rules_120: GREEN
    (
        true,
        &[
            Cond::Le(51, -0.005348262374_f64),
            Cond::Ge(53, 69.060618393279_f64),
            Cond::Eq(35, 20.0_f64),
        ],
    ),
    // 121 ethusdt_1h_rules_121: GREEN
    (
        true,
        &[
            Cond::Le(49, -0.014325177909_f64),
            Cond::Ge(64, 0.016082751957_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 122 ethusdt_1h_rules_122: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.344526637064_f64),
            Cond::Between(50, -0.003610257481_f64, 0.004009867371_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 123 ethusdt_1h_rules_123: RED
    (
        false,
        &[
            Cond::Le(46, 0.0_f64),
            Cond::Le(66, 0.000034905632_f64),
            Cond::Eq(72, 0.0_f64),
        ],
    ),
    // 124 ethusdt_1h_rules_124: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.006837778067_f64),
            Cond::Le(40, 16.05947179_f64),
            Cond::Ge(11, -112.7701187_f64),
            Cond::Eq(35, 16.0_f64),
        ],
    ),
    // 125 ethusdt_1h_rules_125: GREEN
    (
        true,
        &[
            Cond::Le(41, 20.65922058_f64),
            Cond::Le(63, 8.138635_f64),
            Cond::Le(23, -0.1118632156_f64),
            Cond::Eq(58, 1.0_f64),
        ],
    ),
    // 126 ethusdt_1h_rules_126: GREEN
    (
        true,
        &[
            Cond::Le(49, -0.011045001747_f64),
            Cond::Le(68, 0.311970906529_f64),
        ],
    ),
    // 127 ethusdt_1h_rules_127: RED
    (
        false,
        &[
            Cond::Ge(61, 95.36043284_f64),
            Cond::Ge(9, 0.01753783257_f64),
            Cond::Le(36, 0.001638794532_f64),
            Cond::Eq(35, 8.0_f64),
        ],
    ),
    // 128 ethusdt_1h_rules_128: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.365308166414_f64),
            Cond::Ge(5, 0.027907557487_f64),
        ],
    ),
    // 129 ethusdt_1h_rules_129: RED
    (
        false,
        &[Cond::Le(46, 0.0_f64), Cond::Le(52, -0.043364569302_f64)],
    ),
    // 130 ethusdt_1h_rules_130: GREEN
    (
        true,
        &[
            Cond::Le(61, 15.680586695736_f64),
            Cond::Ge(42, 70.426155549544_f64),
        ],
    ),
    // 131 ethusdt_1h_rules_131: RED
    (
        false,
        &[
            Cond::Ge(61, 95.36043284_f64),
            Cond::Ge(7, 0.02432417868_f64),
            Cond::Le(36, 0.001638794532_f64),
            Cond::Eq(35, 0.0_f64),
        ],
    ),
    // 132 ethusdt_1h_rules_132: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.379989986507_f64),
            Cond::Le(2, 0.001924913661_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 133 ethusdt_1h_rules_133: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.344526637064_f64),
            Cond::Le(44, 0.952041479441_f64),
        ],
    ),
    // 134 ethusdt_1h_rules_134: GREEN
    (
        true,
        &[
            Cond::Le(24, 0.00194583421_f64),
            Cond::Le(10, -0.286078754321_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 135 ethusdt_1h_rules_135: GREEN
    (
        true,
        &[
            Cond::Le(24, 0.002650174002_f64),
            Cond::Ge(16, 1.593071747057_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 136 ethusdt_1h_rules_136: GREEN
    (
        true,
        &[
            Cond::Le(21, -0.103167040398_f64),
            Cond::Between(71, -0.005880312126_f64, 0.006771478285_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 137 ethusdt_1h_rules_137: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.005731108736_f64),
            Cond::Ge(27, 0.02923394723_f64),
            Cond::Ge(25, 0.04226528076_f64),
            Cond::Eq(35, 10.0_f64),
            Cond::Eq(72, 3.0_f64),
        ],
    ),
    // 138 ethusdt_1h_rules_138: RED
    (
        false,
        &[
            Cond::Ge(15, 2.041451492706_f64),
            Cond::Le(39, -0.001221253105_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 139 ethusdt_1h_rules_139: GREEN
    (
        true,
        &[
            Cond::Le(53, 18.578805575288_f64),
            Cond::Ge(0, 13.915733334115_f64),
        ],
    ),
    // 140 ethusdt_1h_rules_140: GREEN
    (
        true,
        &[Cond::Le(52, -0.065293293674_f64), Cond::In(35, &[20.0_f64])],
    ),
    // 141 ethusdt_1h_rules_141: RED
    (
        false,
        &[
            Cond::Ge(21, -0.00152070846_f64),
            Cond::Le(40, 33.487434256627_f64),
            Cond::Eq(72, 5.0_f64),
        ],
    ),
    // 142 ethusdt_1h_rules_142: GREEN
    (
        true,
        &[Cond::Le(26, 0.001171005877_f64), Cond::In(35, &[16.0_f64])],
    ),
    // 143 ethusdt_1h_rules_143: GREEN
    (
        true,
        &[
            Cond::Le(62, 10.434588873737_f64),
            Cond::Ge(70, 0.004008177104_f64),
            Cond::Eq(35, 16.0_f64),
        ],
    ),
    // 144 ethusdt_1h_rules_144: GREEN
    (
        true,
        &[
            Cond::Le(61, 5.418823304347_f64),
            Cond::Le(16, 0.517302629848_f64),
        ],
    ),
    // 145 ethusdt_1h_rules_145: GREEN
    (
        true,
        &[Cond::Ge(48, 6.0_f64), Cond::Le(10, -0.134109392588_f64)],
    ),
    // 146 ethusdt_1h_rules_146: GREEN
    (
        true,
        &[Cond::Le(53, 18.578805575288_f64), Cond::In(35, &[9.0_f64])],
    ),
    // 147 ethusdt_1h_rules_147: RED
    (
        false,
        &[
            Cond::Ge(30, 5.0_f64),
            Cond::Le(69, -1.159346030872_f64),
            Cond::Eq(35, 23.0_f64),
        ],
    ),
    // 148 ethusdt_1h_rules_148: GREEN
    (
        true,
        &[
            Cond::Le(8, -0.006812659013_f64),
            Cond::Ge(26, 0.117516911206_f64),
            Cond::Eq(35, 13.0_f64),
        ],
    ),
    // 149 ethusdt_1h_rules_149: GREEN
    (
        true,
        &[
            Cond::Le(54, 22.509371455114_f64),
            Cond::Le(10, -0.422866719649_f64),
        ],
    ),
    // 150 ethusdt_1h_rules_150: GREEN
    (
        true,
        &[
            Cond::Le(53, 21.149731261368_f64),
            Cond::Le(44, 0.494425129905_f64),
        ],
    ),
    // 151 ethusdt_1h_rules_151: GREEN
    (
        true,
        &[Cond::Le(14, -2.289885513984_f64), Cond::Ge(29, 6.0_f64)],
    ),
    // 152 ethusdt_1h_rules_152: RED
    (
        false,
        &[
            Cond::Ge(21, -0.001155514485_f64),
            Cond::Le(24, 0.004099778821_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 153 ethusdt_1h_rules_153: RED
    (
        false,
        &[
            Cond::Ge(54, 87.21365662096_f64),
            Cond::Between(70, -0.003184483329_f64, 0.003367010176_f64),
        ],
    ),
    // 154 ethusdt_1h_rules_154: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.025147324264_f64),
            Cond::Ge(11, -90.37920447042_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 155 ethusdt_1h_rules_155: GREEN
    (
        true,
        &[
            Cond::Le(15, -2.921859236625_f64),
            Cond::Ge(66, 0.007814538794_f64),
            Cond::Eq(58, 1.0_f64),
        ],
    ),
    // 156 ethusdt_1h_rules_156: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.344526637064_f64),
            Cond::Le(2, 0.002837437065_f64),
        ],
    ),
    // 157 ethusdt_1h_rules_157: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.091259020395_f64),
            Cond::Ge(40, 60.696816665125_f64),
        ],
    ),
    // 158 ethusdt_1h_rules_158: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.002929379759_f64),
            Cond::Ge(64, 0.005230358453_f64),
            Cond::Ge(44, 1.386145597_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 159 ethusdt_1h_rules_159: RED
    (
        false,
        &[
            Cond::Ge(54, 78.115236912005_f64),
            Cond::Le(2, 0.002837437065_f64),
        ],
    ),
    // 160 ethusdt_1h_rules_160: GREEN
    (
        true,
        &[
            Cond::Le(13, 0.100215016184_f64),
            Cond::Ge(52, 0.053650017546_f64),
        ],
    ),
    // 161 ethusdt_1h_rules_161: GREEN
    (
        true,
        &[
            Cond::Le(55, 29.828518892658_f64),
            Cond::Le(16, 0.487954125812_f64),
        ],
    ),
    // 162 ethusdt_1h_rules_162: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.405699915549_f64),
            Cond::Ge(5, 0.027039498754_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 163 ethusdt_1h_rules_163: GREEN
    (
        true,
        &[Cond::Le(15, -2.60338060349_f64), Cond::Ge(28, 10.0_f64)],
    ),
    // 164 ethusdt_1h_rules_164: GREEN
    (
        true,
        &[
            Cond::Le(61, 5.418823304347_f64),
            Cond::Le(67, 0.002890012136_f64),
        ],
    ),
    // 165 ethusdt_1h_rules_165: GREEN
    (
        true,
        &[
            Cond::Le(62, 7.980198437_f64),
            Cond::Le(39, -0.01229544773_f64),
            Cond::Le(40, 18.18947095_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 166 ethusdt_1h_rules_166: RED
    (
        false,
        &[
            Cond::Ge(62, 97.799245309998_f64),
            Cond::Ge(19, 0.050318074037_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 167 ethusdt_1h_rules_167: GREEN
    (
        true,
        &[
            Cond::Le(15, -2.596399053377_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::Eq(35, 20.0_f64),
        ],
    ),
    // 168 ethusdt_1h_rules_168: RED
    (
        false,
        &[
            Cond::Ge(62, 97.799245309998_f64),
            Cond::Ge(20, 0.079980230055_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 169 ethusdt_1h_rules_169: GREEN
    (
        true,
        &[
            Cond::Le(62, 3.695232436063_f64),
            Cond::Le(70, -0.028555905248_f64),
        ],
    ),
    // 170 ethusdt_1h_rules_170: GREEN
    (
        true,
        &[
            Cond::Le(24, 0.00194583421_f64),
            Cond::Ge(71, 0.025184469595_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 171 ethusdt_1h_rules_171: RED
    (
        false,
        &[
            Cond::Ge(42, 88.952653717272_f64),
            Cond::Le(10, -0.60399701237_f64),
        ],
    ),
    // 172 ethusdt_1h_rules_172: GREEN
    (
        true,
        &[
            Cond::Le(25, 0.005548156292_f64),
            Cond::Le(54, 17.03206178_f64),
            Cond::Le(33, 0.6571549533_f64),
            Cond::Eq(72, 2.0_f64),
        ],
    ),
    // 173 ethusdt_1h_rules_173: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.344526637064_f64),
            Cond::Ge(18, 0.003178082774_f64),
        ],
    ),
    // 174 ethusdt_1h_rules_174: RED
    (
        false,
        &[
            Cond::Ge(61, 94.73247534402_f64),
            Cond::Le(68, 0.34972482193_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 175 ethusdt_1h_rules_175: GREEN
    (
        true,
        &[
            Cond::Le(62, 7.980198437_f64),
            Cond::Le(42, 18.38030246_f64),
            Cond::Le(55, 12.50217521_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 176 ethusdt_1h_rules_176: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.187038250278_f64),
            Cond::Le(5, 0.002236733024_f64),
        ],
    ),
    // 177 ethusdt_1h_rules_177: GREEN
    (
        true,
        &[
            Cond::Le(40, 21.44107346_f64),
            Cond::Le(13, 0.1223925466_f64),
            Cond::Ge(37, 0.06465758156_f64),
            Cond::Eq(35, 18.0_f64),
        ],
    ),
    // 178 ethusdt_1h_rules_178: GREEN
    (
        true,
        &[
            Cond::Le(62, 2.417455072988_f64),
            Cond::Between(45, -0.614786474045_f64, 0.207539773281_f64),
            Cond::Eq(57, 1.0_f64),
        ],
    ),
    // 179 ethusdt_1h_rules_179: GREEN
    (
        true,
        &[
            Cond::Le(61, 5.418823304347_f64),
            Cond::Le(50, -0.090923229416_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 180 ethusdt_1h_rules_180: GREEN
    (
        true,
        &[
            Cond::Le(55, 27.937723759111_f64),
            Cond::Le(16, 0.517302629848_f64),
        ],
    ),
    // 181 ethusdt_1h_rules_181: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.187038250278_f64),
            Cond::Ge(17, -0.004060754253_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 182 ethusdt_1h_rules_182: GREEN
    (
        true,
        &[
            Cond::Le(61, 5.418823304347_f64),
            Cond::Le(50, -0.090923229416_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 183 ethusdt_1h_rules_183: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.405699915549_f64),
            Cond::Ge(5, 0.027039498754_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 184 ethusdt_1h_rules_184: GREEN
    (
        true,
        &[Cond::Le(26, 0.00254012653_f64), Cond::In(35, &[20.0_f64])],
    ),
    // 185 ethusdt_1h_rules_185: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.005558113318_f64),
            Cond::Le(38, -0.002259446914_f64),
            Cond::Ge(3, 0.04033662233_f64),
            Cond::Eq(72, 1.0_f64),
        ],
    ),
    // 186 ethusdt_1h_rules_186: GREEN
    (
        true,
        &[
            Cond::Le(15, -3.241184302191_f64),
            Cond::Ge(4, -0.008393357935_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 187 ethusdt_1h_rules_187: RED
    (
        false,
        &[
            Cond::Ge(31, 6.0_f64),
            Cond::Ge(2, 0.007385036543_f64),
            Cond::Eq(35, 6.0_f64),
        ],
    ),
    // 188 ethusdt_1h_rules_188: RED
    (
        false,
        &[Cond::Ge(9, 0.061916514734_f64), Cond::In(35, &[15.0_f64])],
    ),
    // 189 ethusdt_1h_rules_189: GREEN
    (
        true,
        &[
            Cond::Le(62, 2.898892702_f64),
            Cond::Le(21, -0.02994954907_f64),
            Cond::Le(42, 14.51065799_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 190 ethusdt_1h_rules_190: RED
    (
        false,
        &[
            Cond::Ge(20, 0.098883002663_f64),
            Cond::Ge(1, 0.834297868579_f64),
            Cond::Eq(56, 1.0_f64),
        ],
    ),
    // 191 ethusdt_1h_rules_191: RED
    (
        false,
        &[Cond::Ge(40, 90.605260301048_f64), Cond::In(35, &[1.0_f64])],
    ),
    // 192 ethusdt_1h_rules_192: GREEN
    (
        true,
        &[
            Cond::Le(11, -163.848507305154_f64),
            Cond::Ge(66, 0.007704921362_f64),
            Cond::Eq(35, 20.0_f64),
        ],
    ),
    // 193 ethusdt_1h_rules_193: RED
    (
        false,
        &[
            Cond::Ge(61, 92.430123495695_f64),
            Cond::Le(68, 0.283114920997_f64),
        ],
    ),
    // 194 ethusdt_1h_rules_194: GREEN
    (
        true,
        &[
            Cond::Le(62, 7.980198437_f64),
            Cond::Le(42, 18.38030246_f64),
            Cond::Le(55, 12.50217521_f64),
            Cond::Eq(72, 3.0_f64),
        ],
    ),
    // 195 ethusdt_1h_rules_195: GREEN
    (
        true,
        &[
            Cond::Le(34, 0.407243075194_f64),
            Cond::Ge(44, 3.519022231122_f64),
        ],
    ),
    // 196 ethusdt_1h_rules_196: RED
    (
        false,
        &[
            Cond::Ge(62, 89.373499384241_f64),
            Cond::Ge(2, 0.027526795523_f64),
            Cond::Eq(72, 3.0_f64),
        ],
    ),
    // 197 ethusdt_1h_rules_197: GREEN
    (
        true,
        &[
            Cond::Le(40, 21.44107346_f64),
            Cond::Le(13, 0.1223925466_f64),
            Cond::Ge(37, 0.06465758156_f64),
            Cond::Eq(35, 16.0_f64),
        ],
    ),
    // 198 ethusdt_1h_rules_198: GREEN
    (
        true,
        &[
            Cond::Le(25, 0.005548156292_f64),
            Cond::Le(54, 17.03206178_f64),
            Cond::Le(33, 0.6571549533_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 199 ethusdt_1h_rules_199: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.006837778067_f64),
            Cond::Le(40, 16.05947179_f64),
            Cond::Ge(11, -112.7701187_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 200 ethusdt_1h_rules_200: GREEN
    (
        true,
        &[
            Cond::Le(62, 2.417455072988_f64),
            Cond::Between(5, 0.002236733024_f64, 0.003968588912_f64),
        ],
    ),
    // 201 ethusdt_1h_rules_201: GREEN
    (
        true,
        &[
            Cond::Ge(37, 99.803555555201_f64),
            Cond::Between(68, 0.697599259542_f64, 0.930270598004_f64),
            Cond::In(
                35,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 202 ethusdt_1h_rules_202: GREEN
    (
        true,
        &[
            Cond::Le(3, -0.025147324264_f64),
            Cond::Ge(11, -90.37920447042_f64),
        ],
    ),
    // 203 ethusdt_1h_rules_203: RED
    (
        false,
        &[
            Cond::Ge(62, 89.373499384241_f64),
            Cond::Ge(2, 0.023002995475_f64),
            Cond::Eq(72, 6.0_f64),
        ],
    ),
    // 204 ethusdt_1h_rules_204: GREEN
    (
        true,
        &[Cond::Le(14, -2.742641598641_f64), Cond::In(35, &[20.0_f64])],
    ),
    // 205 ethusdt_1h_rules_205: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.148849848163_f64),
            Cond::Ge(71, 0.022316510302_f64),
            Cond::In(35, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 206 ethusdt_1h_rules_206: GREEN
    (
        true,
        &[
            Cond::Le(61, 3.503757061587_f64),
            Cond::Le(38, -0.008589755612_f64),
        ],
    ),
    // 207 ethusdt_1h_rules_207: GREEN
    (
        true,
        &[
            Cond::Le(62, 2.417455072988_f64),
            Cond::Ge(15, -1.412432258616_f64),
        ],
    ),
    // 208 ethusdt_1h_rules_208: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.240426681222_f64),
            Cond::Ge(66, 0.010175590584_f64),
            Cond::Eq(35, 20.0_f64),
        ],
    ),
    // 209 ethusdt_1h_rules_209: RED
    (
        false,
        &[
            Cond::Ge(62, 89.373499384241_f64),
            Cond::Ge(2, 0.027526795523_f64),
            Cond::Eq(59, 1.0_f64),
        ],
    ),
    // 210 ethusdt_1h_rules_210: GREEN
    (
        true,
        &[
            Cond::Le(14, -2.289885513984_f64),
            Cond::Le(16, 0.442037442949_f64),
        ],
    ),
];

pub struct EthH1Rules210 {
    buffer: VecDeque<Candle>,
    min_votes: u32,
    rsi7: PyRsiState,
    rsi8: PyRsiState,
    rsi14: PyRsiState,
    rsi21: PyRsiState,
    atr14_ewm: PyAtrEwmState,
    macd: PyMacdState,
    ha: HaState,
    last_votes: (u32, u32),
}

impl EthH1Rules210 {
    pub fn new(min_votes: u32) -> Self {
        Self {
            buffer: VecDeque::with_capacity(MAX_WINDOW + 1),
            min_votes,
            rsi7: PyRsiState::new(7),
            rsi8: PyRsiState::new(8),
            rsi14: PyRsiState::new(14),
            rsi21: PyRsiState::new(21),
            atr14_ewm: PyAtrEwmState::new(14),
            macd: PyMacdState::new(),
            ha: HaState::new(),
            last_votes: (0, 0),
        }
    }

    fn feed(&mut self, candle: &Candle) {
        self.rsi7.update(candle.close);
        self.rsi8.update(candle.close);
        self.rsi14.update(candle.close);
        self.rsi21.update(candle.close);
        self.atr14_ewm.update(candle);
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
            &self.rsi14,
            &self.rsi21,
            &self.atr14_ewm,
            &self.macd,
            &self.ha,
        );
        let (mut gv, mut rv) = (0u32, 0u32);
        for rule in RULES {
            match rule_fires(&feats, rule) {
                Some(true) => gv += 1,
                Some(false) => rv += 1,
                None => {}
            }
        }
        self.last_votes = (gv, rv);
        (gv, rv)
    }
}

impl Strategy for EthH1Rules210 {
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
        self.atr14_ewm.raw()
    }

    fn candle_log_extras(&self) -> String {
        let (gv, rv) = self.last_votes;
        let total = gv + rv;
        if total == 0 {
            return format!("green=0 | red=0 | total=0 | min_votes={}", self.min_votes);
        }
        let dom = gv.max(rv);
        format!(
            "green={} | red={} | total={} | pct={:.1}% | min_votes={}",
            gv,
            rv,
            total,
            dom as f64 / total as f64 * 100.0,
            self.min_votes
        )
    }
}
