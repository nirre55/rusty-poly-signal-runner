#![allow(dead_code)]

use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "btc_5m_rules_626_min_votes_1";
const FEATURE_COUNT: usize = 82;

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
// 3=atr72_pct
// 4=bb_pctb
// 5=body
// 6=body_abs_pct
// 7=body_ratio
// 8=body_sum12
// 9=body_sum3
// 10=body_sum6
// 11=breakout_energy
// 12=cci12
// 13=cci24
// 14=cci72
// 15=close_position
// 16=close_z12
// 17=close_z24
// 18=close_z48
// 19=compression_12_72
// 20=dist_sma12
// 21=dist_sma24
// 22=dist_vwap24
// 23=dist_vwap72
// 24=donch_high12
// 25=donch_high24
// 26=donch_high72
// 27=donch_low12
// 28=donch_low144
// 29=donch_low24
// 30=donch_low72
// 31=failed_high12
// 32=failed_low12
// 33=flip_count12
// 34=flip_count6
// 35=green_count3
// 36=green_count6
// 37=green_streak
// 38=ha_body
// 39=ha_body_ratio
// 40=ha_close_position
// 41=hour
// 42=lower_wick
// 43=lower_wick_body
// 44=macd_hist_pct
// 45=macd_pct
// 46=mfi14
// 47=mfi21
// 48=mfi8
// 49=minute_of_day
// 50=range_atr14
// 51=range_pct_z24
// 52=red_count3
// 53=red_count6
// 54=red_streak
// 55=ret1
// 56=ret12
// 57=ret24
// 58=ret3
// 59=ret72
// 60=rsi14
// 61=rsi21
// 62=rsi7
// 63=rsi8
// 64=session_asia
// 65=session_london
// 66=session_overlap_london_us
// 67=session_us
// 68=signed_volume_ratio20
// 69=stoch_k12
// 70=stoch_k24
// 71=stoch_k72
// 72=upper_wick
// 73=upper_wick_body
// 74=volume_body_efficiency
// 75=volume_range_efficiency
// 76=volume_ratio20
// 77=volume_z24
// 78=volume_z96
// 79=vwap_slope24
// 80=vwap_slope72
// 81=weekday
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
    f[3] = atr_pct_sma(buf, 72, close);
    f[4] = bb_pctb(buf);
    f[5] = body;
    f[6] = body_abs_pct;
    f[7] = body_ratio;
    f[8] = body_sum(buf, 12);
    f[9] = body_sum(buf, 3);
    f[10] = body_sum(buf, 6);
    f[11] = breakout_energy_f(buf);
    f[12] = cci_n(buf, 12);
    f[13] = cci_n(buf, 24);
    f[14] = cci_n(buf, 72);
    f[15] = close_position;
    f[16] = close_z(buf, 12);
    f[17] = close_z(buf, 24);
    f[18] = close_z(buf, 48);
    f[19] = compression_ratio(buf, 12, 72);
    f[20] = dist_sma(buf, 12, close);
    f[21] = dist_sma(buf, 24, close);
    f[22] = dist_vwap(buf, 24, close);
    f[23] = dist_vwap(buf, 72, close);
    f[24] = donch_high(buf, 12, close);
    f[25] = donch_high(buf, 24, close);
    f[26] = donch_high(buf, 72, close);
    f[27] = donch_low(buf, 12, close);
    f[28] = donch_low(buf, 144, close);
    f[29] = donch_low(buf, 24, close);
    f[30] = donch_low(buf, 72, close);
    f[31] = failed_high(buf, 12);
    f[32] = failed_low(buf, 12);
    f[33] = flip_count(buf, 12);
    f[34] = flip_count(buf, 6);
    f[35] = count_color(buf, 3, true);
    f[36] = count_color(buf, 6, true);
    f[37] = Some(green_streak(buf));
    f[38] = ha.ha_body;
    f[39] = ha.ha_body_ratio;
    f[40] = ha.ha_close_pos;
    f[41] = Some(hour);
    f[42] = lower_wick;
    f[43] = lower_wick_body;
    f[44] = macd.hist_pct(close);
    f[45] = macd.line_pct(close);
    f[46] = mfi_n(buf, 14);
    f[47] = mfi_n(buf, 21);
    f[48] = mfi_n(buf, 8);
    f[49] = Some(minute_of_day);
    f[50] = range_atr14(buf, atr14_ewm.raw());
    f[51] = range_pct_z(buf, 24);
    f[52] = count_color(buf, 3, false);
    f[53] = count_color(buf, 6, false);
    f[54] = Some(red_streak(buf));
    f[55] = ret_n(buf, 1);
    f[56] = ret_n(buf, 12);
    f[57] = ret_n(buf, 24);
    f[58] = ret_n(buf, 3);
    f[59] = ret_n(buf, 72);
    f[60] = rsi14.get();
    f[61] = rsi21.get();
    f[62] = rsi7.get();
    f[63] = rsi8.get();
    f[64] = Some(session_asia(minute_of_day));
    f[65] = Some(session_london(minute_of_day));
    f[66] = Some(session_overlap_london_us(minute_of_day));
    f[67] = Some(session_us(minute_of_day));
    f[68] = signed_vol_ratio(buf, 20);
    f[69] = stoch_k(buf, 12, close);
    f[70] = stoch_k(buf, 24, close);
    f[71] = stoch_k(buf, 72, close);
    f[72] = upper_wick;
    f[73] = upper_wick_body;
    f[74] = vol_body_eff(buf);
    f[75] = vol_range_eff(buf);
    f[76] = volume_ratio(buf, 20);
    f[77] = vol_z(buf, 24);
    f[78] = vol_z(buf, 96);
    f[79] = vwap_slope(buf, 24);
    f[80] = vwap_slope(buf, 72);
    f[81] = Some(weekday);
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
    // 1 btcusdt_5m_rules_1: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(6, 0.007864217465_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 2 btcusdt_5m_rules_2: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(30, 0.001652921292_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 3 btcusdt_5m_rules_3: GREEN
    (
        true,
        &[
            Cond::Le(70, 0.787062464583_f64),
            Cond::Ge(45, 0.000533890036_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 4 btcusdt_5m_rules_4: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(46, 27.45320025_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 5 btcusdt_5m_rules_5: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(48, 43.719050398098_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 6 btcusdt_5m_rules_6: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(10, -0.002956606273_f64),
            Cond::Le(61, 35.7929937_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 7 btcusdt_5m_rules_7: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Ge(24, -0.002580027013_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 8 btcusdt_5m_rules_8: RED
    (
        false,
        &[
            Cond::Ge(69, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 9 btcusdt_5m_rules_9: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(10, -0.01817602056_f64),
            Cond::Ge(78, 3.587853962_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 10 btcusdt_5m_rules_10: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(11, -0.134109392588_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 11 btcusdt_5m_rules_11: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.898892702_f64),
            Cond::Le(24, -0.02994954907_f64),
            Cond::Le(48, 14.51065799_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 12 btcusdt_5m_rules_12: RED
    (
        false,
        &[
            Cond::Ge(48, 94.13387702_f64),
            Cond::Ge(24, -0.0001214530509_f64),
            Cond::Le(70, 99.45945562_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 13 btcusdt_5m_rules_13: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.005731108736_f64),
            Cond::Ge(30, 0.02923394723_f64),
            Cond::Ge(28, 0.04226528076_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 14 btcusdt_5m_rules_14: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(15, 0.029402232243_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 15 btcusdt_5m_rules_15: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000993286689_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 16 btcusdt_5m_rules_16: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(26, -0.001004677022_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 17 btcusdt_5m_rules_17: GREEN
    (
        true,
        &[
            Cond::Ge(43, 99.803555555201_f64),
            Cond::Ge(15, 0.944724032971_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 18 btcusdt_5m_rules_18: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 19 btcusdt_5m_rules_19: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Le(1, -5.344432465957_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 20 btcusdt_5m_rules_20: RED
    (
        false,
        &[
            Cond::Ge(4, 1.060875802045_f64),
            Cond::Le(39, 0.140781768746_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 21 btcusdt_5m_rules_21: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.174372887633_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 22 btcusdt_5m_rules_22: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(79, -0.003994395698_f64),
        ],
    ),
    // 23 btcusdt_5m_rules_23: RED
    (
        false,
        &[
            Cond::Ge(48, 92.776570198665_f64),
            Cond::Ge(33, 10.0_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 24 btcusdt_5m_rules_24: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.005731108736_f64),
            Cond::Ge(30, 0.02923394723_f64),
            Cond::Ge(28, 0.04226528076_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 25 btcusdt_5m_rules_25: GREEN
    (
        true,
        &[
            Cond::Le(70, 9.711755951452_f64),
            Cond::Ge(1, 13.438071751687_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 26 btcusdt_5m_rules_26: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.970296088074_f64),
            Cond::Ge(1, 13.438071751687_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 27 btcusdt_5m_rules_27: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.487372513_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Ge(10, -0.004965357046_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 28 btcusdt_5m_rules_28: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.487372513_f64),
            Cond::Eq(41, 13.0_f64),
            Cond::Le(78, 1.219114982_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 29 btcusdt_5m_rules_29: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 97.6614989_f64),
            Cond::Ge(17, 2.292740233_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 30 btcusdt_5m_rules_30: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(75, 0.011732866886_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 31 btcusdt_5m_rules_31: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266398454_f64),
            Cond::Le(57, -0.03454574655_f64),
            Cond::Ge(72, 0.001054938882_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 32 btcusdt_5m_rules_32: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.091431392_f64),
            Cond::Le(10, -0.01392502169_f64),
            Cond::Le(36, 1.0_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 33 btcusdt_5m_rules_33: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000193374386_f64),
            Cond::Ge(4, 1.300789052776_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 34 btcusdt_5m_rules_34: RED
    (
        false,
        &[
            Cond::Ge(70, 97.799245309998_f64),
            Cond::Le(12, 47.005414521394_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 35 btcusdt_5m_rules_35: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(60, 35.01130025_f64),
            Cond::Le(21, -0.003142104413_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 36 btcusdt_5m_rules_36: GREEN
    (
        true,
        &[
            Cond::Le(46, 10.339569656362_f64),
            Cond::In(81, &[2.0_f64]),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 37 btcusdt_5m_rules_37: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(10, -0.005522189783_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 38 btcusdt_5m_rules_38: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Ge(58, -0.004437668774_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 39 btcusdt_5m_rules_39: GREEN
    (
        true,
        &[
            Cond::Le(12, -209.9116877_f64),
            Cond::Le(3, 0.0007461972567_f64),
            Cond::Ge(2, 0.0006619825044_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 40 btcusdt_5m_rules_40: GREEN
    (
        true,
        &[
            Cond::Le(5, 0.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(43, 2.0_f64),
            Cond::Ge(76, 1.0_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 41 btcusdt_5m_rules_41: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 42 btcusdt_5m_rules_42: RED
    (
        false,
        &[
            Cond::Ge(17, 3.06306070625_f64),
            Cond::Between(23, -0.000806012153_f64, 0.001120083964_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 43 btcusdt_5m_rules_43: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.058231615_f64),
            Cond::Ge(30, 0.02558085724_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 44 btcusdt_5m_rules_44: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Le(21, -0.0156322462_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 45 btcusdt_5m_rules_45: RED
    (
        false,
        &[
            Cond::Ge(63, 79.78754453_f64),
            Cond::Eq(41, 21.0_f64),
            Cond::Ge(53, 2.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 46 btcusdt_5m_rules_46: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(44, -0.000401474502_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 47 btcusdt_5m_rules_47: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000518047549_f64),
            Cond::Ge(48, 62.524777166172_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 48 btcusdt_5m_rules_48: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(10, 0.01753783257_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 49 btcusdt_5m_rules_49: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Le(21, -0.0156322462_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 50 btcusdt_5m_rules_50: RED
    (
        false,
        &[
            Cond::Ge(70, 93.669724770642_f64),
            Cond::Le(24, -0.004226946089_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 51 btcusdt_5m_rules_51: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.01911639022_f64),
            Cond::Le(26, -0.001380130085_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 52 btcusdt_5m_rules_52: RED
    (
        false,
        &[
            Cond::Ge(69, 99.995600480172_f64),
            Cond::Le(24, -2.58479000000000e-7_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 53 btcusdt_5m_rules_53: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::Ge(62, 32.766297807582_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 54 btcusdt_5m_rules_54: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(30, 0.03468189691_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 55 btcusdt_5m_rules_55: GREEN
    (
        true,
        &[
            Cond::Le(70, 9.711755951452_f64),
            Cond::Ge(73, 24.524084565759_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 56 btcusdt_5m_rules_56: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Between(46, 44.954532800507_f64, 54.831328521965_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 57 btcusdt_5m_rules_57: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(48, 36.682496680971_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 58 btcusdt_5m_rules_58: GREEN
    (
        true,
        &[
            Cond::Le(70, 5.251600440171_f64),
            Cond::Ge(16, -1.114097501656_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 59 btcusdt_5m_rules_59: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 60 btcusdt_5m_rules_60: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Ge(1, 4.325244011802_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 61 btcusdt_5m_rules_61: GREEN
    (
        true,
        &[
            Cond::Le(63, 21.766483332328_f64),
            Cond::Le(2, 0.00127885769_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 62 btcusdt_5m_rules_62: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 63 btcusdt_5m_rules_63: GREEN
    (
        true,
        &[
            Cond::Le(69, 4.179275745964_f64),
            Cond::Ge(49, 1425.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 64 btcusdt_5m_rules_64: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(74, 0.000024786183_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 65 btcusdt_5m_rules_65: GREEN
    (
        true,
        &[
            Cond::Le(69, 3.592157413_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 66 btcusdt_5m_rules_66: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.703514870312_f64),
            Cond::Ge(21, 0.002274079813_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 67 btcusdt_5m_rules_67: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 68 btcusdt_5m_rules_68: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(44, 0.002366750995_f64),
            Cond::Ge(17, 2.046146229_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 69 btcusdt_5m_rules_69: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 98.87542722_f64),
            Cond::Le(59, 0.03681466689_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 70 btcusdt_5m_rules_70: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Ge(24, -0.002580027013_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 71 btcusdt_5m_rules_71: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(0, 14.249712652214_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 72 btcusdt_5m_rules_72: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.06443813881_f64),
            Cond::Le(3, 0.0004654084234_f64),
            Cond::Ge(61, 39.3931585_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 73 btcusdt_5m_rules_73: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(41, 9.0_f64),
            Cond::Le(18, 3.429563387_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 74 btcusdt_5m_rules_74: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(19, 0.590766072643_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 75 btcusdt_5m_rules_75: GREEN
    (
        true,
        &[
            Cond::Le(5, 0.0_f64),
            Cond::Le(62, 25.0_f64),
            Cond::Ge(43, 4.0_f64),
            Cond::Ge(76, 2.0_f64),
            Cond::Eq(81, 2.0_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 76 btcusdt_5m_rules_76: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.443623461596_f64),
            Cond::In(41, &[3.0_f64]),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 77 btcusdt_5m_rules_77: RED
    (
        false,
        &[
            Cond::Ge(48, 90.540588550667_f64),
            Cond::Le(45, -0.002801477945_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 78 btcusdt_5m_rules_78: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.898892702_f64),
            Cond::Le(44, -0.002344176743_f64),
            Cond::Ge(50, 1.403339542_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 79 btcusdt_5m_rules_79: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000268702787_f64),
            Cond::Ge(19, 1.89989821024_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 80 btcusdt_5m_rules_80: RED
    (
        false,
        &[
            Cond::Ge(63, 73.931490170639_f64),
            Cond::Le(51, -1.61225812853_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 81 btcusdt_5m_rules_81: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.1833565_f64),
            Cond::Ge(17, -2.058232069_f64),
            Cond::Le(42, 0.001092316795_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 82 btcusdt_5m_rules_82: RED
    (
        false,
        &[
            Cond::Ge(69, 94.73247534402_f64),
            Cond::Ge(1, 0.834297868579_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 83 btcusdt_5m_rules_83: RED
    (
        false,
        &[
            Cond::Ge(70, 94.755383566354_f64),
            Cond::Le(68, -0.942445884865_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 84 btcusdt_5m_rules_84: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.048300974027_f64),
            Cond::Le(11, -0.887448173015_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 85 btcusdt_5m_rules_85: RED
    (
        false,
        &[
            Cond::Ge(24, -0.001005628718_f64),
            Cond::Le(44, -0.00032682283_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 86 btcusdt_5m_rules_86: GREEN
    (
        true,
        &[
            Cond::Le(48, 13.48098558_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(3, 0.00646353357_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 87 btcusdt_5m_rules_87: GREEN
    (
        true,
        &[
            Cond::Le(70, 4.056578989833_f64),
            Cond::Ge(33, 10.0_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 88 btcusdt_5m_rules_88: GREEN
    (
        true,
        &[
            Cond::Le(70, 6.753473519311_f64),
            Cond::Le(6, 0.000139017832_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 89 btcusdt_5m_rules_89: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Le(42, 0.0_f64),
            Cond::Le(72, 0.001347937908_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 90 btcusdt_5m_rules_90: RED
    (
        false,
        &[
            Cond::Ge(69, 99.185382275768_f64),
            Cond::Le(80, -0.009431026424_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 91 btcusdt_5m_rules_91: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.127057896226_f64),
            Cond::Le(1, -1.893164059008_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 92 btcusdt_5m_rules_92: RED
    (
        false,
        &[
            Cond::Ge(70, 98.04361321_f64),
            Cond::Ge(8, 0.02432418065_f64),
            Cond::Ge(37, 3.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 93 btcusdt_5m_rules_93: GREEN
    (
        true,
        &[
            Cond::Le(62, 22.509371455114_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 94 btcusdt_5m_rules_94: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 95 btcusdt_5m_rules_95: RED
    (
        false,
        &[
            Cond::Ge(70, 89.373499384241_f64),
            Cond::Le(60, 53.275547235097_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 96 btcusdt_5m_rules_96: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(10, 0.01753783257_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 97 btcusdt_5m_rules_97: RED
    (
        false,
        &[
            Cond::Ge(13, 263.810563801908_f64),
            Cond::Le(40, 0.403524161953_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 98 btcusdt_5m_rules_98: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.235606694343_f64),
            Cond::Ge(79, 0.001053177884_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 99 btcusdt_5m_rules_99: GREEN
    (
        true,
        &[
            Cond::Le(69, 3.503757061587_f64),
            Cond::Le(1, -0.75450257826_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 100 btcusdt_5m_rules_100: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(25, -0.016115379302_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 101 btcusdt_5m_rules_101: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(12, 262.3666355551_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 102 btcusdt_5m_rules_102: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 103 btcusdt_5m_rules_103: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.00237146059_f64),
            Cond::Ge(60, 68.072657708997_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 104 btcusdt_5m_rules_104: RED
    (
        false,
        &[
            Cond::Ge(69, 94.73247534402_f64),
            Cond::Ge(1, 0.834297868579_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 105 btcusdt_5m_rules_105: GREEN
    (
        true,
        &[
            Cond::Le(63, 21.766483332328_f64),
            Cond::Le(2, 0.00127885769_f64),
            Cond::Eq(41, 10.0_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 106 btcusdt_5m_rules_106: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::In(81, &[5.0_f64]),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 107 btcusdt_5m_rules_107: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.5066458518_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(42, 9.29093662300000e-8_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 108 btcusdt_5m_rules_108: RED
    (
        false,
        &[
            Cond::Ge(69, 89.541313522713_f64),
            Cond::Le(40, 0.405699915549_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 109 btcusdt_5m_rules_109: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404179203_f64),
            Cond::Eq(41, 14.0_f64),
            Cond::Le(59, -0.01475596055_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 110 btcusdt_5m_rules_110: GREEN
    (
        true,
        &[
            Cond::Le(70, 0.787062464583_f64),
            Cond::Ge(45, 0.000533890036_f64),
        ],
    ),
    // 111 btcusdt_5m_rules_111: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.5066458518_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(42, 9.29093662300000e-8_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 112 btcusdt_5m_rules_112: RED
    (
        false,
        &[
            Cond::Ge(44, 0.00186594868_f64),
            Cond::Ge(69, 97.66150155_f64),
            Cond::Ge(37, 4.0_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 113 btcusdt_5m_rules_113: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(46, 27.45320025_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 114 btcusdt_5m_rules_114: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 115 btcusdt_5m_rules_115: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.00030861933_f64),
            Cond::Ge(49, 1430.0_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 116 btcusdt_5m_rules_116: GREEN
    (
        true,
        &[
            Cond::Ge(76, 4.86378068509_f64),
            Cond::In(81, &[0.0_f64]),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 117 btcusdt_5m_rules_117: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 118 btcusdt_5m_rules_118: RED
    (
        false,
        &[
            Cond::Ge(69, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 119 btcusdt_5m_rules_119: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(10, -0.01817602056_f64),
            Cond::Ge(78, 3.587853962_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 120 btcusdt_5m_rules_120: GREEN
    (
        true,
        &[
            Cond::Le(62, 11.592876344486_f64),
            Cond::Ge(46, 34.134452110302_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 121 btcusdt_5m_rules_121: GREEN
    (
        true,
        &[
            Cond::Le(62, 24.489145820882_f64),
            Cond::Ge(79, 0.003054208916_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 122 btcusdt_5m_rules_122: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(21, -0.005476785129_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 123 btcusdt_5m_rules_123: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 124 btcusdt_5m_rules_124: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(60, 35.01130025_f64),
            Cond::Le(21, -0.003142104413_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 125 btcusdt_5m_rules_125: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(30, 0.0005943956918_f64),
            Cond::Le(42, 3.70201037800000e-7_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 126 btcusdt_5m_rules_126: GREEN
    (
        true,
        &[
            Cond::Le(60, 34.187315401094_f64),
            Cond::Le(19, 0.519485662844_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 127 btcusdt_5m_rules_127: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.218185817798_f64),
            Cond::Le(1, -6.984363537212_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 128 btcusdt_5m_rules_128: RED
    (
        false,
        &[
            Cond::Ge(69, 92.430123495695_f64),
            Cond::Le(76, 0.283114920997_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 129 btcusdt_5m_rules_129: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Ge(12, -124.741159730216_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 130 btcusdt_5m_rules_130: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(10, -0.004083019831_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 131 btcusdt_5m_rules_131: RED
    (
        false,
        &[Cond::Ge(36, 5.0_f64), Cond::Le(60, 30.070530512101_f64)],
    ),
    // 132 btcusdt_5m_rules_132: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(19, 0.590766072643_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 133 btcusdt_5m_rules_133: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(10, -0.004083019831_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 134 btcusdt_5m_rules_134: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Ge(44, 0.000140769183_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 135 btcusdt_5m_rules_135: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::In(41, &[12.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 136 btcusdt_5m_rules_136: GREEN
    (
        true,
        &[
            Cond::Le(12, -177.186804964052_f64),
            Cond::Le(11, -0.286078754321_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 137 btcusdt_5m_rules_137: RED
    (
        false,
        &[
            Cond::Ge(62, 78.115236912005_f64),
            Cond::Le(13, 47.198310116171_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 138 btcusdt_5m_rules_138: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.110521327309_f64),
            Cond::Ge(0, 1.373119792908_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 139 btcusdt_5m_rules_139: RED
    (
        false,
        &[
            Cond::Ge(70, 83.360348764515_f64),
            Cond::Le(77, -1.509021822217_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 140 btcusdt_5m_rules_140: RED
    (
        false,
        &[
            Cond::Ge(46, 82.208371283464_f64),
            Cond::Le(15, 0.000186502931_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 141 btcusdt_5m_rules_141: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000596058362_f64),
            Cond::Ge(0, 13.480756029953_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 142 btcusdt_5m_rules_142: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(77, -1.457487435051_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 143 btcusdt_5m_rules_143: GREEN
    (
        true,
        &[
            Cond::Le(70, 15.180011010737_f64),
            Cond::Ge(73, 102.022412698827_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 144 btcusdt_5m_rules_144: RED
    (
        false,
        &[
            Cond::Ge(63, 70.510977130054_f64),
            Cond::Ge(33, 11.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 145 btcusdt_5m_rules_145: GREEN
    (
        true,
        &[
            Cond::Le(71, 6.986747793_f64),
            Cond::Le(7, 0.03305785124_f64),
            Cond::Ge(12, -89.88475125_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 146 btcusdt_5m_rules_146: RED
    (
        false,
        &[
            Cond::Ge(40, 0.80253046606_f64),
            Cond::Ge(73, 15.903107859896_f64),
            Cond::Eq(81, 3.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 147 btcusdt_5m_rules_147: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(2, 0.005489115066_f64),
            Cond::Le(3, 0.003517992609_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 148 btcusdt_5m_rules_148: GREEN
    (
        true,
        &[
            Cond::Le(46, 10.339569656362_f64),
            Cond::Le(33, 2.0_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 149 btcusdt_5m_rules_149: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(61, 31.37459303_f64),
            Cond::Le(30, 0.0001481980796_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 150 btcusdt_5m_rules_150: RED
    (
        false,
        &[
            Cond::Ge(16, 2.589440698101_f64),
            Cond::Le(23, -0.001943115236_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 151 btcusdt_5m_rules_151: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(75, 0.005915796217_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 152 btcusdt_5m_rules_152: RED
    (
        false,
        &[
            Cond::Ge(62, 76.287641686518_f64),
            Cond::Le(77, -0.939474731553_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 153 btcusdt_5m_rules_153: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Ge(74, 0.005051760632_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 154 btcusdt_5m_rules_154: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.012972255848_f64),
            Cond::Ge(25, -0.00215355613_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 155 btcusdt_5m_rules_155: RED
    (
        false,
        &[
            Cond::Ge(4, 0.944943849905_f64),
            Cond::Ge(2, 0.014808079873_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 156 btcusdt_5m_rules_156: GREEN
    (
        true,
        &[
            Cond::Le(35, 0.0_f64),
            Cond::Le(54, 1.0_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 157 btcusdt_5m_rules_157: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(34, 0.0_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 158 btcusdt_5m_rules_158: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(2, 0.001277921698_f64),
            Cond::Le(47, 33.22794653_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 159 btcusdt_5m_rules_159: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.695232436063_f64),
            Cond::Ge(1, 3.927244001484_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 160 btcusdt_5m_rules_160: RED
    (
        false,
        &[
            Cond::Ge(17, 1.806249404164_f64),
            Cond::Le(80, -0.014146164354_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 161 btcusdt_5m_rules_161: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.997196098696_f64),
            Cond::Le(0, -11.652215766136_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 162 btcusdt_5m_rules_162: RED
    (
        false,
        &[
            Cond::Ge(63, 85.785356550293_f64),
            Cond::In(41, &[11.0_f64]),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 163 btcusdt_5m_rules_163: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Ge(24, -0.007190350995_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 164 btcusdt_5m_rules_164: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Le(70, 96.70846245_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 165 btcusdt_5m_rules_165: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.880226526547_f64),
            Cond::Ge(56, -0.002710522059_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 166 btcusdt_5m_rules_166: RED
    (
        false,
        &[
            Cond::Ge(69, 87.60245942582_f64),
            Cond::Le(48, 29.338239708836_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 167 btcusdt_5m_rules_167: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.084711620958_f64),
            Cond::Le(50, 0.692354618129_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 168 btcusdt_5m_rules_168: RED
    (
        false,
        &[
            Cond::Ge(69, 87.582148468611_f64),
            Cond::Le(1, -20.780423400716_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 169 btcusdt_5m_rules_169: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.0088103787_f64),
            Cond::Le(77, -0.99874379869_f64),
        ],
    ),
    // 170 btcusdt_5m_rules_170: GREEN
    (
        true,
        &[
            Cond::Le(69, 2.062588143616_f64),
            Cond::Le(46, 9.622580642198_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 171 btcusdt_5m_rules_171: RED
    (
        false,
        &[
            Cond::Ge(70, 89.551972381464_f64),
            Cond::Le(68, -1.939490804131_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 172 btcusdt_5m_rules_172: RED
    (
        false,
        &[
            Cond::Ge(70, 98.060537876782_f64),
            Cond::Le(0, -1.614870643081_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 173 btcusdt_5m_rules_173: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(41, &[21.0_f64, 22.0_f64, 23.0_f64]),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 174 btcusdt_5m_rules_174: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266367569_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Le(48, 14.51065799_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 175 btcusdt_5m_rules_175: RED
    (
        false,
        &[
            Cond::Ge(40, 0.712764986117_f64),
            Cond::Le(48, 21.010779204058_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 176 btcusdt_5m_rules_176: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 177 btcusdt_5m_rules_177: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.307627154465_f64),
            Cond::Ge(45, 0.001241223785_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 178 btcusdt_5m_rules_178: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.004905605597_f64),
            Cond::Le(51, -1.563227737634_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 179 btcusdt_5m_rules_179: RED
    (
        false,
        &[
            Cond::Ge(70, 96.0335550175_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 180 btcusdt_5m_rules_180: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.00254012653_f64),
            Cond::Ge(42, 0.002459555014_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 181 btcusdt_5m_rules_181: RED
    (
        false,
        &[
            Cond::Ge(63, 80.15965415611_f64),
            Cond::Le(51, -0.979734943474_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 182 btcusdt_5m_rules_182: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404179203_f64),
            Cond::Le(24, -0.02387268949_f64),
            Cond::Le(36, 1.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 183 btcusdt_5m_rules_183: RED
    (
        false,
        &[
            Cond::Ge(48, 94.13387702_f64),
            Cond::Ge(24, -0.0001214530509_f64),
            Cond::Le(70, 99.45945562_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 184 btcusdt_5m_rules_184: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000430553703_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 185 btcusdt_5m_rules_185: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(47, 73.95404425_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 186 btcusdt_5m_rules_186: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Ge(75, 0.01004429117_f64),
        ],
    ),
    // 187 btcusdt_5m_rules_187: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(59, 0.0242546669_f64),
            Cond::Ge(8, -0.007039969776_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 188 btcusdt_5m_rules_188: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.330049175336_f64),
            Cond::Le(77, -0.74850086188_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 189 btcusdt_5m_rules_189: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(2, 0.007385036543_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 190 btcusdt_5m_rules_190: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.898892702_f64),
            Cond::Le(24, -0.02994954907_f64),
            Cond::Le(48, 14.51065799_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 191 btcusdt_5m_rules_191: RED
    (
        false,
        &[
            Cond::Ge(60, 81.688139538765_f64),
            Cond::Ge(77, 3.746261866779_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 192 btcusdt_5m_rules_192: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Le(70, 96.70846245_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 193 btcusdt_5m_rules_193: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 194 btcusdt_5m_rules_194: RED
    (
        false,
        &[
            Cond::Ge(17, 2.041451492706_f64),
            Cond::Le(45, -0.001221253105_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 195 btcusdt_5m_rules_195: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404179203_f64),
            Cond::Eq(41, 14.0_f64),
            Cond::Le(59, -0.01475596055_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 196 btcusdt_5m_rules_196: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.160650711145_f64),
            Cond::Ge(0, 4.657215509588_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 197 btcusdt_5m_rules_197: GREEN
    (
        true,
        &[
            Cond::Le(62, 23.828421748691_f64),
            Cond::Ge(70, 43.277212100048_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 198 btcusdt_5m_rules_198: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 199 btcusdt_5m_rules_199: RED
    (
        false,
        &[
            Cond::Le(52, 0.0_f64),
            Cond::Ge(33, 10.0_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 200 btcusdt_5m_rules_200: RED
    (
        false,
        &[
            Cond::Ge(63, 85.785356550293_f64),
            Cond::Le(60, 74.332784822998_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 201 btcusdt_5m_rules_201: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(26, -0.001004677022_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 202 btcusdt_5m_rules_202: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000886273409_f64),
            Cond::Le(44, -0.001638248858_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 203 btcusdt_5m_rules_203: RED
    (
        false,
        &[
            Cond::Ge(69, 89.541313522713_f64),
            Cond::Le(77, -1.385238691491_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 204 btcusdt_5m_rules_204: RED
    (
        false,
        &[
            Cond::Ge(70, 96.0335550175_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 205 btcusdt_5m_rules_205: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Between(48, 37.154692990356_f64, 62.524777166172_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 206 btcusdt_5m_rules_206: RED
    (
        false,
        &[
            Cond::Ge(62, 67.899626426361_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 207 btcusdt_5m_rules_207: RED
    (
        false,
        &[
            Cond::Ge(70, 98.13255444133_f64),
            Cond::Ge(19, 2.511911306736_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 208 btcusdt_5m_rules_208: GREEN
    (
        true,
        &[
            Cond::Le(13, -183.059643654916_f64),
            Cond::Le(15, 0.0_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 209 btcusdt_5m_rules_209: RED
    (
        false,
        &[
            Cond::Ge(63, 76.610575342271_f64),
            Cond::Ge(0, 14.249712652214_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 210 btcusdt_5m_rules_210: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.583474519884_f64),
            Cond::Ge(40, 0.603485537608_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 211 btcusdt_5m_rules_211: RED
    (
        false,
        &[
            Cond::Ge(70, 94.302371417231_f64),
            Cond::Le(58, -0.000384754229_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 212 btcusdt_5m_rules_212: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.417455072988_f64),
            Cond::Ge(17, -1.412432258616_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 213 btcusdt_5m_rules_213: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000268702787_f64),
            Cond::Ge(19, 1.89989821024_f64),
            Cond::Eq(81, 6.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 214 btcusdt_5m_rules_214: GREEN
    (
        true,
        &[
            Cond::Le(12, -169.388968825084_f64),
            Cond::Ge(15, 1.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 215 btcusdt_5m_rules_215: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 216 btcusdt_5m_rules_216: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.114097501656_f64),
            Cond::Ge(4, 0.615571013053_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 217 btcusdt_5m_rules_217: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 218 btcusdt_5m_rules_218: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.129202691063_f64),
            Cond::Le(2, 0.000647938293_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 219 btcusdt_5m_rules_219: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.04489019383_f64),
            Cond::Ge(7, 0.9987638412_f64),
            Cond::Ge(13, -201.1674785_f64),
            Cond::Eq(41, 9.0_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 220 btcusdt_5m_rules_220: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.334336613744_f64),
            Cond::Ge(46, 75.524132937806_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 221 btcusdt_5m_rules_221: RED
    (
        false,
        &[
            Cond::Ge(48, 88.952653717272_f64),
            Cond::Ge(0, 13.915733334115_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 222 btcusdt_5m_rules_222: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(30, 0.0005943956918_f64),
            Cond::Le(42, 3.70201037800000e-7_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 223 btcusdt_5m_rules_223: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.110392815456_f64),
            Cond::Ge(1, 5.602555707271_f64),
            Cond::Eq(66, 1.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 224 btcusdt_5m_rules_224: RED
    (
        false,
        &[
            Cond::Ge(63, 83.73543324921_f64),
            Cond::Le(80, -0.004060985168_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 225 btcusdt_5m_rules_225: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 226 btcusdt_5m_rules_226: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 39.31642972534_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 227 btcusdt_5m_rules_227: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.01911639022_f64),
            Cond::Le(26, -0.001380130085_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 228 btcusdt_5m_rules_228: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 229 btcusdt_5m_rules_229: GREEN
    (
        true,
        &[
            Cond::Le(44, -0.002650404386_f64),
            Cond::Le(69, 5.152344313_f64),
            Cond::Le(70, 3.478803314_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 230 btcusdt_5m_rules_230: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(60, 27.52356397_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 231 btcusdt_5m_rules_231: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.127057896226_f64),
            Cond::Le(1, -1.893164059008_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 232 btcusdt_5m_rules_232: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(2, 0.007385036543_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 233 btcusdt_5m_rules_233: RED
    (
        false,
        &[
            Cond::Ge(17, 2.041451492706_f64),
            Cond::Le(45, -0.001221253105_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 234 btcusdt_5m_rules_234: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(51, 1.413382543016_f64),
            Cond::Eq(66, 1.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 235 btcusdt_5m_rules_235: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.6_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 236 btcusdt_5m_rules_236: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 237 btcusdt_5m_rules_237: RED
    (
        false,
        &[
            Cond::Ge(63, 82.454573967079_f64),
            Cond::Le(19, 0.639508691164_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 238 btcusdt_5m_rules_238: RED
    (
        false,
        &[
            Cond::Ge(69, 92.58675389879_f64),
            Cond::Le(15, 0.306261093449_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 239 btcusdt_5m_rules_239: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 240 btcusdt_5m_rules_240: RED
    (
        false,
        &[
            Cond::Ge(63, 85.785356550293_f64),
            Cond::Ge(34, 5.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 241 btcusdt_5m_rules_241: GREEN
    (
        true,
        &[
            Cond::Le(60, 21.149731261368_f64),
            Cond::Ge(13, -88.21972132586_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 242 btcusdt_5m_rules_242: GREEN
    (
        true,
        &[
            Cond::Le(63, 35.054701915527_f64),
            Cond::Ge(60, 47.567686159211_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 243 btcusdt_5m_rules_243: RED
    (
        false,
        &[
            Cond::Ge(63, 70.510977130054_f64),
            Cond::Le(19, 0.421734393474_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 244 btcusdt_5m_rules_244: RED
    (
        false,
        &[
            Cond::Ge(4, 1.075503944993_f64),
            Cond::Le(51, -0.822178298076_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 245 btcusdt_5m_rules_245: RED
    (
        false,
        &[
            Cond::Ge(16, 1.908788195242_f64),
            Cond::Le(50, 0.47715417535_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 246 btcusdt_5m_rules_246: GREEN
    (
        true,
        &[
            Cond::Le(12, -188.547426560093_f64),
            Cond::Le(51, -0.638402427824_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 247 btcusdt_5m_rules_247: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.126060052744_f64),
            Cond::Le(51, -0.975669526392_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 248 btcusdt_5m_rules_248: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::In(81, &[5.0_f64]),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 249 btcusdt_5m_rules_249: RED
    (
        false,
        &[
            Cond::Ge(17, 2.299998090695_f64),
            Cond::Le(77, -0.990552977922_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 250 btcusdt_5m_rules_250: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.004499095898_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 251 btcusdt_5m_rules_251: RED
    (
        false,
        &[
            Cond::Ge(70, 92.398919564528_f64),
            Cond::Le(69, 78.724397723401_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 252 btcusdt_5m_rules_252: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(30, 0.0005943956918_f64),
            Cond::Le(42, 3.70201037800000e-7_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 253 btcusdt_5m_rules_253: RED
    (
        false,
        &[
            Cond::Ge(63, 84.02165584_f64),
            Cond::Le(3, 0.0009239513225_f64),
            Cond::Ge(43, 0.01797752809_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 254 btcusdt_5m_rules_254: GREEN
    (
        true,
        &[
            Cond::Le(70, 15.545991535082_f64),
            Cond::Ge(13, -21.140333967991_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 255 btcusdt_5m_rules_255: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.371343923784_f64),
            Cond::Ge(58, 0.00710213073_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 256 btcusdt_5m_rules_256: GREEN
    (
        true,
        &[
            Cond::Ge(53, 5.0_f64),
            Cond::Ge(40, 0.713503577816_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 257 btcusdt_5m_rules_257: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 258 btcusdt_5m_rules_258: RED
    (
        false,
        &[
            Cond::Ge(62, 87.21365662096_f64),
            Cond::In(81, &[6.0_f64]),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 259 btcusdt_5m_rules_259: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(75, 0.005915796217_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 260 btcusdt_5m_rules_260: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.443623461596_f64),
            Cond::Ge(15, 0.810041941282_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 261 btcusdt_5m_rules_261: GREEN
    (
        true,
        &[
            Cond::Le(13, -192.451948846018_f64),
            Cond::Le(50, 0.456323118123_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 262 btcusdt_5m_rules_262: RED
    (
        false,
        &[
            Cond::Ge(10, 0.008390907843_f64),
            Cond::Ge(24, -0.0003773777571_f64),
            Cond::Le(42, 0.0_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 263 btcusdt_5m_rules_263: GREEN
    (
        true,
        &[
            Cond::Le(62, 37.285529400902_f64),
            Cond::Ge(80, 0.021524632953_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 264 btcusdt_5m_rules_264: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(73, 0.151279003962_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 265 btcusdt_5m_rules_265: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(51, -0.052398857241_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 266 btcusdt_5m_rules_266: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.469218562694_f64),
            Cond::Ge(44, 0.000140769183_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 267 btcusdt_5m_rules_267: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.220922553819_f64),
            Cond::Ge(44, 0.000404314765_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 268 btcusdt_5m_rules_268: RED
    (
        false,
        &[
            Cond::Ge(63, 82.454573967079_f64),
            Cond::Le(19, 0.639508691164_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 269 btcusdt_5m_rules_269: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.829683244288_f64),
            Cond::Ge(33, 9.0_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 270 btcusdt_5m_rules_270: RED
    (
        false,
        &[
            Cond::Ge(70, 90.414132730189_f64),
            Cond::Le(35, 0.0_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 271 btcusdt_5m_rules_271: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.563149366712_f64),
            Cond::Ge(79, 0.001902549548_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 272 btcusdt_5m_rules_272: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.583474519884_f64),
            Cond::Le(45, -0.005856406184_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 273 btcusdt_5m_rules_273: RED
    (
        false,
        &[
            Cond::Ge(4, 1.060875802045_f64),
            Cond::Le(39, 0.140781768746_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 274 btcusdt_5m_rules_274: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Le(30, 0.001228070749_f64),
            Cond::Eq(81, 1.0_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 275 btcusdt_5m_rules_275: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(45, 0.004216605253_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 276 btcusdt_5m_rules_276: RED
    (
        false,
        &[
            Cond::Ge(70, 92.398919564528_f64),
            Cond::Le(60, 53.275547235097_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 277 btcusdt_5m_rules_277: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 278 btcusdt_5m_rules_278: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Between(50, 0.754270965462_f64, 0.952041479441_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 279 btcusdt_5m_rules_279: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(10, -0.004965357046_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 280 btcusdt_5m_rules_280: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000993286689_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 281 btcusdt_5m_rules_281: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.130209885596_f64),
            Cond::Ge(13, 47.198310116171_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 282 btcusdt_5m_rules_282: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Between(11, 0.163926820707_f64, 0.45738367187_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 283 btcusdt_5m_rules_283: GREEN
    (
        true,
        &[
            Cond::Le(62, 10.880992792283_f64),
            Cond::Le(6, 0.000391630717_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 284 btcusdt_5m_rules_284: GREEN
    (
        true,
        &[
            Cond::Le(62, 28.288679424359_f64),
            Cond::Ge(13, -19.900866452413_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 285 btcusdt_5m_rules_285: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.01911639022_f64),
            Cond::Ge(14, 301.1917591_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 286 btcusdt_5m_rules_286: GREEN
    (
        true,
        &[
            Cond::Le(18, -2.447691672_f64),
            Cond::Le(3, 0.0006406963614_f64),
            Cond::Le(8, -0.004124824725_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 287 btcusdt_5m_rules_287: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 288 btcusdt_5m_rules_288: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.051658096366_f64),
            Cond::Ge(44, 0.000552259724_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 289 btcusdt_5m_rules_289: GREEN
    (
        true,
        &[
            Cond::Le(62, 18.416929648717_f64),
            Cond::Le(53, 2.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 290 btcusdt_5m_rules_290: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(39, 0.255739517915_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 291 btcusdt_5m_rules_291: RED
    (
        false,
        &[
            Cond::Ge(4, 1.244413952997_f64),
            Cond::Le(38, 0.001148336911_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 292 btcusdt_5m_rules_292: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(18, 3.429563387_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 293 btcusdt_5m_rules_293: RED
    (
        false,
        &[
            Cond::Ge(46, 82.208371283464_f64),
            Cond::Le(40, 0.335678247138_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 294 btcusdt_5m_rules_294: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::In(81, &[3.0_f64]),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 295 btcusdt_5m_rules_295: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.829683244288_f64),
            Cond::Ge(33, 9.0_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 296 btcusdt_5m_rules_296: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(76, 0.319900011809_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 297 btcusdt_5m_rules_297: RED
    (
        false,
        &[
            Cond::Ge(63, 80.23066448_f64),
            Cond::Le(2, 0.000953452966_f64),
            Cond::Ge(47, 78.27879154_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 298 btcusdt_5m_rules_298: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.091259020395_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 299 btcusdt_5m_rules_299: RED
    (
        false,
        &[
            Cond::Ge(40, 0.682488787105_f64),
            Cond::Le(12, -89.913738186233_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 300 btcusdt_5m_rules_300: GREEN
    (
        true,
        &[
            Cond::Le(12, -169.388968825084_f64),
            Cond::Ge(15, 1.0_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 301 btcusdt_5m_rules_301: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.811743944906_f64),
            Cond::Ge(40, 0.665950478981_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 302 btcusdt_5m_rules_302: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Ge(73, 17.001463414648_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 303 btcusdt_5m_rules_303: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 304 btcusdt_5m_rules_304: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.008267982551_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 305 btcusdt_5m_rules_305: GREEN
    (
        true,
        &[
            Cond::Le(62, 16.815653800848_f64),
            Cond::Le(51, -0.781972702142_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 306 btcusdt_5m_rules_306: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(46, 27.45320025_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 307 btcusdt_5m_rules_307: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 308 btcusdt_5m_rules_308: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(28, 0.001091384768_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 309 btcusdt_5m_rules_309: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(10, -0.005522189783_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 310 btcusdt_5m_rules_310: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Ge(74, 0.005051760632_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 311 btcusdt_5m_rules_311: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Le(3, 0.0007461976822_f64),
            Cond::Le(14, -130.1004463_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 312 btcusdt_5m_rules_312: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::In(41, &[2.0_f64]),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 313 btcusdt_5m_rules_313: GREEN
    (
        true,
        &[
            Cond::Le(13, -183.059643654916_f64),
            Cond::Le(15, 0.0_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 314 btcusdt_5m_rules_314: RED
    (
        false,
        &[
            Cond::Ge(16, 1.908788195242_f64),
            Cond::Le(50, 0.47715417535_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 315 btcusdt_5m_rules_315: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(6, 0.007864217465_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 316 btcusdt_5m_rules_316: RED
    (
        false,
        &[
            Cond::Ge(48, 94.13387702_f64),
            Cond::Ge(24, -0.0001214530509_f64),
            Cond::Le(70, 99.45945562_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 317 btcusdt_5m_rules_317: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 21.0_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 318 btcusdt_5m_rules_318: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(20, -0.000850585157_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 319 btcusdt_5m_rules_319: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 22.0_f64),
            Cond::Le(72, 9.22394520900000e-8_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 320 btcusdt_5m_rules_320: RED
    (
        false,
        &[
            Cond::Ge(60, 82.623793495996_f64),
            Cond::Between(2, 0.006117730378_f64, 0.007861395294_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 321 btcusdt_5m_rules_321: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.822812092734_f64),
            Cond::Le(45, -0.009895985427_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 322 btcusdt_5m_rules_322: GREEN
    (
        true,
        &[
            Cond::Le(44, -0.002650404386_f64),
            Cond::Le(69, 5.152344313_f64),
            Cond::Le(70, 3.478803314_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 323 btcusdt_5m_rules_323: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(20, -0.000850585157_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 324 btcusdt_5m_rules_324: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.160811012751_f64),
            Cond::Le(76, 0.697599259542_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 325 btcusdt_5m_rules_325: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::Ge(7, 0.841316476733_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 326 btcusdt_5m_rules_326: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Between(48, 43.719050398098_f64, 56.000133414606_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 327 btcusdt_5m_rules_327: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Between(7, 0.348371370028_f64, 0.50546791832_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 328 btcusdt_5m_rules_328: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(41, 9.0_f64),
            Cond::Le(18, 3.429563387_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 329 btcusdt_5m_rules_329: RED
    (
        false,
        &[
            Cond::Ge(4, 1.242274122_f64),
            Cond::Eq(81, 3.0_f64),
            Cond::Le(78, 2.098463339_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 330 btcusdt_5m_rules_330: RED
    (
        false,
        &[
            Cond::Ge(70, 96.167908771068_f64),
            Cond::Le(25, -0.001936103113_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 331 btcusdt_5m_rules_331: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(80, -0.023171858787_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 332 btcusdt_5m_rules_332: RED
    (
        false,
        &[
            Cond::Ge(62, 76.287641686518_f64),
            Cond::Le(77, -0.992604162908_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 333 btcusdt_5m_rules_333: GREEN
    (
        true,
        &[
            Cond::Le(70, 9.590121893836_f64),
            Cond::Le(11, -0.887448173015_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 334 btcusdt_5m_rules_334: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(11, -0.743751369455_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 335 btcusdt_5m_rules_335: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 336 btcusdt_5m_rules_336: RED
    (
        false,
        &[
            Cond::Ge(70, 96.0335550175_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 337 btcusdt_5m_rules_337: RED
    (
        false,
        &[
            Cond::Ge(62, 78.115236912005_f64),
            Cond::Le(13, 47.198310116171_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 338 btcusdt_5m_rules_338: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 339 btcusdt_5m_rules_339: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.970296088074_f64),
            Cond::Ge(68, 1.841596579591_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 340 btcusdt_5m_rules_340: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 13.0_f64),
            Cond::Ge(48, 25.84167143_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 341 btcusdt_5m_rules_341: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(15, 0.000045742666_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 342 btcusdt_5m_rules_342: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 343 btcusdt_5m_rules_343: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Ge(62, 75.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(81, 3.0_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 344 btcusdt_5m_rules_344: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.997196098696_f64),
            Cond::Le(50, 0.391660049444_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 345 btcusdt_5m_rules_345: RED
    (
        false,
        &[
            Cond::Ge(60, 77.03278368_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Le(78, 2.919374564_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 346 btcusdt_5m_rules_346: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 347 btcusdt_5m_rules_347: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.829683244288_f64),
            Cond::Ge(46, 44.954532800507_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 348 btcusdt_5m_rules_348: RED
    (
        false,
        &[
            Cond::Ge(48, 94.13387702_f64),
            Cond::Ge(24, -0.0001214530509_f64),
            Cond::Le(70, 99.45945562_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 349 btcusdt_5m_rules_349: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 350 btcusdt_5m_rules_350: RED
    (
        false,
        &[
            Cond::Ge(16, 1.722377357787_f64),
            Cond::Ge(1, 25.309101221933_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 351 btcusdt_5m_rules_351: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.836158938595_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 352 btcusdt_5m_rules_352: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(18, 3.429563387_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 353 btcusdt_5m_rules_353: RED
    (
        false,
        &[
            Cond::Ge(63, 70.589612029705_f64),
            Cond::Le(46, 40.166548899026_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 354 btcusdt_5m_rules_354: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(18, 3.429563387_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 355 btcusdt_5m_rules_355: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.056247011082_f64),
            Cond::Ge(32, 1.0_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 356 btcusdt_5m_rules_356: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006709289818_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Le(78, 2.912906413_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 357 btcusdt_5m_rules_357: RED
    (
        false,
        &[
            Cond::Ge(60, 79.132602591729_f64),
            Cond::Le(11, -0.134109392588_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 358 btcusdt_5m_rules_358: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(14, -153.295298_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 359 btcusdt_5m_rules_359: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.056247011082_f64),
            Cond::Ge(32, 1.0_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 360 btcusdt_5m_rules_360: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 39.31642972534_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 361 btcusdt_5m_rules_361: RED
    (
        false,
        &[
            Cond::Ge(63, 85.785356550293_f64),
            Cond::Ge(0, 7.069171215076_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 362 btcusdt_5m_rules_362: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(60, 35.01130025_f64),
            Cond::Le(21, -0.003142104413_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 363 btcusdt_5m_rules_363: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(21, -0.005476785129_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 364 btcusdt_5m_rules_364: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(1, -39.147158960848_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 365 btcusdt_5m_rules_365: RED
    (
        false,
        &[
            Cond::Ge(69, 96.3831114696_f64),
            Cond::Le(24, -0.002433279017_f64),
        ],
    ),
    // 366 btcusdt_5m_rules_366: GREEN
    (
        true,
        &[
            Cond::Le(24, -0.03797357864_f64),
            Cond::Le(47, 13.8331558_f64),
            Cond::Le(18, -3.063615586_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 367 btcusdt_5m_rules_367: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.307627154465_f64),
            Cond::Ge(45, 0.001241223785_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 368 btcusdt_5m_rules_368: GREEN
    (
        true,
        &[
            Cond::Le(13, -183.059643654916_f64),
            Cond::Le(15, 0.0_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 369 btcusdt_5m_rules_369: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(2, 0.001277921698_f64),
            Cond::Le(47, 33.22794653_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 370 btcusdt_5m_rules_370: RED
    (
        false,
        &[
            Cond::Ge(48, 92.538838851052_f64),
            Cond::Le(40, 0.405699915549_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 371 btcusdt_5m_rules_371: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::Ge(10, -0.000689286324_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 372 btcusdt_5m_rules_372: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Le(70, 96.70846245_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 373 btcusdt_5m_rules_373: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.005548156292_f64),
            Cond::Le(8, -0.03657074613_f64),
            Cond::Le(46, 21.44107346_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 374 btcusdt_5m_rules_374: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 375 btcusdt_5m_rules_375: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.1833565_f64),
            Cond::Le(3, 0.0006406963614_f64),
            Cond::Ge(63, 16.98438998_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 376 btcusdt_5m_rules_376: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.116199896442_f64),
            Cond::Ge(29, 0.017910613907_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 377 btcusdt_5m_rules_377: RED
    (
        false,
        &[
            Cond::Ge(70, 98.04361321_f64),
            Cond::Ge(8, 0.02432418065_f64),
            Cond::Ge(37, 3.0_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 378 btcusdt_5m_rules_378: GREEN
    (
        true,
        &[
            Cond::Le(48, 13.48098558_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(3, 0.00646353357_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 379 btcusdt_5m_rules_379: RED
    (
        false,
        &[
            Cond::Ge(62, 79.187266351359_f64),
            Cond::Ge(74, 0.007704921362_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 380 btcusdt_5m_rules_380: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::In(81, &[2.0_f64]),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 381 btcusdt_5m_rules_381: GREEN
    (
        true,
        &[
            Cond::Le(62, 19.291134799525_f64),
            Cond::Ge(43, 103.358300394673_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 382 btcusdt_5m_rules_382: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.695232436063_f64),
            Cond::Ge(1, 3.927244001484_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 383 btcusdt_5m_rules_383: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Le(50, 0.692354618129_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 384 btcusdt_5m_rules_384: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 385 btcusdt_5m_rules_385: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.880226526547_f64),
            Cond::Ge(56, -0.002710522059_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 386 btcusdt_5m_rules_386: GREEN
    (
        true,
        &[
            Cond::Le(9, -0.005375380189_f64),
            Cond::Ge(60, 72.313585043492_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 387 btcusdt_5m_rules_387: RED
    (
        false,
        &[
            Cond::Ge(63, 80.23066448_f64),
            Cond::Le(18, 1.456773235_f64),
            Cond::Le(46, 73.70646328_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 388 btcusdt_5m_rules_388: RED
    (
        false,
        &[
            Cond::Ge(17, 3.093219443607_f64),
            Cond::Le(2, 0.000515254142_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 389 btcusdt_5m_rules_389: GREEN
    (
        true,
        &[
            Cond::Le(69, 4.796469453139_f64),
            Cond::Le(1, -8.733281848762_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 390 btcusdt_5m_rules_390: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(15, 0.029402232243_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 391 btcusdt_5m_rules_391: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(45, 0.004216605253_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 392 btcusdt_5m_rules_392: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 393 btcusdt_5m_rules_393: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(46, 27.45320025_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 394 btcusdt_5m_rules_394: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(12, -213.8206725_f64),
            Cond::Ge(71, 2.887028121_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 395 btcusdt_5m_rules_395: RED
    (
        false,
        &[
            Cond::Ge(60, 81.688139538765_f64),
            Cond::Ge(0, 1.293775146209_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 396 btcusdt_5m_rules_396: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Ge(24, -0.007190350995_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 397 btcusdt_5m_rules_397: RED
    (
        false,
        &[
            Cond::Ge(60, 77.03278368_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Le(78, 2.919374564_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 398 btcusdt_5m_rules_398: RED
    (
        false,
        &[
            Cond::Ge(48, 92.776570198665_f64),
            Cond::Ge(33, 10.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 399 btcusdt_5m_rules_399: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.880226526547_f64),
            Cond::Ge(56, -0.002710522059_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 400 btcusdt_5m_rules_400: GREEN
    (
        true,
        &[
            Cond::Le(63, 15.184312318203_f64),
            Cond::Le(51, -0.464821755383_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 401 btcusdt_5m_rules_401: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 29.891000233355_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 402 btcusdt_5m_rules_402: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Le(2, 0.0007499679689_f64),
            Cond::Le(18, -3.032681751_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 403 btcusdt_5m_rules_403: GREEN
    (
        true,
        &[
            Cond::Le(70, 9.711755951452_f64),
            Cond::Ge(43, 50.815714285668_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 404 btcusdt_5m_rules_404: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 405 btcusdt_5m_rules_405: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.824148755859_f64),
            Cond::Ge(75, 0.015517215474_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 406 btcusdt_5m_rules_406: RED
    (
        false,
        &[
            Cond::Ge(63, 73.931490170639_f64),
            Cond::Le(51, -1.61225812853_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 407 btcusdt_5m_rules_407: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(75, 0.005915796217_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 408 btcusdt_5m_rules_408: RED
    (
        false,
        &[
            Cond::Ge(69, 99.185382275768_f64),
            Cond::Le(80, -0.009431026424_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 409 btcusdt_5m_rules_409: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Le(46, 13.050476937067_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 410 btcusdt_5m_rules_410: RED
    (
        false,
        &[
            Cond::Ge(69, 93.154663125084_f64),
            Cond::Le(77, -1.38757033289_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 411 btcusdt_5m_rules_411: RED
    (
        false,
        &[
            Cond::Ge(16, 2.082088491926_f64),
            Cond::Le(45, -0.004456144174_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 412 btcusdt_5m_rules_412: RED
    (
        false,
        &[
            Cond::Ge(70, 98.13255444133_f64),
            Cond::Ge(19, 2.511911306736_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 413 btcusdt_5m_rules_413: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.006182723316_f64),
            Cond::Le(75, 0.001159553433_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 414 btcusdt_5m_rules_414: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.6_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 415 btcusdt_5m_rules_415: RED
    (
        false,
        &[
            Cond::Ge(4, 1.060875802045_f64),
            Cond::Le(39, 0.140781768746_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 416 btcusdt_5m_rules_416: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00041072089_f64),
            Cond::Ge(45, 0.009375325403_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 417 btcusdt_5m_rules_417: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.022037703312_f64),
            Cond::Ge(19, 1.906223811824_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 418 btcusdt_5m_rules_418: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 419 btcusdt_5m_rules_419: RED
    (
        false,
        &[
            Cond::Ge(63, 83.73543324921_f64),
            Cond::Le(80, -0.004060985168_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 420 btcusdt_5m_rules_420: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.127057896226_f64),
            Cond::Le(1, -1.893164059008_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 421 btcusdt_5m_rules_421: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 39.31642972534_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 422 btcusdt_5m_rules_422: RED
    (
        false,
        &[
            Cond::Ge(10, 0.008390907843_f64),
            Cond::Ge(24, -0.0003773777571_f64),
            Cond::Le(42, 0.0_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 423 btcusdt_5m_rules_423: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.012972255848_f64),
            Cond::Ge(25, -0.00215355613_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 424 btcusdt_5m_rules_424: RED
    (
        false,
        &[
            Cond::Ge(40, 0.772240951118_f64),
            Cond::Le(11, -1.02601582424_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 425 btcusdt_5m_rules_425: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 426 btcusdt_5m_rules_426: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0004654084234_f64),
            Cond::Ge(2, 0.0002988690878_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 427 btcusdt_5m_rules_427: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Le(3, 0.0007461976822_f64),
            Cond::Le(14, -130.1004463_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 428 btcusdt_5m_rules_428: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Ge(68, 2.515643159476_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 429 btcusdt_5m_rules_429: RED
    (
        false,
        &[
            Cond::Ge(17, 3.093219443607_f64),
            Cond::Le(2, 0.000515254142_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 430 btcusdt_5m_rules_430: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(59, 0.0242546669_f64),
            Cond::Ge(8, -0.007039969776_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 431 btcusdt_5m_rules_431: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404171868_f64),
            Cond::Le(38, -0.007640254912_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 432 btcusdt_5m_rules_432: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(12, -213.8206725_f64),
            Cond::Ge(71, 2.887028121_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 433 btcusdt_5m_rules_433: RED
    (
        false,
        &[
            Cond::Ge(16, 2.589440698101_f64),
            Cond::Ge(74, 0.005051760632_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 434 btcusdt_5m_rules_434: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.293693136323_f64),
            Cond::Ge(73, 0.389314900769_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 435 btcusdt_5m_rules_435: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Le(45, -0.001221253105_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 436 btcusdt_5m_rules_436: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(44, 0.002366750995_f64),
            Cond::Ge(17, 2.046146229_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 437 btcusdt_5m_rules_437: RED
    (
        false,
        &[
            Cond::Ge(48, 94.13387702_f64),
            Cond::Ge(24, -0.0001214530509_f64),
            Cond::Le(70, 99.45945562_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 438 btcusdt_5m_rules_438: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(60, 35.01130025_f64),
            Cond::Le(21, -0.003142104413_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 439 btcusdt_5m_rules_439: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 440 btcusdt_5m_rules_440: RED
    (
        false,
        &[
            Cond::Ge(70, 94.755383566354_f64),
            Cond::Le(0, -7.332863959192_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 441 btcusdt_5m_rules_441: RED
    (
        false,
        &[
            Cond::Ge(60, 81.688139538765_f64),
            Cond::Ge(0, 2.347119575634_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 442 btcusdt_5m_rules_442: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::Ge(7, 0.841316476733_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 443 btcusdt_5m_rules_443: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.008177319388_f64),
            Cond::Ge(42, 0.000102169501_f64),
        ],
    ),
    // 444 btcusdt_5m_rules_444: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Le(46, 13.050476937067_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 445 btcusdt_5m_rules_445: RED
    (
        false,
        &[
            Cond::Ge(70, 96.167908771068_f64),
            Cond::Le(25, -0.001936103113_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 446 btcusdt_5m_rules_446: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.005548156292_f64),
            Cond::Le(8, -0.03657074613_f64),
            Cond::Le(46, 21.44107346_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 447 btcusdt_5m_rules_447: RED
    (
        false,
        &[
            Cond::Ge(16, 1.694065028932_f64),
            Cond::Le(24, -0.014727084111_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 448 btcusdt_5m_rules_448: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000886273409_f64),
            Cond::Le(44, -0.001638248858_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 449 btcusdt_5m_rules_449: GREEN
    (
        true,
        &[
            Cond::Le(69, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 450 btcusdt_5m_rules_450: RED
    (
        false,
        &[
            Cond::Ge(70, 94.958449012797_f64),
            Cond::Ge(72, 0.003815742964_f64),
        ],
    ),
    // 451 btcusdt_5m_rules_451: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.235606694343_f64),
            Cond::Ge(79, 0.001053177884_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 452 btcusdt_5m_rules_452: RED
    (
        false,
        &[
            Cond::Ge(70, 93.669724770642_f64),
            Cond::Le(24, -0.004226946089_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 453 btcusdt_5m_rules_453: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 454 btcusdt_5m_rules_454: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000362649771_f64),
            Cond::Ge(2, 0.0076658772_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 455 btcusdt_5m_rules_455: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.836158938595_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 456 btcusdt_5m_rules_456: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Le(1, -5.344432465957_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 457 btcusdt_5m_rules_457: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(56, 0.004280476703_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 458 btcusdt_5m_rules_458: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Eq(41, 13.0_f64),
            Cond::Le(78, 0.7579850134_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 459 btcusdt_5m_rules_459: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(60, 27.52356397_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 460 btcusdt_5m_rules_460: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(10, -0.002956606273_f64),
            Cond::Le(61, 35.7929937_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 461 btcusdt_5m_rules_461: RED
    (
        false,
        &[
            Cond::Ge(69, 89.541313522713_f64),
            Cond::Ge(0, 30.625040783455_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 462 btcusdt_5m_rules_462: GREEN
    (
        true,
        &[
            Cond::Le(62, 11.111136496641_f64),
            Cond::Le(80, -0.024921049724_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 463 btcusdt_5m_rules_463: RED
    (
        false,
        &[
            Cond::Ge(62, 78.115236912005_f64),
            Cond::Le(13, 47.198310116171_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 464 btcusdt_5m_rules_464: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.293693136323_f64),
            Cond::Between(44, -0.00032682283_f64, 0.000294242144_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 465 btcusdt_5m_rules_465: GREEN
    (
        true,
        &[
            Cond::Le(62, 23.828421748691_f64),
            Cond::Ge(70, 43.277212100048_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 466 btcusdt_5m_rules_466: RED
    (
        false,
        &[
            Cond::Ge(4, 1.242274122_f64),
            Cond::Eq(81, 3.0_f64),
            Cond::Le(78, 2.098463339_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 467 btcusdt_5m_rules_467: RED
    (
        false,
        &[
            Cond::Ge(62, 85.681824259181_f64),
            Cond::Ge(74, 0.0067308277_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 468 btcusdt_5m_rules_468: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(14, -153.295298_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 469 btcusdt_5m_rules_469: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.811743944906_f64),
            Cond::Ge(12, -47.410571675673_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 470 btcusdt_5m_rules_470: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.703514870312_f64),
            Cond::Ge(21, 0.002274079813_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 471 btcusdt_5m_rules_471: GREEN
    (
        true,
        &[
            Cond::Le(62, 14.028360392873_f64),
            Cond::Ge(79, -0.001373496039_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 472 btcusdt_5m_rules_472: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.235606694343_f64),
            Cond::Ge(79, 0.001053177884_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 473 btcusdt_5m_rules_473: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(56, -0.005712097907_f64),
            Cond::Le(61, 31.37459303_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 474 btcusdt_5m_rules_474: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 475 btcusdt_5m_rules_475: RED
    (
        false,
        &[
            Cond::Ge(10, 0.008390907843_f64),
            Cond::Ge(24, -0.0003773777571_f64),
            Cond::Le(42, 0.0_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 476 btcusdt_5m_rules_476: RED
    (
        false,
        &[
            Cond::Ge(63, 83.980782208383_f64),
            Cond::Between(27, 0.002765568341_f64, 0.004563556495_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 477 btcusdt_5m_rules_477: GREEN
    (
        true,
        &[
            Cond::Le(46, 10.339569656362_f64),
            Cond::Le(50, 0.663177938908_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 478 btcusdt_5m_rules_478: GREEN
    (
        true,
        &[
            Cond::Le(70, 10.434588873737_f64),
            Cond::Ge(79, 0.004008177104_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 479 btcusdt_5m_rules_479: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.735894374538_f64),
            Cond::Ge(46, 74.05027843037_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 480 btcusdt_5m_rules_480: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.898892702_f64),
            Cond::Le(44, -0.002344176743_f64),
            Cond::Ge(50, 1.403339542_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 481 btcusdt_5m_rules_481: GREEN
    (
        true,
        &[
            Cond::Le(63, 26.242804604165_f64),
            Cond::Ge(45, 0.001241223785_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 482 btcusdt_5m_rules_482: RED
    (
        false,
        &[
            Cond::Ge(44, 0.00186594868_f64),
            Cond::Ge(69, 95.36034773_f64),
            Cond::Ge(63, 86.81303658_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 483 btcusdt_5m_rules_483: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.65525868_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(71, 10.166951_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 484 btcusdt_5m_rules_484: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 485 btcusdt_5m_rules_485: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(56, -0.005712097907_f64),
            Cond::Le(61, 31.37459303_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 486 btcusdt_5m_rules_486: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 6.0_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 487 btcusdt_5m_rules_487: RED
    (
        false,
        &[
            Cond::Ge(16, 2.589440698101_f64),
            Cond::Le(23, -0.001943115236_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 488 btcusdt_5m_rules_488: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Ge(73, 17.001463414648_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 489 btcusdt_5m_rules_489: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(2, 0.007385036543_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 490 btcusdt_5m_rules_490: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(44, 0.002366750995_f64),
            Cond::Ge(17, 2.046146229_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 491 btcusdt_5m_rules_491: RED
    (
        false,
        &[
            Cond::Ge(16, 2.082088491926_f64),
            Cond::Le(45, -0.004456144174_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 492 btcusdt_5m_rules_492: RED
    (
        false,
        &[
            Cond::Ge(17, 3.06306070625_f64),
            Cond::Between(23, -0.000806012153_f64, 0.001120083964_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 493 btcusdt_5m_rules_493: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.005548156292_f64),
            Cond::Le(8, -0.03657074613_f64),
            Cond::Le(46, 21.44107346_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 494 btcusdt_5m_rules_494: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.016780876652_f64),
            Cond::Le(44, -0.004351170326_f64),
        ],
    ),
    // 495 btcusdt_5m_rules_495: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.00030861933_f64),
            Cond::Ge(49, 1430.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 496 btcusdt_5m_rules_496: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 497 btcusdt_5m_rules_497: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 97.6614989_f64),
            Cond::Ge(17, 2.292740233_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 498 btcusdt_5m_rules_498: RED
    (
        false,
        &[
            Cond::Ge(16, 2.589440698101_f64),
            Cond::Ge(74, 0.005051760632_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 499 btcusdt_5m_rules_499: RED
    (
        false,
        &[
            Cond::Ge(44, 0.00186594868_f64),
            Cond::Ge(69, 97.66150155_f64),
            Cond::Ge(37, 4.0_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 500 btcusdt_5m_rules_500: GREEN
    (
        true,
        &[
            Cond::Le(12, -209.954494058912_f64),
            Cond::Le(80, -0.011145677221_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 501 btcusdt_5m_rules_501: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(41, &[21.0_f64, 22.0_f64, 23.0_f64]),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 502 btcusdt_5m_rules_502: RED
    (
        false,
        &[
            Cond::Ge(16, 2.254214593777_f64),
            Cond::Ge(2, 0.010292432286_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 503 btcusdt_5m_rules_503: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(59, 0.0242546669_f64),
            Cond::Ge(8, -0.007039969776_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 504 btcusdt_5m_rules_504: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(61, 31.37459303_f64),
            Cond::Le(30, 0.0001481980796_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 505 btcusdt_5m_rules_505: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(46, 27.45320025_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 506 btcusdt_5m_rules_506: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.563149366712_f64),
            Cond::Ge(79, 0.001902549548_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 507 btcusdt_5m_rules_507: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 508 btcusdt_5m_rules_508: GREEN
    (
        true,
        &[
            Cond::Le(63, 24.393415791832_f64),
            Cond::Le(51, -1.033363918481_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 509 btcusdt_5m_rules_509: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.405699915549_f64),
            Cond::Ge(6, 0.027039498754_f64),
        ],
    ),
    // 510 btcusdt_5m_rules_510: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.492345474495_f64),
            Cond::Ge(45, 0.000938501478_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 511 btcusdt_5m_rules_511: GREEN
    (
        true,
        &[
            Cond::Le(70, 6.753473519311_f64),
            Cond::Ge(43, 5.286416861829_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 512 btcusdt_5m_rules_512: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(75, 0.007974367829_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 513 btcusdt_5m_rules_513: RED
    (
        false,
        &[
            Cond::Ge(69, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 514 btcusdt_5m_rules_514: RED
    (
        false,
        &[
            Cond::Ge(4, 1.064060482446_f64),
            Cond::Le(25, -0.015171778412_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 515 btcusdt_5m_rules_515: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(75, 0.007022934773_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 516 btcusdt_5m_rules_516: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 97.6614989_f64),
            Cond::Ge(17, 2.292740233_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 517 btcusdt_5m_rules_517: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Le(21, -0.0156322462_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 518 btcusdt_5m_rules_518: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.898892702_f64),
            Cond::Le(44, -0.002344176743_f64),
            Cond::Ge(50, 1.403339542_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 519 btcusdt_5m_rules_519: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.970296088074_f64),
            Cond::Ge(68, 1.841596579591_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 520 btcusdt_5m_rules_520: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(25, -0.016115379302_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 521 btcusdt_5m_rules_521: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.009345051371_f64),
            Cond::Le(38, -0.015034637046_f64),
        ],
    ),
    // 522 btcusdt_5m_rules_522: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(10, -0.01817602056_f64),
            Cond::Ge(78, 3.587853962_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 523 btcusdt_5m_rules_523: RED
    (
        false,
        &[
            Cond::Ge(70, 98.13255444133_f64),
            Cond::Ge(19, 2.511911306736_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 524 btcusdt_5m_rules_524: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266367569_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Le(48, 14.51065799_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 525 btcusdt_5m_rules_525: GREEN
    (
        true,
        &[Cond::Le(24, -0.103167040398_f64), Cond::Ge(66, 1.0_f64)],
    ),
    // 526 btcusdt_5m_rules_526: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(10, -0.002956606273_f64),
            Cond::Le(61, 35.7929937_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 527 btcusdt_5m_rules_527: RED
    (
        false,
        &[
            Cond::Ge(70, 97.799245309998_f64),
            Cond::Ge(22, 0.050318074037_f64),
        ],
    ),
    // 528 btcusdt_5m_rules_528: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(47, 73.95404425_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 529 btcusdt_5m_rules_529: RED
    (
        false,
        &[
            Cond::Ge(63, 85.600813068941_f64),
            Cond::Ge(42, 0.008326115895_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 530 btcusdt_5m_rules_530: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.583474519884_f64),
            Cond::Le(45, -0.005856406184_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 531 btcusdt_5m_rules_531: RED
    (
        false,
        &[
            Cond::Ge(70, 96.167908771068_f64),
            Cond::Le(25, -0.001936103113_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 532 btcusdt_5m_rules_532: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(56, 0.042504219742_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 533 btcusdt_5m_rules_533: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266398454_f64),
            Cond::Le(8, -0.01970911953_f64),
            Cond::Ge(78, 2.919374564_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 534 btcusdt_5m_rules_534: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 535 btcusdt_5m_rules_535: RED
    (
        false,
        &[
            Cond::Ge(63, 73.931490170639_f64),
            Cond::Le(51, -1.61225812853_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 536 btcusdt_5m_rules_536: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Le(51, 0.216746335689_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 537 btcusdt_5m_rules_537: RED
    (
        false,
        &[
            Cond::Ge(62, 76.287641686518_f64),
            Cond::Le(45, -0.002303401329_f64),
        ],
    ),
    // 538 btcusdt_5m_rules_538: RED
    (
        false,
        &[
            Cond::Ge(17, 2.041451492706_f64),
            Cond::Le(45, -0.001221253105_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 539 btcusdt_5m_rules_539: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000993286689_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 540 btcusdt_5m_rules_540: GREEN
    (
        true,
        &[
            Cond::Le(63, 26.242804604165_f64),
            Cond::Ge(45, 0.001241223785_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 541 btcusdt_5m_rules_541: GREEN
    (
        true,
        &[
            Cond::Le(44, -0.002650404386_f64),
            Cond::Le(69, 5.152344313_f64),
            Cond::Le(70, 3.478803314_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 542 btcusdt_5m_rules_542: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(10, 0.01753783257_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 543 btcusdt_5m_rules_543: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006709289818_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Le(78, 2.912906413_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 544 btcusdt_5m_rules_544: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(2, 0.001277921698_f64),
            Cond::Le(47, 33.22794653_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 545 btcusdt_5m_rules_545: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(73, 50.208999999976_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 546 btcusdt_5m_rules_546: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404171868_f64),
            Cond::Le(38, -0.007640254912_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 547 btcusdt_5m_rules_547: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.1832969_f64),
            Cond::Le(3, 0.0007461976822_f64),
            Cond::Ge(70, 7.498316732_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 548 btcusdt_5m_rules_548: RED
    (
        false,
        &[
            Cond::Ge(16, 2.254214593777_f64),
            Cond::Ge(2, 0.010292432286_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 549 btcusdt_5m_rules_549: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266367569_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Le(48, 14.51065799_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 550 btcusdt_5m_rules_550: RED
    (
        false,
        &[
            Cond::Ge(70, 98.04361321_f64),
            Cond::Ge(8, 0.02432418065_f64),
            Cond::Ge(37, 3.0_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 551 btcusdt_5m_rules_551: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 552 btcusdt_5m_rules_552: GREEN
    (
        true,
        &[
            Cond::Le(12, -209.954494058912_f64),
            Cond::Le(80, -0.011145677221_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 553 btcusdt_5m_rules_553: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 554 btcusdt_5m_rules_554: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.679463493_f64),
            Cond::Ge(6, 0.008140445126_f64),
            Cond::Le(45, -0.00345019142_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 555 btcusdt_5m_rules_555: GREEN
    (
        true,
        &[
            Cond::Le(62, 19.291134799525_f64),
            Cond::Le(11, -0.313680587227_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 556 btcusdt_5m_rules_556: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266398454_f64),
            Cond::Le(57, -0.03454574655_f64),
            Cond::Ge(72, 0.001054938882_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 557 btcusdt_5m_rules_557: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Ge(1, 4.325244011802_f64),
            Cond::Eq(66, 1.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 558 btcusdt_5m_rules_558: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000518047549_f64),
            Cond::Le(44, -0.003453836366_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 559 btcusdt_5m_rules_559: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000960780049_f64),
            Cond::Le(44, -0.00540043628_f64),
        ],
    ),
    // 560 btcusdt_5m_rules_560: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Ge(58, -0.003325085454_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 561 btcusdt_5m_rules_561: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Between(48, 43.719050398098_f64, 56.000133414606_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 562 btcusdt_5m_rules_562: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(25, -0.016115379302_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 563 btcusdt_5m_rules_563: GREEN
    (
        true,
        &[
            Cond::Le(22, -0.059419189352_f64),
            Cond::Le(42, 0.000663724138_f64),
        ],
    ),
    // 564 btcusdt_5m_rules_564: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(75, 0.007022934773_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 565 btcusdt_5m_rules_565: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 566 btcusdt_5m_rules_566: RED
    (
        false,
        &[
            Cond::Ge(25, -0.003294721515_f64),
            Cond::Ge(6, 0.034547716221_f64),
        ],
    ),
    // 567 btcusdt_5m_rules_567: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Le(2, 0.000647938293_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 568 btcusdt_5m_rules_568: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(47, 73.95404425_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 569 btcusdt_5m_rules_569: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(10, -0.005522189783_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 570 btcusdt_5m_rules_570: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000673519805_f64),
            Cond::Le(79, -0.011317995852_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 571 btcusdt_5m_rules_571: RED
    (
        false,
        &[
            Cond::Ge(69, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 572 btcusdt_5m_rules_572: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Le(45, -0.001221253105_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 573 btcusdt_5m_rules_573: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.492345474495_f64),
            Cond::Ge(45, 0.000938501478_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 574 btcusdt_5m_rules_574: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404171868_f64),
            Cond::Le(38, -0.007640254912_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 575 btcusdt_5m_rules_575: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001091384106_f64),
            Cond::Le(10, -0.01817603669_f64),
            Cond::Ge(78, 2.919388313_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 576 btcusdt_5m_rules_576: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0008266398454_f64),
            Cond::Le(57, -0.03454574655_f64),
            Cond::Ge(72, 0.001054938882_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 577 btcusdt_5m_rules_577: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.240426681222_f64),
            Cond::Ge(74, 0.010175590584_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 578 btcusdt_5m_rules_578: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(30, 0.03468189691_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 579 btcusdt_5m_rules_579: GREEN
    (
        true,
        &[
            Cond::Le(56, -0.03224343338_f64),
            Cond::Le(69, 3.779328959_f64),
            Cond::Le(13, -175.7548746_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 580 btcusdt_5m_rules_580: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(47, 73.95404425_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 581 btcusdt_5m_rules_581: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Le(48, 18.35512826_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 582 btcusdt_5m_rules_582: RED
    (
        false,
        &[
            Cond::Ge(44, 0.00186594868_f64),
            Cond::Ge(69, 97.66150155_f64),
            Cond::Ge(37, 4.0_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 583 btcusdt_5m_rules_583: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.407243075194_f64),
            Cond::Ge(50, 3.519022231122_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 584 btcusdt_5m_rules_584: GREEN
    (
        true,
        &[
            Cond::Le(48, 8.551406555269_f64),
            Cond::Le(0, -10.65253216544_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 585 btcusdt_5m_rules_585: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.058231615_f64),
            Cond::Ge(30, 0.02558085724_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 586 btcusdt_5m_rules_586: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(56, 0.01939351557_f64),
            Cond::Eq(81, 4.0_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 587 btcusdt_5m_rules_587: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 29.891000233355_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 588 btcusdt_5m_rules_588: RED
    (
        false,
        &[
            Cond::Ge(60, 81.688139538765_f64),
            Cond::Ge(0, 1.293775146209_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 589 btcusdt_5m_rules_589: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(30, 0.001652921292_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 590 btcusdt_5m_rules_590: GREEN
    (
        true,
        &[
            Cond::Le(62, 16.815653800848_f64),
            Cond::Le(51, -0.781972702142_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 591 btcusdt_5m_rules_591: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(10, 0.01753783257_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 592 btcusdt_5m_rules_592: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(68, 1.323958574628_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 593 btcusdt_5m_rules_593: RED
    (
        false,
        &[
            Cond::Ge(40, 0.767976820667_f64),
            Cond::Le(60, 41.054134762643_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 594 btcusdt_5m_rules_594: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(2, 0.001277921698_f64),
            Cond::Le(47, 33.22794653_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 595 btcusdt_5m_rules_595: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.022563546352_f64),
            Cond::Ge(72, 0.012910358989_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 596 btcusdt_5m_rules_596: RED
    (
        false,
        &[
            Cond::Ge(69, 99.566819537935_f64),
            Cond::Le(51, -1.087983177347_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 597 btcusdt_5m_rules_597: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(10, 0.01753783257_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 598 btcusdt_5m_rules_598: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.286381306679_f64),
            Cond::Ge(60, 37.712563201862_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 599 btcusdt_5m_rules_599: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 600 btcusdt_5m_rules_600: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(28, 0.001091384768_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 601 btcusdt_5m_rules_601: GREEN
    (
        true,
        &[
            Cond::Le(62, 18.416929648717_f64),
            Cond::Le(53, 2.0_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 602 btcusdt_5m_rules_602: GREEN
    (
        true,
        &[
            Cond::Le(70, 4.960424772543_f64),
            Cond::Ge(1, 23.119074919152_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 603 btcusdt_5m_rules_603: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(10, -0.01817602056_f64),
            Cond::Ge(78, 3.587853962_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 604 btcusdt_5m_rules_604: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(56, -0.005712097907_f64),
            Cond::Le(61, 31.37459303_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 605 btcusdt_5m_rules_605: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.127057896226_f64),
            Cond::Le(1, -1.893164059008_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 606 btcusdt_5m_rules_606: GREEN
    (
        true,
        &[
            Cond::Le(70, 5.132606156_f64),
            Cond::Ge(59, 0.02599541236_f64),
            Cond::Le(62, 24.46140344_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 607 btcusdt_5m_rules_607: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(4, -0.2340435963_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 608 btcusdt_5m_rules_608: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 609 btcusdt_5m_rules_609: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(6, 0.007864217465_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 610 btcusdt_5m_rules_610: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(69, 97.6614989_f64),
            Cond::Ge(17, 2.292740233_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 611 btcusdt_5m_rules_611: GREEN
    (
        true,
        &[
            Cond::Le(44, -0.002650404386_f64),
            Cond::Le(69, 5.152344313_f64),
            Cond::Le(70, 3.478803314_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 612 btcusdt_5m_rules_612: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(56, -0.005712097907_f64),
            Cond::Le(61, 31.37459303_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 613 btcusdt_5m_rules_613: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(14, -153.295298_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 614 btcusdt_5m_rules_614: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.997196098696_f64),
            Cond::Ge(75, 0.032227797354_f64),
        ],
    ),
    // 615 btcusdt_5m_rules_615: GREEN
    (
        true,
        &[
            Cond::Le(56, -0.03224343338_f64),
            Cond::Le(69, 3.779328959_f64),
            Cond::Le(13, -175.7548746_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 616 btcusdt_5m_rules_616: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(75, 0.007022934773_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 617 btcusdt_5m_rules_617: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Ge(44, 0.000140769183_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 618 btcusdt_5m_rules_618: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(70, 4.53517561_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 619 btcusdt_5m_rules_619: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(60, 27.52356397_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 620 btcusdt_5m_rules_620: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 621 btcusdt_5m_rules_621: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001091384106_f64),
            Cond::Le(10, -0.01817603669_f64),
            Cond::Ge(78, 2.919388313_f64),
            Cond::Eq(81, 3.0_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 622 btcusdt_5m_rules_622: GREEN
    (
        true,
        &[
            Cond::Le(62, 16.441111837773_f64),
            Cond::Le(77, -0.478087109912_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 623 btcusdt_5m_rules_623: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(2, 0.001277921698_f64),
            Cond::Le(47, 33.22794653_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 624 btcusdt_5m_rules_624: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(15, 0.029402232243_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 625 btcusdt_5m_rules_625: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001091384106_f64),
            Cond::Le(10, -0.01817603669_f64),
            Cond::Ge(78, 2.919388313_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 626 btcusdt_5m_rules_626: RED
    (
        false,
        &[
            Cond::Ge(69, 95.36043284_f64),
            Cond::Ge(10, 0.01753783257_f64),
            Cond::Le(42, 0.001638794532_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
];

pub struct BtcRules626 {
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

impl BtcRules626 {
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

impl Strategy for BtcRules626 {
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
