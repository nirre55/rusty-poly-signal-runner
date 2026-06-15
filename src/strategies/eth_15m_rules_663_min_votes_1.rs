use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "eth_15m_rules_663_min_votes_1";
const FEATURE_COUNT: usize = 85;

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
// 33=failed_low24
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
// 59=ret6
// 60=ret72
// 61=rsi14
// 62=rsi21
// 63=rsi7
// 64=rsi8
// 65=same_color_ratio12
// 66=session_asia
// 67=session_london
// 68=session_overlap_london_us
// 69=session_us
// 70=signed_volume_ratio20
// 71=stoch_k12
// 72=stoch_k24
// 73=stoch_k72
// 74=upper_wick
// 75=upper_wick_body
// 76=volume_body_efficiency
// 77=volume_range_efficiency
// 78=volume_ratio20
// 79=volume_z24
// 80=volume_z96
// 81=vwap_slope24
// 82=vwap_slope72
// 83=weekday
// 84=williams_r12
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
    f[33] = failed_low(buf, 24);
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
    f[59] = ret_n(buf, 6);
    f[60] = ret_n(buf, 72);
    f[61] = rsi14.get();
    f[62] = rsi21.get();
    f[63] = rsi7.get();
    f[64] = rsi8.get();
    f[65] = same_color_ratio(buf, 12);
    f[66] = Some(session_asia(minute_of_day));
    f[67] = Some(session_london(minute_of_day));
    f[68] = Some(session_overlap_london_us(minute_of_day));
    f[69] = Some(session_us(minute_of_day));
    f[70] = signed_vol_ratio(buf, 20);
    f[71] = stoch_k(buf, 12, close);
    f[72] = stoch_k(buf, 24, close);
    f[73] = stoch_k(buf, 72, close);
    f[74] = upper_wick;
    f[75] = upper_wick_body;
    f[76] = vol_body_eff(buf);
    f[77] = vol_range_eff(buf);
    f[78] = volume_ratio(buf, 20);
    f[79] = vol_z(buf, 24);
    f[80] = vol_z(buf, 96);
    f[81] = vwap_slope(buf, 24);
    f[82] = vwap_slope(buf, 72);
    f[83] = Some(weekday);
    f[84] = williams_r(buf, 12);
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
    // 1 ethusdt_15m_rules_1: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0015569182_f64),
            Cond::Le(62, 30.19044475_f64),
            Cond::Ge(44, -0.00105807534_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 2 ethusdt_15m_rules_2: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.722439334531_f64),
            Cond::Le(2, 0.003127045792_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 3 ethusdt_15m_rules_3: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.60338060349_f64),
            Cond::In(41, &[8.0_f64]),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 4 ethusdt_15m_rules_4: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(63, 40.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 5 ethusdt_15m_rules_5: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Ge(13, -43.945068697837_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 6 ethusdt_15m_rules_6: GREEN
    (
        true,
        &[
            Cond::Le(61, 34.187315401094_f64),
            Cond::Le(19, 0.519485662844_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 7 ethusdt_15m_rules_7: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.062588143616_f64),
            Cond::In(41, &[6.0_f64]),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 8 ethusdt_15m_rules_8: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Le(6, 0.000139017832_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 9 ethusdt_15m_rules_9: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 6.0_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 10 ethusdt_15m_rules_10: GREEN
    (
        true,
        &[
            Cond::Le(63, 29.776688316978_f64),
            Cond::Ge(82, 0.027527019994_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 11 ethusdt_15m_rules_11: RED
    (
        false,
        &[
            Cond::Ge(48, 92.869319882182_f64),
            Cond::In(41, &[8.0_f64]),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 12 ethusdt_15m_rules_12: RED
    (
        false,
        &[
            Cond::Ge(72, 97.799245309998_f64),
            Cond::Ge(2, 0.015565536959_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 13 ethusdt_15m_rules_13: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.002929379759_f64),
            Cond::Ge(74, 0.005230358453_f64),
            Cond::Ge(50, 1.386145597_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 14 ethusdt_15m_rules_14: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Ge(58, -0.004437668774_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 15 ethusdt_15m_rules_15: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.714414358796_f64),
            Cond::Between(12, -20.481234551541_f64, 24.904802782132_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 16 ethusdt_15m_rules_16: RED
    (
        false,
        &[
            Cond::Ge(46, 82.208371283464_f64),
            Cond::Le(40, 0.335678247138_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 17 ethusdt_15m_rules_17: GREEN
    (
        true,
        &[
            Cond::Le(72, 9.711755951452_f64),
            Cond::Le(78, 0.357082553591_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 18 ethusdt_15m_rules_18: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000053521742_f64),
            Cond::Ge(75, 0.056497175141_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 19 ethusdt_15m_rules_19: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00135922631_f64),
            Cond::Ge(45, 0.013873233781_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 20 ethusdt_15m_rules_20: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.371343923784_f64),
            Cond::Ge(58, 0.00710213073_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 21 ethusdt_15m_rules_21: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 22 ethusdt_15m_rules_22: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.495792277202_f64),
            Cond::Ge(23, 0.013943331868_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 23 ethusdt_15m_rules_23: GREEN
    (
        true,
        &[
            Cond::Le(48, 13.804718914397_f64),
            Cond::Ge(56, 0.009362722287_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 24 ethusdt_15m_rules_24: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.547097817_f64),
            Cond::Le(2, 0.001795740443_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 25 ethusdt_15m_rules_25: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(3, 0.002457878466_f64),
            Cond::Le(71, 12.12790869_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 26 ethusdt_15m_rules_26: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.371097431342_f64),
            Cond::Ge(82, 0.010852199486_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 27 ethusdt_15m_rules_27: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.210641246093_f64),
            Cond::Ge(23, 0.007795884974_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 28 ethusdt_15m_rules_28: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 29 ethusdt_15m_rules_29: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.009495690125_f64),
            Cond::Le(2, 0.003127045792_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 30 ethusdt_15m_rules_30: RED
    (
        false,
        &[
            Cond::Ge(17, 1.812243215967_f64),
            Cond::Le(0, -18.301546716969_f64),
        ],
    ),
    // 31 ethusdt_15m_rules_31: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(43, 0.01349188119_f64),
            Cond::Le(74, 0.00008848352749_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 32 ethusdt_15m_rules_32: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::Le(82, -0.008748313451_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 33 ethusdt_15m_rules_33: GREEN
    (
        true,
        &[
            Cond::Le(46, 15.154587110767_f64),
            Cond::Le(2, 0.001121922468_f64),
        ],
    ),
    // 34 ethusdt_15m_rules_34: RED
    (
        false,
        &[
            Cond::Ge(63, 66.597829192963_f64),
            Cond::Le(46, 30.058809486348_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 35 ethusdt_15m_rules_35: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.002650174002_f64),
            Cond::Ge(50, 3.036678151198_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 36 ethusdt_15m_rules_36: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(42, 0.0001510480269_f64),
            Cond::Ge(14, -111.0492288_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 37 ethusdt_15m_rules_37: GREEN
    (
        true,
        &[
            Cond::Le(71, 8.594155904669_f64),
            Cond::Le(76, 0.000068985592_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 38 ethusdt_15m_rules_38: GREEN
    (
        true,
        &[
            Cond::Le(72, 13.103119286977_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 39 ethusdt_15m_rules_39: RED
    (
        false,
        &[
            Cond::Ge(4, 1.02132756984_f64),
            Cond::Between(58, -0.001576545149_f64, 0.001867688078_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 40 ethusdt_15m_rules_40: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000813794422_f64),
            Cond::Le(45, -0.007007852866_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 41 ethusdt_15m_rules_41: RED
    (
        false,
        &[
            Cond::Ge(48, 88.271737232917_f64),
            Cond::Le(29, 0.007117316977_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 42 ethusdt_15m_rules_42: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.921859236625_f64),
            Cond::Le(2, 0.001519053115_f64),
        ],
    ),
    // 43 ethusdt_15m_rules_43: RED
    (
        false,
        &[
            Cond::Ge(24, -0.001005628718_f64),
            Cond::Le(76, 0.000147895196_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 44 ethusdt_15m_rules_44: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(39, 0.255739517915_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 45 ethusdt_15m_rules_45: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.051658096366_f64),
            Cond::Ge(44, 0.000552259724_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 46 ethusdt_15m_rules_46: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.002929379759_f64),
            Cond::Ge(74, 0.005230358453_f64),
            Cond::Ge(50, 1.386145597_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 47 ethusdt_15m_rules_47: RED
    (
        false,
        &[
            Cond::Ge(72, 89.597440746859_f64),
            Cond::Ge(74, 0.005420124579_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 48 ethusdt_15m_rules_48: GREEN
    (
        true,
        &[
            Cond::Le(63, 11.111136496641_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 49 ethusdt_15m_rules_49: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.586231489434_f64),
            Cond::Le(0, -7.107013931768_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 50 ethusdt_15m_rules_50: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.069277361163_f64),
            Cond::Between(46, 44.867293806931_f64, 55.028092949405_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 51 ethusdt_15m_rules_51: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00121384214_f64),
            Cond::Le(15, 0.408114754734_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 52 ethusdt_15m_rules_52: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 53 ethusdt_15m_rules_53: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.004437668774_f64),
            Cond::Ge(61, 74.897385339306_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 54 ethusdt_15m_rules_54: RED
    (
        false,
        &[
            Cond::Ge(16, 2.483924092893_f64),
            Cond::Ge(82, 0.017955482572_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 55 ethusdt_15m_rules_55: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.00471951852_f64),
            Cond::Le(77, 0.001096723147_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 56 ethusdt_15m_rules_56: GREEN
    (
        true,
        &[
            Cond::Le(71, 0.5066458518_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Ge(42, 9.29093662300000e-8_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 57 ethusdt_15m_rules_57: GREEN
    (
        true,
        &[
            Cond::Le(61, 27.69290095_f64),
            Cond::Le(72, 3.478803314_f64),
            Cond::Le(54, 3.0_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 58 ethusdt_15m_rules_58: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.089213320949_f64),
            Cond::Ge(32, 1.0_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 59 ethusdt_15m_rules_59: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000886273409_f64),
            Cond::In(41, &[12.0_f64]),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 60 ethusdt_15m_rules_60: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(12, -191.4788279_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 61 ethusdt_15m_rules_61: GREEN
    (
        true,
        &[
            Cond::Le(63, 11.111136496641_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 62 ethusdt_15m_rules_62: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(71, 95.36043284_f64),
            Cond::Le(72, 96.70846245_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 63 ethusdt_15m_rules_63: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.722439334531_f64),
            Cond::Ge(81, -0.004763212286_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 64 ethusdt_15m_rules_64: RED
    (
        false,
        &[
            Cond::Ge(64, 73.82299439_f64),
            Cond::Le(60, -0.01475596055_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 65 ethusdt_15m_rules_65: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Between(63, 46.363173508285_f64, 54.618381008027_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 66 ethusdt_15m_rules_66: RED
    (
        false,
        &[
            Cond::Ge(71, 94.73247534402_f64),
            Cond::Le(2, 0.0031654888_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 67 ethusdt_15m_rules_67: RED
    (
        false,
        &[
            Cond::Ge(71, 98.621001727441_f64),
            Cond::Le(15, 0.926451224707_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 68 ethusdt_15m_rules_68: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(24, -0.007401969759_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 69 ethusdt_15m_rules_69: RED
    (
        false,
        &[
            Cond::Ge(72, 92.847408686743_f64),
            Cond::Le(39, 0.017282150796_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 70 ethusdt_15m_rules_70: RED
    (
        false,
        &[
            Cond::Ge(40, 0.767976820667_f64),
            Cond::Le(8, -0.013022960192_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 71 ethusdt_15m_rules_71: RED
    (
        false,
        &[
            Cond::Ge(72, 87.431021042805_f64),
            Cond::Ge(74, 0.008462058792_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 72 ethusdt_15m_rules_72: RED
    (
        false,
        &[
            Cond::Ge(16, 1.714252214364_f64),
            Cond::Le(51, -1.129440954302_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 73 ethusdt_15m_rules_73: GREEN
    (
        true,
        &[
            Cond::Le(12, -196.455697922622_f64),
            Cond::In(41, &[22.0_f64]),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 74 ethusdt_15m_rules_74: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 39.31642972534_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 75 ethusdt_15m_rules_75: RED
    (
        false,
        &[
            Cond::Ge(63, 76.287641686518_f64),
            Cond::Le(79, -0.939474731553_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 76 ethusdt_15m_rules_76: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.417455072988_f64),
            Cond::Le(19, 0.693883935668_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 77 ethusdt_15m_rules_77: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(2, 0.00260692919_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 78 ethusdt_15m_rules_78: RED
    (
        false,
        &[
            Cond::Ge(17, 2.287561321199_f64),
            Cond::Le(45, -0.000898726231_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 79 ethusdt_15m_rules_79: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.289885513984_f64),
            Cond::Ge(34, 6.0_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 80 ethusdt_15m_rules_80: RED
    (
        false,
        &[
            Cond::Ge(71, 95.010861843374_f64),
            Cond::Le(46, 30.621116950057_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 81 ethusdt_15m_rules_81: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.039352357177_f64),
            Cond::Ge(44, -0.002122380767_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 82 ethusdt_15m_rules_82: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001668628847_f64),
            Cond::Ge(82, 0.017955482572_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 83 ethusdt_15m_rules_83: RED
    (
        false,
        &[
            Cond::Ge(40, 0.735382052348_f64),
            Cond::Le(46, 24.709740053435_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 84 ethusdt_15m_rules_84: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.405699915549_f64),
            Cond::Ge(6, 0.027039498754_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 85 ethusdt_15m_rules_85: GREEN
    (
        true,
        &[
            Cond::Le(35, 0.0_f64),
            Cond::Ge(20, 0.010119255562_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 86 ethusdt_15m_rules_86: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(79, -1.159346030872_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 87 ethusdt_15m_rules_87: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000409020996_f64),
            Cond::Le(39, 0.159137292685_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 88 ethusdt_15m_rules_88: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.005348262374_f64),
            Cond::Ge(61, 69.060618393279_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 89 ethusdt_15m_rules_89: RED
    (
        false,
        &[
            Cond::Ge(71, 99.566819537935_f64),
            Cond::Le(82, -0.012633865095_f64),
        ],
    ),
    // 90 ethusdt_15m_rules_90: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(57, -0.01422341075_f64),
            Cond::Le(80, 0.678389517_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 91 ethusdt_15m_rules_91: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 92 ethusdt_15m_rules_92: GREEN
    (
        true,
        &[
            Cond::Le(71, 12.958361968728_f64),
            Cond::Le(79, -0.99874379869_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 93 ethusdt_15m_rules_93: RED
    (
        false,
        &[
            Cond::Ge(46, 87.326571431094_f64),
            Cond::Between(23, -0.003483247059_f64, 0.005497807331_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 94 ethusdt_15m_rules_94: RED
    (
        false,
        &[
            Cond::Ge(17, 2.359838557827_f64),
            Cond::Le(20, 0.00203958547_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 95 ethusdt_15m_rules_95: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(24, -0.01631766978_f64),
            Cond::Ge(7, 0.761334494_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 96 ethusdt_15m_rules_96: RED
    (
        false,
        &[
            Cond::Ge(72, 98.632249177409_f64),
            Cond::Ge(0, 0.941851345832_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 97 ethusdt_15m_rules_97: GREEN
    (
        true,
        &[
            Cond::Le(72, 5.77000366238_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 98 ethusdt_15m_rules_98: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.248491103125_f64),
            Cond::Le(19, 0.442037442949_f64),
        ],
    ),
    // 99 ethusdt_15m_rules_99: RED
    (
        false,
        &[
            Cond::Ge(25, -0.001841392139_f64),
            Cond::Le(76, 0.000132092228_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 100 ethusdt_15m_rules_100: RED
    (
        false,
        &[
            Cond::Ge(63, 78.115236912005_f64),
            Cond::Le(27, 0.011447152483_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 101 ethusdt_15m_rules_101: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(71, 95.36043284_f64),
            Cond::Le(72, 96.70846245_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 102 ethusdt_15m_rules_102: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.065876434269_f64),
            Cond::Le(40, 0.216230850514_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 103 ethusdt_15m_rules_103: RED
    (
        false,
        &[
            Cond::Ge(40, 0.767976820667_f64),
            Cond::Le(20, -0.000786922954_f64),
        ],
    ),
    // 104 ethusdt_15m_rules_104: GREEN
    (
        true,
        &[
            Cond::Le(47, 20.65922058_f64),
            Cond::Eq(83, 2.0_f64),
            Cond::Le(63, 33.60114385_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 105 ethusdt_15m_rules_105: GREEN
    (
        true,
        &[
            Cond::Le(71, 6.452676568325_f64),
            Cond::In(41, &[21.0_f64]),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 106 ethusdt_15m_rules_106: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Le(50, 0.692354618129_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 107 ethusdt_15m_rules_107: RED
    (
        false,
        &[
            Cond::Ge(71, 98.683285404721_f64),
            Cond::Ge(37, 6.0_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 108 ethusdt_15m_rules_108: RED
    (
        false,
        &[
            Cond::Ge(72, 96.0335550175_f64),
            Cond::Le(0, -1.167889331231_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 109 ethusdt_15m_rules_109: GREEN
    (
        true,
        &[
            Cond::Ge(78, 4.86378068509_f64),
            Cond::In(83, &[0.0_f64]),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 110 ethusdt_15m_rules_110: GREEN
    (
        true,
        &[
            Cond::Le(73, 8.202131158_f64),
            Cond::Eq(41, 12.0_f64),
            Cond::Le(62, 32.08032065_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 111 ethusdt_15m_rules_111: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0015569182_f64),
            Cond::Le(62, 30.19044475_f64),
            Cond::Ge(44, -0.00105807534_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 112 ethusdt_15m_rules_112: RED
    (
        false,
        &[
            Cond::Ge(61, 79.132602591729_f64),
            Cond::Le(11, -0.134109392588_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 113 ethusdt_15m_rules_113: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.056247011082_f64),
            Cond::Le(11, -0.479441712554_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 114 ethusdt_15m_rules_114: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000161605691_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 115 ethusdt_15m_rules_115: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.00237146059_f64),
            Cond::Ge(61, 68.072657708997_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 116 ethusdt_15m_rules_116: GREEN
    (
        true,
        &[
            Cond::Le(61, 34.187315401094_f64),
            Cond::Le(2, 0.000913763684_f64),
        ],
    ),
    // 117 ethusdt_15m_rules_117: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(81, 0.014461038157_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 118 ethusdt_15m_rules_118: GREEN
    (
        true,
        &[
            Cond::Le(46, 15.30340668771_f64),
            Cond::Ge(1, 25.855242512076_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 119 ethusdt_15m_rules_119: RED
    (
        false,
        &[
            Cond::Ge(40, 0.734509067946_f64),
            Cond::Le(25, -0.037167657112_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 120 ethusdt_15m_rules_120: RED
    (
        false,
        &[
            Cond::Ge(4, 1.02132756984_f64),
            Cond::Between(58, -0.001576545149_f64, 0.001867688078_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 121 ethusdt_15m_rules_121: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.005185093586_f64),
            Cond::Ge(12, 178.260877568223_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 122 ethusdt_15m_rules_122: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00121384214_f64),
            Cond::Le(15, 0.408114754734_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 123 ethusdt_15m_rules_123: RED
    (
        false,
        &[
            Cond::Ge(72, 94.958449012797_f64),
            Cond::Ge(74, 0.003815742964_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 124 ethusdt_15m_rules_124: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(11, -0.134109392588_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 125 ethusdt_15m_rules_125: RED
    (
        false,
        &[
            Cond::Ge(72, 87.431021042805_f64),
            Cond::Ge(74, 0.008462058792_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 126 ethusdt_15m_rules_126: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.417455072988_f64),
            Cond::Ge(0, 0.228653572966_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 127 ethusdt_15m_rules_127: GREEN
    (
        true,
        &[
            Cond::Le(48, 9.919091826186_f64),
            Cond::Ge(8, 0.003864538504_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 128 ethusdt_15m_rules_128: GREEN
    (
        true,
        &[
            Cond::Le(73, 6.338975885_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Le(28, 0.008900804981_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 129 ethusdt_15m_rules_129: GREEN
    (
        true,
        &[
            Cond::Le(72, 1.724176425078_f64),
            Cond::Le(46, 16.087074533329_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 130 ethusdt_15m_rules_130: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.025827454359_f64),
            Cond::Ge(19, 1.995743208991_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 131 ethusdt_15m_rules_131: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Between(50, 0.754270965462_f64, 0.952041479441_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 132 ethusdt_15m_rules_132: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.003883345889_f64),
            Cond::Ge(1, 13.438071751687_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 133 ethusdt_15m_rules_133: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Ge(51, 2.14921727869_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 134 ethusdt_15m_rules_134: RED
    (
        false,
        &[
            Cond::Ge(72, 97.799245309998_f64),
            Cond::In(41, &[6.0_f64]),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 135 ethusdt_15m_rules_135: RED
    (
        false,
        &[
            Cond::Ge(71, 98.026144238666_f64),
            Cond::Le(58, 0.002986725743_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 136 ethusdt_15m_rules_136: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.002900258506_f64),
            Cond::Ge(81, 0.004380073053_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 137 ethusdt_15m_rules_137: RED
    (
        false,
        &[
            Cond::Ge(10, 0.008390907843_f64),
            Cond::Ge(24, -0.0003773777571_f64),
            Cond::Le(42, 0.0_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 138 ethusdt_15m_rules_138: RED
    (
        false,
        &[
            Cond::Le(52, 0.0_f64),
            Cond::Le(37, 2.0_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 139 ethusdt_15m_rules_139: GREEN
    (
        true,
        &[
            Cond::Le(71, 3.592157413_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 140 ethusdt_15m_rules_140: GREEN
    (
        true,
        &[
            Cond::Le(47, 20.65922058_f64),
            Cond::Eq(83, 2.0_f64),
            Cond::Le(63, 33.60114385_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 141 ethusdt_15m_rules_141: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.289885513984_f64),
            Cond::Le(19, 0.442037442949_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 142 ethusdt_15m_rules_142: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.722439334531_f64),
            Cond::Ge(23, -0.012496232218_f64),
        ],
    ),
    // 143 ethusdt_15m_rules_143: RED
    (
        false,
        &[
            Cond::Ge(48, 92.869319882182_f64),
            Cond::Le(20, 0.004404610817_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 144 ethusdt_15m_rules_144: RED
    (
        false,
        &[
            Cond::Ge(48, 92.538838851052_f64),
            Cond::In(41, &[13.0_f64]),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 145 ethusdt_15m_rules_145: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000886273409_f64),
            Cond::Le(44, -0.001638248858_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 146 ethusdt_15m_rules_146: RED
    (
        false,
        &[
            Cond::Ge(63, 76.287641686518_f64),
            Cond::Le(79, -0.939474731553_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 147 ethusdt_15m_rules_147: RED
    (
        false,
        &[
            Cond::Ge(71, 97.75223967825_f64),
            Cond::Le(44, -0.000433596303_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 148 ethusdt_15m_rules_148: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.325581395_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Ge(84, -99.31370042_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 149 ethusdt_15m_rules_149: RED
    (
        false,
        &[
            Cond::Ge(16, 2.483924092893_f64),
            Cond::Ge(82, 0.017955482572_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 150 ethusdt_15m_rules_150: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.009495690125_f64),
            Cond::Le(2, 0.003127045792_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 151 ethusdt_15m_rules_151: RED
    (
        false,
        &[
            Cond::Ge(61, 82.623793495996_f64),
            Cond::Between(2, 0.006117730378_f64, 0.007861395294_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 152 ethusdt_15m_rules_152: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000896459525_f64),
            Cond::Le(78, 0.413342496682_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 153 ethusdt_15m_rules_153: RED
    (
        false,
        &[
            Cond::Ge(72, 89.373499384241_f64),
            Cond::Le(15, 0.089213320949_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 154 ethusdt_15m_rules_154: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.009345051371_f64),
            Cond::Le(38, -0.015034637046_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 155 ethusdt_15m_rules_155: RED
    (
        false,
        &[
            Cond::Ge(72, 96.395460340985_f64),
            Cond::Le(76, 0.000172338933_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 156 ethusdt_15m_rules_156: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.282482101409_f64),
            Cond::In(41, &[0.0_f64]),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 157 ethusdt_15m_rules_157: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.005311716219_f64),
            Cond::Ge(0, 28.212125507464_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 158 ethusdt_15m_rules_158: RED
    (
        false,
        &[
            Cond::Ge(48, 88.952653717272_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 159 ethusdt_15m_rules_159: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000161605691_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 160 ethusdt_15m_rules_160: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Ge(33, 1.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 161 ethusdt_15m_rules_161: GREEN
    (
        true,
        &[
            Cond::Le(72, 7.366825932134_f64),
            Cond::Ge(40, 0.555858847899_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 162 ethusdt_15m_rules_162: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(65, 0.833333333333_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 163 ethusdt_15m_rules_163: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.60338060349_f64),
            Cond::Ge(21, -0.00660351419_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 164 ethusdt_15m_rules_164: GREEN
    (
        true,
        &[
            Cond::Le(71, 4.845802158605_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 165 ethusdt_15m_rules_165: GREEN
    (
        true,
        &[
            Cond::Le(72, 15.545991535082_f64),
            Cond::Ge(13, -21.140333967991_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 166 ethusdt_15m_rules_166: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.069277361163_f64),
            Cond::Between(46, 44.867293806931_f64, 55.028092949405_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 167 ethusdt_15m_rules_167: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(57, -0.01422341075_f64),
            Cond::Le(80, 0.678389517_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 168 ethusdt_15m_rules_168: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Le(77, 0.000714281391_f64),
        ],
    ),
    // 169 ethusdt_15m_rules_169: GREEN
    (
        true,
        &[
            Cond::Le(46, 12.370381786246_f64),
            Cond::Ge(44, -0.00093425444_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 170 ethusdt_15m_rules_170: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 171 ethusdt_15m_rules_171: GREEN
    (
        true,
        &[
            Cond::Le(48, 8.24623315708_f64),
            Cond::Le(82, -0.023171858787_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 172 ethusdt_15m_rules_172: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.062588143616_f64),
            Cond::In(41, &[6.0_f64]),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 173 ethusdt_15m_rules_173: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00135922631_f64),
            Cond::Le(36, 1.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 174 ethusdt_15m_rules_174: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.417455072988_f64),
            Cond::Between(51, -0.614786474045_f64, 0.207539773281_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 175 ethusdt_15m_rules_175: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::Le(11, -0.327777065765_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 176 ethusdt_15m_rules_176: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(51, 1.413382543016_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 177 ethusdt_15m_rules_177: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Ge(51, 2.14921727869_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 178 ethusdt_15m_rules_178: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::In(83, &[3.0_f64]),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 179 ethusdt_15m_rules_179: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.469218562694_f64),
            Cond::Ge(44, 0.000140769183_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 180 ethusdt_15m_rules_180: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::Between(27, 0.008375974151_f64, 0.013233652106_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 181 ethusdt_15m_rules_181: GREEN
    (
        true,
        &[
            Cond::Le(9, -0.007111471533_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 182 ethusdt_15m_rules_182: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(51, -1.422984964535_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 183 ethusdt_15m_rules_183: RED
    (
        false,
        &[
            Cond::Ge(40, 0.644678507722_f64),
            Cond::Ge(6, 0.010655328013_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 184 ethusdt_15m_rules_184: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::In(41, &[17.0_f64]),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 185 ethusdt_15m_rules_185: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.027370425915_f64),
            Cond::Ge(49, 1320.0_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 186 ethusdt_15m_rules_186: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.0088103787_f64),
            Cond::Le(1, -1.819858189307_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 187 ethusdt_15m_rules_187: GREEN
    (
        true,
        &[
            Cond::Le(64, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 188 ethusdt_15m_rules_188: RED
    (
        false,
        &[
            Cond::Ge(48, 92.869319882182_f64),
            Cond::Le(20, 0.004404610817_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 189 ethusdt_15m_rules_189: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.335678247138_f64),
            Cond::Le(51, -1.536658910705_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 190 ethusdt_15m_rules_190: RED
    (
        false,
        &[
            Cond::Ge(72, 89.597440746859_f64),
            Cond::Ge(74, 0.005420124579_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 191 ethusdt_15m_rules_191: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.008267982551_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 192 ethusdt_15m_rules_192: RED
    (
        false,
        &[
            Cond::Ge(27, 0.113311886667_f64),
            Cond::Le(50, 0.545495059332_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 193 ethusdt_15m_rules_193: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(77, 0.005915796217_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 194 ethusdt_15m_rules_194: GREEN
    (
        true,
        &[
            Cond::Le(63, 37.285529400902_f64),
            Cond::Ge(82, 0.021524632953_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 195 ethusdt_15m_rules_195: GREEN
    (
        true,
        &[
            Cond::Le(48, 17.929017212354_f64),
            Cond::Ge(46, 69.85818583021_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 196 ethusdt_15m_rules_196: RED
    (
        false,
        &[
            Cond::Ge(64, 70.589612029705_f64),
            Cond::Le(46, 40.166548899026_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 197 ethusdt_15m_rules_197: RED
    (
        false,
        &[
            Cond::Ge(72, 89.373499384241_f64),
            Cond::Le(61, 53.275547235097_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 198 ethusdt_15m_rules_198: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.062588143616_f64),
            Cond::In(41, &[6.0_f64]),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 199 ethusdt_15m_rules_199: RED
    (
        false,
        &[
            Cond::Ge(71, 90.051572964609_f64),
            Cond::Le(5, -0.002629444341_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 200 ethusdt_15m_rules_200: RED
    (
        false,
        &[
            Cond::Ge(4, 1.011928971326_f64),
            Cond::Le(51, -1.203389651906_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 201 ethusdt_15m_rules_201: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Between(56, -0.003610257481_f64, 0.004009867371_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 202 ethusdt_15m_rules_202: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::In(83, &[3.0_f64]),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 203 ethusdt_15m_rules_203: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(76, 0.007814538794_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 204 ethusdt_15m_rules_204: GREEN
    (
        true,
        &[
            Cond::Le(72, 7.980198437_f64),
            Cond::Le(45, -0.01229544773_f64),
            Cond::Le(46, 18.18947095_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 205 ethusdt_15m_rules_205: RED
    (
        false,
        &[
            Cond::Ge(63, 76.287641686518_f64),
            Cond::Le(79, -0.939474731553_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 206 ethusdt_15m_rules_206: RED
    (
        false,
        &[
            Cond::Ge(63, 67.899626426361_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 207 ethusdt_15m_rules_207: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(57, -0.01422341075_f64),
            Cond::Le(80, 0.678389517_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 208 ethusdt_15m_rules_208: GREEN
    (
        true,
        &[
            Cond::Le(63, 37.09030925003_f64),
            Cond::Ge(81, 0.014139086641_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 209 ethusdt_15m_rules_209: RED
    (
        false,
        &[
            Cond::Ge(72, 93.669724770642_f64),
            Cond::Le(24, -0.004226946089_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 210 ethusdt_15m_rules_210: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.342144035663_f64),
            Cond::Ge(8, 0.006843139792_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 211 ethusdt_15m_rules_211: GREEN
    (
        true,
        &[
            Cond::Le(48, 10.831501560562_f64),
            Cond::Ge(27, 0.032597623032_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 212 ethusdt_15m_rules_212: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(79, -1.341363982825_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 213 ethusdt_15m_rules_213: RED
    (
        false,
        &[
            Cond::Ge(16, 2.264771657561_f64),
            Cond::Ge(76, 0.01327194434_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 214 ethusdt_15m_rules_214: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(82, -0.023171858787_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 215 ethusdt_15m_rules_215: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(77, 0.007022934773_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 216 ethusdt_15m_rules_216: GREEN
    (
        true,
        &[
            Cond::Le(71, 9.266584745766_f64),
            Cond::Le(2, 0.001611324521_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 217 ethusdt_15m_rules_217: GREEN
    (
        true,
        &[
            Cond::Le(5, 0.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(43, 2.0_f64),
            Cond::Ge(78, 1.0_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 218 ethusdt_15m_rules_218: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.00471951852_f64),
            Cond::Le(77, 0.001096723147_f64),
        ],
    ),
    // 219 ethusdt_15m_rules_219: RED
    (
        false,
        &[
            Cond::Ge(72, 83.360348764515_f64),
            Cond::Le(79, -1.509021822217_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 220 ethusdt_15m_rules_220: GREEN
    (
        true,
        &[
            Cond::Le(64, 37.670808563448_f64),
            Cond::Ge(17, -0.382943541103_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 221 ethusdt_15m_rules_221: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.767007525744_f64),
            Cond::Ge(49, 1425.0_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 222 ethusdt_15m_rules_222: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000717425385_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 223 ethusdt_15m_rules_223: RED
    (
        false,
        &[
            Cond::Ge(17, 2.475880255608_f64),
            Cond::Le(82, -0.008414798179_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 224 ethusdt_15m_rules_224: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.829683244288_f64),
            Cond::Ge(46, 44.954532800507_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 225 ethusdt_15m_rules_225: GREEN
    (
        true,
        &[
            Cond::Le(71, 13.57158226389_f64),
            Cond::Le(2, 0.001924913661_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 226 ethusdt_15m_rules_226: GREEN
    (
        true,
        &[
            Cond::Le(51, -1.536658910705_f64),
            Cond::Le(78, 0.226931525872_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 227 ethusdt_15m_rules_227: RED
    (
        false,
        &[
            Cond::Ge(40, 0.80253046606_f64),
            Cond::Le(79, -0.845591632774_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 228 ethusdt_15m_rules_228: RED
    (
        false,
        &[
            Cond::Ge(48, 92.538838851052_f64),
            Cond::In(41, &[12.0_f64]),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 229 ethusdt_15m_rules_229: RED
    (
        false,
        &[
            Cond::Ge(40, 0.712764986117_f64),
            Cond::Le(76, 0.00010233672_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 230 ethusdt_15m_rules_230: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Ge(46, 78.511633328839_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 231 ethusdt_15m_rules_231: RED
    (
        false,
        &[
            Cond::Le(52, 0.0_f64),
            Cond::Le(76, 0.000034905632_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 232 ethusdt_15m_rules_232: RED
    (
        false,
        &[
            Cond::Ge(63, 76.287641686518_f64),
            Cond::Le(79, -0.939474731553_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 233 ethusdt_15m_rules_233: RED
    (
        false,
        &[
            Cond::Ge(63, 85.681824259181_f64),
            Cond::Ge(76, 0.0067308277_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 234 ethusdt_15m_rules_234: GREEN
    (
        true,
        &[
            Cond::Le(35, 0.0_f64),
            Cond::Ge(61, 72.546035051275_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 235 ethusdt_15m_rules_235: GREEN
    (
        true,
        &[
            Cond::Le(71, 13.40911567_f64),
            Cond::Le(80, -0.8641092984_f64),
            Cond::Ge(43, 0.1106917761_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 236 ethusdt_15m_rules_236: RED
    (
        false,
        &[
            Cond::Ge(25, -0.001665889857_f64),
            Cond::Le(40, 0.405699915549_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 237 ethusdt_15m_rules_237: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(71, 61.254716857094_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 238 ethusdt_15m_rules_238: RED
    (
        false,
        &[
            Cond::Ge(71, 87.582148468611_f64),
            Cond::Le(15, 0.056247011082_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 239 ethusdt_15m_rules_239: RED
    (
        false,
        &[
            Cond::Ge(40, 0.772240951118_f64),
            Cond::Le(11, -1.02601582424_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 240 ethusdt_15m_rules_240: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00121384214_f64),
            Cond::Le(15, 0.408114754734_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 241 ethusdt_15m_rules_241: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.828421748691_f64),
            Cond::Ge(24, -0.003494666934_f64),
        ],
    ),
    // 242 ethusdt_15m_rules_242: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.325581395_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Ge(84, -99.31370042_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 243 ethusdt_15m_rules_243: RED
    (
        false,
        &[
            Cond::Ge(71, 93.154663125084_f64),
            Cond::Le(5, -0.001147611306_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 244 ethusdt_15m_rules_244: GREEN
    (
        true,
        &[
            Cond::Le(13, -183.059643654916_f64),
            Cond::Le(15, 0.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 245 ethusdt_15m_rules_245: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.089556532241_f64),
            Cond::Le(82, -0.015077248431_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 246 ethusdt_15m_rules_246: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.006837778067_f64),
            Cond::Le(63, 13.5929322_f64),
            Cond::Ge(17, -2.584346456_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 247 ethusdt_15m_rules_247: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000544949204_f64),
            Cond::Ge(74, 0.004468097235_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 248 ethusdt_15m_rules_248: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000186502931_f64),
            Cond::Le(79, -1.385238691491_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 249 ethusdt_15m_rules_249: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(45, 0.004216605253_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 250 ethusdt_15m_rules_250: RED
    (
        false,
        &[
            Cond::Ge(64, 82.454573967079_f64),
            Cond::Le(27, 0.017971631655_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 251 ethusdt_15m_rules_251: GREEN
    (
        true,
        &[
            Cond::Le(71, 3.592157413_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 252 ethusdt_15m_rules_252: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(51, -0.052398857241_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 253 ethusdt_15m_rules_253: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.65908861946_f64),
            Cond::Le(19, 0.607016051968_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 254 ethusdt_15m_rules_254: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(41, 9.0_f64),
            Cond::Le(18, 3.429563387_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 255 ethusdt_15m_rules_255: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::In(41, &[20.0_f64]),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 256 ethusdt_15m_rules_256: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.679463493_f64),
            Cond::Ge(6, 0.008140445126_f64),
            Cond::Le(45, -0.00345019142_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 257 ethusdt_15m_rules_257: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(6, 0.000049170938_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 258 ethusdt_15m_rules_258: RED
    (
        false,
        &[
            Cond::Ge(64, 82.454573967079_f64),
            Cond::Le(19, 0.639508691164_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 259 ethusdt_15m_rules_259: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.006258679323_f64),
            Cond::Ge(48, 88.271737232917_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 260 ethusdt_15m_rules_260: GREEN
    (
        true,
        &[
            Cond::Le(5, 0.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(43, 2.0_f64),
            Cond::Ge(78, 1.0_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 261 ethusdt_15m_rules_261: RED
    (
        false,
        &[
            Cond::Ge(24, -0.00152070846_f64),
            Cond::Ge(0, 4.34858605411_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 262 ethusdt_15m_rules_262: RED
    (
        false,
        &[
            Cond::Ge(72, 87.588360619543_f64),
            Cond::Le(36, 1.0_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 263 ethusdt_15m_rules_263: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Ge(43, 5.286416861829_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 264 ethusdt_15m_rules_264: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.404114354323_f64),
            Cond::Ge(10, -0.005516138794_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 265 ethusdt_15m_rules_265: GREEN
    (
        true,
        &[
            Cond::Le(64, 27.937723759111_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 266 ethusdt_15m_rules_266: RED
    (
        false,
        &[
            Cond::Ge(40, 0.712764986117_f64),
            Cond::Ge(7, 0.854118381787_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 267 ethusdt_15m_rules_267: RED
    (
        false,
        &[
            Cond::Ge(63, 78.115236912005_f64),
            Cond::Le(13, 47.198310116171_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 268 ethusdt_15m_rules_268: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.53343598_f64),
            Cond::Le(42, 0.0001510480269_f64),
            Cond::Ge(18, -2.194643073_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 269 ethusdt_15m_rules_269: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.240426681222_f64),
            Cond::Ge(23, 0.013975919138_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 270 ethusdt_15m_rules_270: RED
    (
        false,
        &[
            Cond::Ge(16, 1.962352079221_f64),
            Cond::Le(48, 37.017750359136_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 271 ethusdt_15m_rules_271: RED
    (
        false,
        &[
            Cond::Ge(64, 79.78754453_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Le(62, 64.95549342_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 272 ethusdt_15m_rules_272: GREEN
    (
        true,
        &[
            Cond::Le(72, 13.317971610423_f64),
            Cond::Between(71, 43.047101515345_f64, 61.449567146817_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 273 ethusdt_15m_rules_273: GREEN
    (
        true,
        &[
            Cond::Le(72, 10.434588873737_f64),
            Cond::Ge(81, 0.004008177104_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 274 ethusdt_15m_rules_274: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.6_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 275 ethusdt_15m_rules_275: RED
    (
        false,
        &[
            Cond::Ge(48, 92.869319882182_f64),
            Cond::Le(74, 0.000272339529_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 276 ethusdt_15m_rules_276: RED
    (
        false,
        &[
            Cond::Ge(64, 88.325740640992_f64),
            Cond::Le(17, 1.703673786689_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 277 ethusdt_15m_rules_277: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::In(41, &[12.0_f64]),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 278 ethusdt_15m_rules_278: RED
    (
        false,
        &[
            Cond::Ge(48, 90.21492658429_f64),
            Cond::Le(11, -0.479441712554_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 279 ethusdt_15m_rules_279: RED
    (
        false,
        &[
            Cond::Ge(16, 2.082088491926_f64),
            Cond::Le(45, -0.004456144174_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 280 ethusdt_15m_rules_280: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.335678247138_f64),
            Cond::Ge(44, 0.002443200772_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 281 ethusdt_15m_rules_281: RED
    (
        false,
        &[
            Cond::Ge(72, 96.0335550175_f64),
            Cond::Le(79, -0.845591632774_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 282 ethusdt_15m_rules_282: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.469218562694_f64),
            Cond::In(41, &[22.0_f64]),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 283 ethusdt_15m_rules_283: RED
    (
        false,
        &[
            Cond::Ge(9, 0.02269168662_f64),
            Cond::Le(59, -0.013915095326_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 284 ethusdt_15m_rules_284: GREEN
    (
        true,
        &[
            Cond::Le(61, 18.578805575288_f64),
            Cond::Ge(59, -0.018916226339_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 285 ethusdt_15m_rules_285: GREEN
    (
        true,
        &[
            Cond::Le(63, 36.800776725066_f64),
            Cond::Le(51, -1.422984964535_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 286 ethusdt_15m_rules_286: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.013135821846_f64),
            Cond::Ge(19, 2.080444997831_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 287 ethusdt_15m_rules_287: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.130209885596_f64),
            Cond::Ge(13, 47.198310116171_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 288 ethusdt_15m_rules_288: RED
    (
        false,
        &[
            Cond::Ge(72, 96.0335550175_f64),
            Cond::In(41, &[11.0_f64]),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 289 ethusdt_15m_rules_289: RED
    (
        false,
        &[
            Cond::Ge(16, 1.594894995073_f64),
            Cond::Le(63, 54.691946764945_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 290 ethusdt_15m_rules_290: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.005506890184_f64),
            Cond::Ge(48, 79.537380124647_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 291 ethusdt_15m_rules_291: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(78, 0.216880790466_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 292 ethusdt_15m_rules_292: GREEN
    (
        true,
        &[
            Cond::Le(72, 13.599250472473_f64),
            Cond::Le(1, -21.606633337189_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 293 ethusdt_15m_rules_293: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(78, 0.216880790466_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 294 ethusdt_15m_rules_294: GREEN
    (
        true,
        &[
            Cond::Ge(78, 4.86378068509_f64),
            Cond::Between(16, -1.130209885596_f64, 1.148339337785_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 295 ethusdt_15m_rules_295: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000717425385_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 296 ethusdt_15m_rules_296: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 297 ethusdt_15m_rules_297: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.083545302651_f64),
            Cond::Le(78, 0.342897905517_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 298 ethusdt_15m_rules_298: RED
    (
        false,
        &[
            Cond::Ge(40, 0.712764986117_f64),
            Cond::Ge(7, 0.854118381787_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 299 ethusdt_15m_rules_299: RED
    (
        false,
        &[
            Cond::Ge(24, -0.002433279017_f64),
            Cond::Ge(33, 1.0_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 300 ethusdt_15m_rules_300: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.025827454359_f64),
            Cond::Ge(71, 83.212445361432_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 301 ethusdt_15m_rules_301: GREEN
    (
        true,
        &[
            Cond::Le(72, 5.042031086835_f64),
            Cond::Ge(49, 1425.0_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 302 ethusdt_15m_rules_302: GREEN
    (
        true,
        &[
            Cond::Le(72, 13.317971610423_f64),
            Cond::Between(71, 43.047101515345_f64, 61.449567146817_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 303 ethusdt_15m_rules_303: GREEN
    (
        true,
        &[
            Cond::Ge(53, 5.0_f64),
            Cond::Ge(40, 0.713503577816_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 304 ethusdt_15m_rules_304: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.405699915549_f64),
            Cond::Ge(38, 0.008514598019_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 305 ethusdt_15m_rules_305: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::Le(11, -0.436252860345_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 306 ethusdt_15m_rules_306: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(77, 0.00198697707_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 307 ethusdt_15m_rules_307: RED
    (
        false,
        &[
            Cond::Ge(72, 87.588360619543_f64),
            Cond::Ge(1, 13.438071751687_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 308 ethusdt_15m_rules_308: RED
    (
        false,
        &[
            Cond::Ge(71, 92.58675389879_f64),
            Cond::Le(15, 0.306261093449_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 309 ethusdt_15m_rules_309: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000589689786_f64),
            Cond::Ge(2, 0.0076658772_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 310 ethusdt_15m_rules_310: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.933538583907_f64),
            Cond::Ge(63, 33.871698910911_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 311 ethusdt_15m_rules_311: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000360001149_f64),
            Cond::Le(1, -0.920039690208_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 312 ethusdt_15m_rules_312: RED
    (
        false,
        &[
            Cond::Ge(71, 95.649332745762_f64),
            Cond::Ge(82, 0.027527019994_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 313 ethusdt_15m_rules_313: GREEN
    (
        true,
        &[
            Cond::Le(64, 27.937723759111_f64),
            Cond::Le(19, 0.517302629848_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 314 ethusdt_15m_rules_314: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.210641246093_f64),
            Cond::Ge(23, 0.007795884974_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 315 ethusdt_15m_rules_315: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.148849848163_f64),
            Cond::Ge(82, 0.022316510302_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 316 ethusdt_15m_rules_316: GREEN
    (
        true,
        &[
            Cond::Le(61, 34.187315401094_f64),
            Cond::Le(19, 0.519485662844_f64),
        ],
    ),
    // 317 ethusdt_15m_rules_317: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(51, -1.315533522184_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 318 ethusdt_15m_rules_318: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(63, 25.0_f64),
            Cond::Ge(50, 1.2_f64),
            Cond::Ge(7, 0.45_f64),
            Cond::Eq(83, 6.0_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 319 ethusdt_15m_rules_319: GREEN
    (
        true,
        &[
            Cond::Le(44, -0.002650404386_f64),
            Cond::Le(71, 5.152344313_f64),
            Cond::Le(72, 3.478803314_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 320 ethusdt_15m_rules_320: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(57, -0.01422341075_f64),
            Cond::Le(80, 0.678389517_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 321 ethusdt_15m_rules_321: RED
    (
        false,
        &[
            Cond::Ge(16, 2.483924092893_f64),
            Cond::Le(23, -0.009394741506_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 322 ethusdt_15m_rules_322: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.005185093586_f64),
            Cond::Le(2, 0.0031654888_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 323 ethusdt_15m_rules_323: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::Le(77, 0.001627937385_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 324 ethusdt_15m_rules_324: RED
    (
        false,
        &[
            Cond::Ge(4, 1.02132756984_f64),
            Cond::Le(79, -0.82264325452_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 325 ethusdt_15m_rules_325: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.027370425915_f64),
            Cond::Le(1, -0.295289470852_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 326 ethusdt_15m_rules_326: RED
    (
        false,
        &[
            Cond::Ge(72, 98.04361321_f64),
            Cond::Ge(8, 0.02432418065_f64),
            Cond::Ge(37, 3.0_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 327 ethusdt_15m_rules_327: RED
    (
        false,
        &[
            Cond::Ge(64, 70.510977130054_f64),
            Cond::Le(1, -20.780423400716_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 328 ethusdt_15m_rules_328: GREEN
    (
        true,
        &[
            Cond::Le(72, 8.683865767064_f64),
            Cond::Ge(16, -0.753237336396_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 329 ethusdt_15m_rules_329: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(63, 40.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 330 ethusdt_15m_rules_330: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::In(41, &[12.0_f64]),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 331 ethusdt_15m_rules_331: RED
    (
        false,
        &[
            Cond::Ge(71, 96.3831114696_f64),
            Cond::Le(78, 0.357082553591_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 332 ethusdt_15m_rules_332: RED
    (
        false,
        &[
            Cond::Ge(25, -0.001665889857_f64),
            Cond::Le(40, 0.405699915549_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 333 ethusdt_15m_rules_333: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::Le(6, 0.000412803504_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 334 ethusdt_15m_rules_334: RED
    (
        false,
        &[
            Cond::Ge(72, 97.799245309998_f64),
            Cond::Le(12, 47.005414521394_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 335 ethusdt_15m_rules_335: RED
    (
        false,
        &[
            Cond::Ge(71, 94.73247534402_f64),
            Cond::Le(19, 0.562623940981_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 336 ethusdt_15m_rules_336: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(77, 0.007022934773_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 337 ethusdt_15m_rules_337: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.353463333651_f64),
            Cond::Ge(34, 6.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 338 ethusdt_15m_rules_338: GREEN
    (
        true,
        &[
            Cond::Le(72, 9.590121893836_f64),
            Cond::Le(11, -0.887448173015_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 339 ethusdt_15m_rules_339: RED
    (
        false,
        &[
            Cond::Ge(63, 87.21365662096_f64),
            Cond::In(83, &[6.0_f64]),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 340 ethusdt_15m_rules_340: GREEN
    (
        true,
        &[
            Cond::Le(12, -188.547426560093_f64),
            Cond::Le(51, -0.638402427824_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 341 ethusdt_15m_rules_341: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(0, 4.657215509588_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 342 ethusdt_15m_rules_342: GREEN
    (
        true,
        &[
            Cond::Le(72, 13.599250472473_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 343 ethusdt_15m_rules_343: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(6, 0.007864217465_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 344 ethusdt_15m_rules_344: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.007057262978_f64),
            Cond::Ge(13, 151.60195878472_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 345 ethusdt_15m_rules_345: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000547061338_f64),
            Cond::In(83, &[5.0_f64]),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 346 ethusdt_15m_rules_346: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.828421748691_f64),
            Cond::Ge(12, -20.481234551541_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 347 ethusdt_15m_rules_347: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(3, 0.002457878466_f64),
            Cond::Le(71, 12.12790869_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 348 ethusdt_15m_rules_348: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Ge(8, -0.009912424083_f64),
            Cond::Ge(7, 0.761334494_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 349 ethusdt_15m_rules_349: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000886273409_f64),
            Cond::Le(44, -0.002122380767_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 350 ethusdt_15m_rules_350: RED
    (
        false,
        &[
            Cond::Ge(71, 95.010861843374_f64),
            Cond::Le(46, 30.621116950057_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 351 ethusdt_15m_rules_351: RED
    (
        false,
        &[
            Cond::Ge(71, 98.621001727441_f64),
            Cond::Le(15, 0.926451224707_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 352 ethusdt_15m_rules_352: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(82, -0.023171858787_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 353 ethusdt_15m_rules_353: RED
    (
        false,
        &[
            Cond::Ge(4, 1.02132756984_f64),
            Cond::Le(79, -0.82264325452_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 354 ethusdt_15m_rules_354: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 355 ethusdt_15m_rules_355: RED
    (
        false,
        &[
            Cond::Ge(9, 0.044598294078_f64),
            Cond::Le(2, 0.010328064626_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 356 ethusdt_15m_rules_356: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(79, -0.992604162908_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 357 ethusdt_15m_rules_357: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.371343923784_f64),
            Cond::Ge(58, 0.00710213073_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 358 ethusdt_15m_rules_358: RED
    (
        false,
        &[
            Cond::Ge(71, 95.36043284_f64),
            Cond::Ge(56, 0.01939351557_f64),
            Cond::Eq(83, 4.0_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 359 ethusdt_15m_rules_359: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.002456946598_f64),
            Cond::Ge(19, 1.804032915518_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 360 ethusdt_15m_rules_360: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.001538919285_f64),
            Cond::Ge(82, 0.025184469595_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 361 ethusdt_15m_rules_361: RED
    (
        false,
        &[
            Cond::Ge(40, 0.767976820667_f64),
            Cond::Le(46, 24.135433762724_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 362 ethusdt_15m_rules_362: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(24, -0.02357829884_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 363 ethusdt_15m_rules_363: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Ge(57, -0.01042349892_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 364 ethusdt_15m_rules_364: RED
    (
        false,
        &[
            Cond::Ge(16, 2.120054690474_f64),
            Cond::Le(46, 30.058809486348_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 365 ethusdt_15m_rules_365: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.062588143616_f64),
            Cond::In(41, &[6.0_f64]),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 366 ethusdt_15m_rules_366: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Ge(13, -43.945068697837_f64),
        ],
    ),
    // 367 ethusdt_15m_rules_367: GREEN
    (
        true,
        &[
            Cond::Le(71, 3.170746597814_f64),
            Cond::Ge(75, 3.979483531844_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 368 ethusdt_15m_rules_368: GREEN
    (
        true,
        &[
            Cond::Le(71, 7.081893123604_f64),
            Cond::Le(51, -0.975669526392_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 369 ethusdt_15m_rules_369: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Le(19, 0.519485662844_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 370 ethusdt_15m_rules_370: GREEN
    (
        true,
        &[
            Cond::Le(12, -225.990615221245_f64),
            Cond::Ge(38, -0.00191371853_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 371 ethusdt_15m_rules_371: GREEN
    (
        true,
        &[
            Cond::Le(22, -0.049413631372_f64),
            Cond::Le(1, -1.127098909018_f64),
        ],
    ),
    // 372 ethusdt_15m_rules_372: GREEN
    (
        true,
        &[
            Cond::Le(46, 20.566606160548_f64),
            Cond::Le(19, 0.487954125812_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 373 ethusdt_15m_rules_373: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(0, 14.249712652214_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 374 ethusdt_15m_rules_374: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Le(50, 0.692354618129_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 375 ethusdt_15m_rules_375: GREEN
    (
        true,
        &[
            Cond::Le(5, 0.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(43, 2.0_f64),
            Cond::Ge(78, 1.0_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 376 ethusdt_15m_rules_376: RED
    (
        false,
        &[
            Cond::Ge(64, 82.454573967079_f64),
            Cond::Le(19, 0.639508691164_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 377 ethusdt_15m_rules_377: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.469218562694_f64),
            Cond::In(41, &[22.0_f64]),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 378 ethusdt_15m_rules_378: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.124295441041_f64),
            Cond::Ge(23, 0.0103205015_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 379 ethusdt_15m_rules_379: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.349039721054_f64),
            Cond::Le(51, -1.243294430995_f64),
        ],
    ),
    // 380 ethusdt_15m_rules_380: RED
    (
        false,
        &[
            Cond::Ge(40, 0.660005226124_f64),
            Cond::Ge(2, 0.014808079873_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 381 ethusdt_15m_rules_381: RED
    (
        false,
        &[
            Cond::Ge(71, 90.051572964609_f64),
            Cond::Le(5, -0.002629444341_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 382 ethusdt_15m_rules_382: RED
    (
        false,
        &[
            Cond::Ge(72, 83.706222662312_f64),
            Cond::Le(27, 0.004099778821_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 383 ethusdt_15m_rules_383: RED
    (
        false,
        &[
            Cond::Ge(72, 98.060537876782_f64),
            Cond::Le(0, -1.614870643081_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 384 ethusdt_15m_rules_384: RED
    (
        false,
        &[
            Cond::Le(52, 0.0_f64),
            Cond::Le(76, 0.000034905632_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 385 ethusdt_15m_rules_385: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(51, 1.413382543016_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 386 ethusdt_15m_rules_386: GREEN
    (
        true,
        &[
            Cond::Le(63, 19.291134799525_f64),
            Cond::Le(11, -0.313680587227_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 387 ethusdt_15m_rules_387: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Between(79, -0.470588798961_f64, -0.128994916769_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 388 ethusdt_15m_rules_388: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 389 ethusdt_15m_rules_389: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Le(6, 0.000139017832_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 390 ethusdt_15m_rules_390: GREEN
    (
        true,
        &[
            Cond::Le(71, 4.845802158605_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 391 ethusdt_15m_rules_391: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.218185817798_f64),
            Cond::Le(19, 0.712660732397_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 392 ethusdt_15m_rules_392: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.273882464262_f64),
            Cond::Ge(29, 0.030155709967_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 393 ethusdt_15m_rules_393: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000813794422_f64),
            Cond::Le(45, -0.007007852866_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 394 ethusdt_15m_rules_394: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(63, 40.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 395 ethusdt_15m_rules_395: RED
    (
        false,
        &[
            Cond::Ge(72, 96.167908771068_f64),
            Cond::Le(25, -0.001936103113_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 396 ethusdt_15m_rules_396: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.005348262374_f64),
            Cond::Ge(61, 69.060618393279_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 397 ethusdt_15m_rules_397: RED
    (
        false,
        &[
            Cond::Ge(72, 98.04361321_f64),
            Cond::Ge(8, 0.02432418065_f64),
            Cond::Ge(37, 3.0_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 398 ethusdt_15m_rules_398: GREEN
    (
        true,
        &[
            Cond::Le(71, 24.430142129257_f64),
            Cond::Le(50, 0.277103422926_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 399 ethusdt_15m_rules_399: RED
    (
        false,
        &[
            Cond::Ge(24, -0.002433279017_f64),
            Cond::Ge(33, 1.0_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 400 ethusdt_15m_rules_400: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(24, -0.01631766978_f64),
            Cond::Ge(7, 0.761334494_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 401 ethusdt_15m_rules_401: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000377873908_f64),
            Cond::Le(82, -0.010076231_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 402 ethusdt_15m_rules_402: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000193374386_f64),
            Cond::Le(78, 0.427551130261_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 403 ethusdt_15m_rules_403: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.002929379759_f64),
            Cond::Ge(74, 0.005230358453_f64),
            Cond::Ge(50, 1.386145597_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 404 ethusdt_15m_rules_404: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.1302923821_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Le(61, 34.24698515_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 405 ethusdt_15m_rules_405: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.014926060763_f64),
            Cond::Ge(34, 6.0_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 406 ethusdt_15m_rules_406: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(79, -1.341363982825_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 407 ethusdt_15m_rules_407: RED
    (
        false,
        &[
            Cond::Ge(12, 239.826401687036_f64),
            Cond::Ge(76, 0.003626385089_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 408 ethusdt_15m_rules_408: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(51, -1.315533522184_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 409 ethusdt_15m_rules_409: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.005558113318_f64),
            Cond::Le(44, -0.003023911405_f64),
            Cond::Ge(18, -2.500795018_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 410 ethusdt_15m_rules_410: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 411 ethusdt_15m_rules_411: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.742641598641_f64),
            Cond::Ge(46, 55.226633333302_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 412 ethusdt_15m_rules_412: RED
    (
        false,
        &[
            Cond::Ge(40, 0.795203506196_f64),
            Cond::Le(0, -9.185931229593_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 413 ethusdt_15m_rules_413: RED
    (
        false,
        &[
            Cond::Ge(4, 1.011928971326_f64),
            Cond::Le(51, -1.203389651906_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 414 ethusdt_15m_rules_414: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Le(30, 0.001228070749_f64),
            Cond::Eq(83, 1.0_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 415 ethusdt_15m_rules_415: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(6, 0.000026576198_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 416 ethusdt_15m_rules_416: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00135922631_f64),
            Cond::Ge(45, 0.013873233781_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 417 ethusdt_15m_rules_417: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(79, -1.159346030872_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 418 ethusdt_15m_rules_418: GREEN
    (
        true,
        &[
            Cond::Le(63, 16.441111837773_f64),
            Cond::Between(58, -0.003698886648_f64, 0.003969005461_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 419 ethusdt_15m_rules_419: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Ge(75, 1.27376961958_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 420 ethusdt_15m_rules_420: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.084109929238_f64),
            Cond::Ge(29, 0.024230497416_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 421 ethusdt_15m_rules_421: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(10, -0.01817602056_f64),
            Cond::Ge(80, 3.587853962_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 422 ethusdt_15m_rules_422: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.246542685688_f64),
            Cond::Ge(25, -0.006079005042_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 423 ethusdt_15m_rules_423: GREEN
    (
        true,
        &[
            Cond::Le(71, 6.452676568325_f64),
            Cond::Ge(44, 0.000677516823_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 424 ethusdt_15m_rules_424: GREEN
    (
        true,
        &[
            Cond::Le(71, 3.592157413_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Le(48, 18.94457115_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 425 ethusdt_15m_rules_425: GREEN
    (
        true,
        &[
            Cond::Le(9, -0.005375380189_f64),
            Cond::Ge(21, 0.029677055093_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 426 ethusdt_15m_rules_426: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000120857876_f64),
            Cond::Le(7, 0.302247845896_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 427 ethusdt_15m_rules_427: GREEN
    (
        true,
        &[
            Cond::Le(71, 6.452676568325_f64),
            Cond::Le(0, -1.915883231776_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 428 ethusdt_15m_rules_428: RED
    (
        false,
        &[
            Cond::Ge(72, 98.632249177409_f64),
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 429 ethusdt_15m_rules_429: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Ge(76, 0.006286214631_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 430 ethusdt_15m_rules_430: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000905802239_f64),
            Cond::Ge(81, 0.005828474127_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 431 ethusdt_15m_rules_431: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Between(50, 0.754270965462_f64, 0.952041479441_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 432 ethusdt_15m_rules_432: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.240426681222_f64),
            Cond::Ge(23, 0.013975919138_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 433 ethusdt_15m_rules_433: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Between(1, -0.138476139903_f64, -0.002142237909_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 434 ethusdt_15m_rules_434: RED
    (
        false,
        &[
            Cond::Ge(40, 0.80253046606_f64),
            Cond::Ge(0, 7.069171215076_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 435 ethusdt_15m_rules_435: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.502131887069_f64),
            Cond::Ge(20, -0.016588506863_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 436 ethusdt_15m_rules_436: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.898892702_f64),
            Cond::Le(44, -0.002344176743_f64),
            Cond::Ge(50, 1.403339542_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 437 ethusdt_15m_rules_437: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.371097431342_f64),
            Cond::Ge(82, 0.010852199486_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 438 ethusdt_15m_rules_438: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(77, 0.00198697707_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 439 ethusdt_15m_rules_439: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 6.0_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 440 ethusdt_15m_rules_440: GREEN
    (
        true,
        &[
            Cond::Le(72, 1.091431392_f64),
            Cond::Le(10, -0.01392502169_f64),
            Cond::Le(36, 1.0_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 441 ethusdt_15m_rules_441: GREEN
    (
        true,
        &[
            Cond::Le(72, 5.251600440171_f64),
            Cond::Le(76, 0.000495743204_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 442 ethusdt_15m_rules_442: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001668628847_f64),
            Cond::Ge(82, 0.017955482572_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 443 ethusdt_15m_rules_443: GREEN
    (
        true,
        &[
            Cond::Le(63, 37.285529400902_f64),
            Cond::Ge(82, 0.021524632953_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 444 ethusdt_15m_rules_444: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 6.0_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 445 ethusdt_15m_rules_445: GREEN
    (
        true,
        &[
            Cond::Le(61, 24.975510474295_f64),
            Cond::Le(42, 0.000193642335_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 446 ethusdt_15m_rules_446: GREEN
    (
        true,
        &[
            Cond::Le(24, -0.03797357864_f64),
            Cond::Le(47, 13.8331558_f64),
            Cond::Le(18, -3.063615586_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 447 ethusdt_15m_rules_447: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.084109929238_f64),
            Cond::Ge(29, 0.024230497416_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 448 ethusdt_15m_rules_448: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(57, -0.01422341075_f64),
            Cond::Le(80, 0.678389517_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 449 ethusdt_15m_rules_449: RED
    (
        false,
        &[
            Cond::Ge(72, 98.632249177409_f64),
            Cond::Ge(0, 0.941851345832_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 450 ethusdt_15m_rules_450: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(24, -0.007401969759_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 451 ethusdt_15m_rules_451: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.048300974027_f64),
            Cond::Ge(27, 0.038486437398_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 452 ethusdt_15m_rules_452: RED
    (
        false,
        &[
            Cond::Ge(16, 2.120054690474_f64),
            Cond::Le(46, 30.058809486348_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 453 ethusdt_15m_rules_453: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.002900258506_f64),
            Cond::Ge(82, 0.020288489414_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 454 ethusdt_15m_rules_454: GREEN
    (
        true,
        &[
            Cond::Le(73, 8.202131158_f64),
            Cond::Eq(41, 12.0_f64),
            Cond::Le(62, 32.08032065_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 455 ethusdt_15m_rules_455: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(71, 95.36043284_f64),
            Cond::Le(72, 96.70846245_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 456 ethusdt_15m_rules_456: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 457 ethusdt_15m_rules_457: GREEN
    (
        true,
        &[
            Cond::Ge(78, 4.86378068509_f64),
            Cond::In(83, &[0.0_f64]),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 458 ethusdt_15m_rules_458: RED
    (
        false,
        &[
            Cond::Ge(64, 70.589612029705_f64),
            Cond::Le(46, 40.166548899026_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 459 ethusdt_15m_rules_459: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.834423453642_f64),
            Cond::Le(77, 0.000714281391_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 460 ethusdt_15m_rules_460: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(2, 0.00260692919_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 461 ethusdt_15m_rules_461: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000377873908_f64),
            Cond::Le(82, -0.010076231_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 462 ethusdt_15m_rules_462: RED
    (
        false,
        &[
            Cond::Ge(72, 87.431021042805_f64),
            Cond::Ge(74, 0.008462058792_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 463 ethusdt_15m_rules_463: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.00194583421_f64),
            Cond::Ge(82, 0.025184469595_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 464 ethusdt_15m_rules_464: GREEN
    (
        true,
        &[
            Cond::Le(61, 25.790818624552_f64),
            Cond::Ge(24, -0.007460989272_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 465 ethusdt_15m_rules_465: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.022563546352_f64),
            Cond::Ge(74, 0.012910358989_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 466 ethusdt_15m_rules_466: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.00237146059_f64),
            Cond::Ge(61, 68.072657708997_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 467 ethusdt_15m_rules_467: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Ge(8, -0.009912424083_f64),
            Cond::Ge(7, 0.761334494_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 468 ethusdt_15m_rules_468: RED
    (
        false,
        &[
            Cond::Ge(40, 0.734509067946_f64),
            Cond::Le(25, -0.037167657112_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 469 ethusdt_15m_rules_469: RED
    (
        false,
        &[
            Cond::Ge(72, 96.395460340985_f64),
            Cond::Le(76, 0.000172338933_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 470 ethusdt_15m_rules_470: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000161605691_f64),
            Cond::Ge(2, 0.008348928269_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 471 ethusdt_15m_rules_471: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.148849848163_f64),
            Cond::Ge(82, 0.022316510302_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 472 ethusdt_15m_rules_472: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Le(6, 0.000139017832_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 473 ethusdt_15m_rules_473: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.933538583907_f64),
            Cond::Ge(63, 33.871698910911_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 474 ethusdt_15m_rules_474: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.218185817798_f64),
            Cond::Le(19, 0.712660732397_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 475 ethusdt_15m_rules_475: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::Le(6, 0.000412803504_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 476 ethusdt_15m_rules_476: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(63, 40.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 477 ethusdt_15m_rules_477: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.246542685688_f64),
            Cond::Ge(25, -0.006079005042_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 478 ethusdt_15m_rules_478: GREEN
    (
        true,
        &[
            Cond::Le(64, 29.828518892658_f64),
            Cond::Le(19, 0.517302629848_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 479 ethusdt_15m_rules_479: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.418823304347_f64),
            Cond::Ge(19, 1.731253361596_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 480 ethusdt_15m_rules_480: GREEN
    (
        true,
        &[
            Cond::Le(72, 13.599250472473_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 481 ethusdt_15m_rules_481: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::Le(27, 0.001007431938_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 482 ethusdt_15m_rules_482: RED
    (
        false,
        &[
            Cond::Ge(46, 87.326571431094_f64),
            Cond::Between(23, -0.003483247059_f64, 0.005497807331_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 483 ethusdt_15m_rules_483: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000258173049_f64),
            Cond::Ge(15, 0.287315499607_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 484 ethusdt_15m_rules_484: RED
    (
        false,
        &[
            Cond::Ge(72, 97.799245309998_f64),
            Cond::Ge(22, 0.050318074037_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 485 ethusdt_15m_rules_485: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.1302923821_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Le(61, 34.24698515_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 486 ethusdt_15m_rules_486: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Ge(43, 5.286416861829_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 487 ethusdt_15m_rules_487: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(24, -0.02357829884_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 488 ethusdt_15m_rules_488: RED
    (
        false,
        &[
            Cond::Ge(72, 99.030619805153_f64),
            Cond::In(41, &[20.0_f64]),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 489 ethusdt_15m_rules_489: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.065876434269_f64),
            Cond::Le(40, 0.216230850514_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 490 ethusdt_15m_rules_490: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(12, -191.4788279_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 491 ethusdt_15m_rules_491: RED
    (
        false,
        &[
            Cond::Ge(72, 98.632249177409_f64),
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 492 ethusdt_15m_rules_492: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.418823304347_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 493 ethusdt_15m_rules_493: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000053521742_f64),
            Cond::Ge(43, 2.665841584159_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 494 ethusdt_15m_rules_494: RED
    (
        false,
        &[
            Cond::Ge(64, 73.82299439_f64),
            Cond::Le(60, -0.01475596055_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 495 ethusdt_15m_rules_495: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000394386125_f64),
            Cond::Le(51, -0.781972702142_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 496 ethusdt_15m_rules_496: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::In(41, &[12.0_f64]),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 497 ethusdt_15m_rules_497: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.834423453642_f64),
            Cond::Le(77, 0.000714281391_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 498 ethusdt_15m_rules_498: GREEN
    (
        true,
        &[
            Cond::Le(71, 4.845802158605_f64),
            Cond::Le(51, -0.781972702142_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 499 ethusdt_15m_rules_499: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Ge(5, -0.006258679323_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 500 ethusdt_15m_rules_500: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.1302923821_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Le(61, 34.24698515_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 501 ethusdt_15m_rules_501: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Between(50, 0.754270965462_f64, 0.952041479441_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 502 ethusdt_15m_rules_502: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(39, 0.255739517915_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 503 ethusdt_15m_rules_503: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(43, 0.01349188119_f64),
            Cond::Le(74, 0.00008848352749_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 504 ethusdt_15m_rules_504: GREEN
    (
        true,
        &[
            Cond::Le(71, 0.5066458518_f64),
            Cond::Eq(83, 5.0_f64),
            Cond::Ge(42, 9.29093662300000e-8_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 505 ethusdt_15m_rules_505: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00135922631_f64),
            Cond::Ge(45, 0.013873233781_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 506 ethusdt_15m_rules_506: GREEN
    (
        true,
        &[
            Cond::Le(72, 9.711755951452_f64),
            Cond::Le(78, 0.357082553591_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 507 ethusdt_15m_rules_507: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::Ge(21, -0.010226964393_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 508 ethusdt_15m_rules_508: GREEN
    (
        true,
        &[
            Cond::Le(71, 6.452676568325_f64),
            Cond::Ge(76, 0.017899964951_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 509 ethusdt_15m_rules_509: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000546936819_f64),
            Cond::Le(7, 0.109269006706_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 510 ethusdt_15m_rules_510: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.53343598_f64),
            Cond::Le(42, 0.0001510480269_f64),
            Cond::Ge(18, -2.194643073_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 511 ethusdt_15m_rules_511: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.509371455114_f64),
            Cond::Ge(17, -1.189485911056_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 512 ethusdt_15m_rules_512: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.53343598_f64),
            Cond::Le(42, 0.0001510480269_f64),
            Cond::Ge(18, -2.194643073_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 513 ethusdt_15m_rules_513: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(63, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 6.0_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 514 ethusdt_15m_rules_514: GREEN
    (
        true,
        &[
            Cond::Le(64, 29.216775727159_f64),
            Cond::Le(19, 0.442037442949_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 515 ethusdt_15m_rules_515: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000394386125_f64),
            Cond::Le(50, 0.663177938908_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 516 ethusdt_15m_rules_516: RED
    (
        false,
        &[
            Cond::Ge(63, 67.899626426361_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 517 ethusdt_15m_rules_517: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.399203004425_f64),
            Cond::Ge(77, 0.005915796217_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 518 ethusdt_15m_rules_518: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::Le(77, 0.001627937385_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 519 ethusdt_15m_rules_519: GREEN
    (
        true,
        &[
            Cond::Le(61, 24.975510474295_f64),
            Cond::Le(42, 0.000193642335_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 520 ethusdt_15m_rules_520: GREEN
    (
        true,
        &[
            Cond::Le(63, 11.111136496641_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 521 ethusdt_15m_rules_521: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.722439334531_f64),
            Cond::Le(2, 0.003127045792_f64),
        ],
    ),
    // 522 ethusdt_15m_rules_522: RED
    (
        false,
        &[
            Cond::Ge(24, -0.001005628718_f64),
            Cond::Le(76, 0.000048914233_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 523 ethusdt_15m_rules_523: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.371097431342_f64),
            Cond::Le(46, 21.227386631273_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 524 ethusdt_15m_rules_524: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.605810138122_f64),
            Cond::Between(12, -20.291648932434_f64, 24.041740853877_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 525 ethusdt_15m_rules_525: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(81, 0.014461038157_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 526 ethusdt_15m_rules_526: RED
    (
        false,
        &[
            Cond::Ge(25, -0.005068052866_f64),
            Cond::Ge(2, 0.022335792029_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 527 ethusdt_15m_rules_527: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000547061338_f64),
            Cond::In(83, &[5.0_f64]),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 528 ethusdt_15m_rules_528: GREEN
    (
        true,
        &[
            Cond::Le(63, 17.596607453968_f64),
            Cond::Le(19, 0.607016051968_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 529 ethusdt_15m_rules_529: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000960780049_f64),
            Cond::Le(44, -0.00540043628_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 530 ethusdt_15m_rules_530: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(61, 35.01130025_f64),
            Cond::Le(21, -0.003142104413_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 531 ethusdt_15m_rules_531: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.220922553819_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 532 ethusdt_15m_rules_532: GREEN
    (
        true,
        &[
            Cond::Le(48, 9.919091826186_f64),
            Cond::Ge(8, 0.003864538504_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 533 ethusdt_15m_rules_533: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Le(19, 0.519485662844_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 534 ethusdt_15m_rules_534: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::Ge(81, -0.001373496039_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 535 ethusdt_15m_rules_535: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.705007578106_f64),
            Cond::Ge(70, 3.241967620561_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 536 ethusdt_15m_rules_536: GREEN
    (
        true,
        &[
            Cond::Le(13, -183.059643654916_f64),
            Cond::Le(15, 0.0_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 537 ethusdt_15m_rules_537: RED
    (
        false,
        &[
            Cond::Ge(71, 96.35554424836_f64),
            Cond::Le(44, -0.000547643196_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 538 ethusdt_15m_rules_538: RED
    (
        false,
        &[
            Cond::Ge(44, 0.00186594868_f64),
            Cond::Ge(71, 97.66150155_f64),
            Cond::Ge(37, 4.0_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 539 ethusdt_15m_rules_539: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.418823304347_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 540 ethusdt_15m_rules_540: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.058231615_f64),
            Cond::Ge(30, 0.02558085724_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 541 ethusdt_15m_rules_541: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Ge(5, -0.006258679323_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 542 ethusdt_15m_rules_542: GREEN
    (
        true,
        &[
            Cond::Le(71, 8.594155904669_f64),
            Cond::Le(76, 0.000068985592_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 543 ethusdt_15m_rules_543: GREEN
    (
        true,
        &[
            Cond::Le(64, 21.766483332328_f64),
            Cond::Le(2, 0.00127885769_f64),
        ],
    ),
    // 544 ethusdt_15m_rules_544: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.679463493_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(2, 0.001522731461_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 545 ethusdt_15m_rules_545: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(63, 40.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 546 ethusdt_15m_rules_546: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Le(50, 1.586225659_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 547 ethusdt_15m_rules_547: GREEN
    (
        true,
        &[
            Cond::Le(64, 15.690633044821_f64),
            Cond::Ge(2, 0.018953226359_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 548 ethusdt_15m_rules_548: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(57, -0.01422341075_f64),
            Cond::Le(80, 0.678389517_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 549 ethusdt_15m_rules_549: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.1302923821_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Le(61, 34.24698515_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 550 ethusdt_15m_rules_550: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000120857876_f64),
            Cond::Le(7, 0.302247845896_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 551 ethusdt_15m_rules_551: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000268702787_f64),
            Cond::Le(81, -0.008392877382_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 552 ethusdt_15m_rules_552: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.210641246093_f64),
            Cond::Ge(29, 0.01508702537_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 553 ethusdt_15m_rules_553: GREEN
    (
        true,
        &[
            Cond::Le(61, 24.975510474295_f64),
            Cond::Ge(46, 39.788932118881_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 554 ethusdt_15m_rules_554: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(43, 0.01349188119_f64),
            Cond::Le(74, 0.00008848352749_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 555 ethusdt_15m_rules_555: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Ge(44, 0.000140769183_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 556 ethusdt_15m_rules_556: GREEN
    (
        true,
        &[
            Cond::Le(64, 15.690633044821_f64),
            Cond::Le(2, 0.002590558025_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 557 ethusdt_15m_rules_557: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.828421748691_f64),
            Cond::Ge(12, -20.481234551541_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 558 ethusdt_15m_rules_558: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.004437668774_f64),
            Cond::Ge(61, 74.897385339306_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 559 ethusdt_15m_rules_559: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.722439334531_f64),
            Cond::Ge(81, -0.004763212286_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 560 ethusdt_15m_rules_560: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Ge(8, -0.009912424083_f64),
            Cond::Ge(7, 0.761334494_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 561 ethusdt_15m_rules_561: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000186502931_f64),
            Cond::Le(79, -1.385238691491_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 562 ethusdt_15m_rules_562: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.016780876652_f64),
            Cond::Le(44, -0.004351170326_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 563 ethusdt_15m_rules_563: GREEN
    (
        true,
        &[
            Cond::Le(12, -163.848507305154_f64),
            Cond::Ge(76, 0.007704921362_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 564 ethusdt_15m_rules_564: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.65908861946_f64),
            Cond::In(41, &[22.0_f64]),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 565 ethusdt_15m_rules_565: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.307627154465_f64),
            Cond::Ge(45, 0.001241223785_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 566 ethusdt_15m_rules_566: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 567 ethusdt_15m_rules_567: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0015569182_f64),
            Cond::Le(62, 30.19044475_f64),
            Cond::Ge(44, -0.00105807534_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 568 ethusdt_15m_rules_568: RED
    (
        false,
        &[
            Cond::Ge(72, 98.632249177409_f64),
            Cond::Ge(74, 0.000972276508_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 569 ethusdt_15m_rules_569: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.014325177909_f64),
            Cond::Ge(74, 0.016082751957_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 570 ethusdt_15m_rules_570: GREEN
    (
        true,
        &[
            Cond::Le(72, 1.724176425078_f64),
            Cond::Le(46, 16.087074533329_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 571 ethusdt_15m_rules_571: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.359680944_f64),
            Cond::Le(2, 0.001795740443_f64),
            Cond::Le(4, -0.07487622772_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 572 ethusdt_15m_rules_572: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 573 ethusdt_15m_rules_573: RED
    (
        false,
        &[
            Cond::Ge(48, 88.952653717272_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 574 ethusdt_15m_rules_574: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(19, 0.656844883173_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 575 ethusdt_15m_rules_575: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.009495690125_f64),
            Cond::Le(2, 0.003127045792_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 576 ethusdt_15m_rules_576: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000544949204_f64),
            Cond::Ge(74, 0.004468097235_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 577 ethusdt_15m_rules_577: GREEN
    (
        true,
        &[
            Cond::Le(71, 3.170746597814_f64),
            Cond::Ge(75, 3.979483531844_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 578 ethusdt_15m_rules_578: GREEN
    (
        true,
        &[
            Cond::Le(71, 3.503757061587_f64),
            Cond::Le(1, -0.75450257826_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 579 ethusdt_15m_rules_579: GREEN
    (
        true,
        &[
            Cond::Le(71, 13.57158226389_f64),
            Cond::Le(76, 0.000034905632_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 580 ethusdt_15m_rules_580: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.022563546352_f64),
            Cond::Ge(74, 0.012910358989_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 581 ethusdt_15m_rules_581: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Le(19, 0.519485662844_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 582 ethusdt_15m_rules_582: GREEN
    (
        true,
        &[
            Cond::Le(64, 12.502131887069_f64),
            Cond::Ge(20, -0.016588506863_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 583 ethusdt_15m_rules_583: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Ge(57, -0.01042349892_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 584 ethusdt_15m_rules_584: RED
    (
        false,
        &[
            Cond::Ge(72, 96.395460340985_f64),
            Cond::Le(76, 0.000172338933_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 585 ethusdt_15m_rules_585: GREEN
    (
        true,
        &[
            Cond::Le(61, 25.790818624552_f64),
            Cond::Ge(24, -0.007460989272_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 586 ethusdt_15m_rules_586: GREEN
    (
        true,
        &[
            Cond::Le(64, 27.937723759111_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 587 ethusdt_15m_rules_587: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Le(6, 0.000139017832_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 588 ethusdt_15m_rules_588: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.60338060349_f64),
            Cond::Ge(21, -0.00660351419_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 589 ethusdt_15m_rules_589: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.55850139458_f64),
            Cond::Le(19, 0.499150169133_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 590 ethusdt_15m_rules_590: GREEN
    (
        true,
        &[
            Cond::Le(71, 4.179275745964_f64),
            Cond::Le(82, -0.024921049724_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 591 ethusdt_15m_rules_591: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.509371455114_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 592 ethusdt_15m_rules_592: GREEN
    (
        true,
        &[
            Cond::Le(72, 9.711755951452_f64),
            Cond::Ge(75, 24.524084565759_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 593 ethusdt_15m_rules_593: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(21, -0.01345743525_f64),
            Cond::Le(15, 0.1384615385_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 594 ethusdt_15m_rules_594: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.048300974027_f64),
            Cond::Le(11, -0.887448173015_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 595 ethusdt_15m_rules_595: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(2, 0.00260692919_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 596 ethusdt_15m_rules_596: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.55850139458_f64),
            Cond::Between(79, -0.481713759114_f64, -0.108072495769_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 597 ethusdt_15m_rules_597: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(12, -191.4788279_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 598 ethusdt_15m_rules_598: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Le(42, 0.00002645309807_f64),
            Cond::Ge(8, -0.007253030737_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 599 ethusdt_15m_rules_599: GREEN
    (
        true,
        &[
            Cond::Le(72, 6.753473519311_f64),
            Cond::Le(6, 0.000139017832_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 600 ethusdt_15m_rules_600: GREEN
    (
        true,
        &[
            Cond::Le(72, 5.251600440171_f64),
            Cond::Ge(55, -0.000749714289_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 601 ethusdt_15m_rules_601: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.00194583421_f64),
            Cond::Ge(82, 0.025184469595_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 602 ethusdt_15m_rules_602: GREEN
    (
        true,
        &[
            Cond::Le(72, 0.003627163843_f64),
            Cond::Le(81, -0.004875294255_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 603 ethusdt_15m_rules_603: GREEN
    (
        true,
        &[
            Cond::Le(63, 14.028360392873_f64),
            Cond::Le(77, 0.001627937385_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 604 ethusdt_15m_rules_604: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Ge(58, -0.004437668774_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 605 ethusdt_15m_rules_605: GREEN
    (
        true,
        &[
            Cond::Le(63, 11.111136496641_f64),
            Cond::Le(82, -0.024921049724_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 606 ethusdt_15m_rules_606: RED
    (
        false,
        &[
            Cond::Ge(17, 2.359838557827_f64),
            Cond::Le(20, 0.00203958547_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 607 ethusdt_15m_rules_607: RED
    (
        false,
        &[
            Cond::Ge(40, 0.767976820667_f64),
            Cond::Le(61, 41.054134762643_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 608 ethusdt_15m_rules_608: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000673519805_f64),
            Cond::Le(81, -0.011317995852_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 609 ethusdt_15m_rules_609: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.53343598_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Le(72, 16.25327588_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 610 ethusdt_15m_rules_610: RED
    (
        false,
        &[
            Cond::Ge(71, 96.022441207446_f64),
            Cond::Le(78, 0.34972482193_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 611 ethusdt_15m_rules_611: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000813794422_f64),
            Cond::Le(45, -0.007007852866_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 612 ethusdt_15m_rules_612: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.834423453642_f64),
            Cond::Le(77, 0.000714281391_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 613 ethusdt_15m_rules_613: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.371097431342_f64),
            Cond::Ge(82, 0.010852199486_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 614 ethusdt_15m_rules_614: GREEN
    (
        true,
        &[
            Cond::Le(64, 22.26951785_f64),
            Cond::Le(42, 0.0001510480269_f64),
            Cond::Ge(14, -111.0492288_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 615 ethusdt_15m_rules_615: GREEN
    (
        true,
        &[
            Cond::Le(63, 29.776688316978_f64),
            Cond::Ge(82, 0.027527019994_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 616 ethusdt_15m_rules_616: GREEN
    (
        true,
        &[
            Cond::Le(63, 11.111136496641_f64),
            Cond::Le(82, -0.024921049724_f64),
            Cond::Eq(83, 0.0_f64),
        ],
    ),
    // 617 ethusdt_15m_rules_617: GREEN
    (
        true,
        &[
            Cond::Le(64, 27.937723759111_f64),
            Cond::Le(19, 0.517302629848_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 618 ethusdt_15m_rules_618: GREEN
    (
        true,
        &[
            Cond::Le(72, 2.359680944_f64),
            Cond::Le(2, 0.001795740443_f64),
            Cond::Le(4, -0.07487622772_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 619 ethusdt_15m_rules_619: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00135922631_f64),
            Cond::Ge(29, 0.084097168244_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 620 ethusdt_15m_rules_620: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.581548268_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(28, 0.006105932389_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 621 ethusdt_15m_rules_621: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000813794422_f64),
            Cond::Le(45, -0.007007852866_f64),
            Cond::Eq(83, 1.0_f64),
        ],
    ),
    // 622 ethusdt_15m_rules_622: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.371343923784_f64),
            Cond::Ge(58, 0.00710213073_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 623 ethusdt_15m_rules_623: GREEN
    (
        true,
        &[
            Cond::Le(48, 10.831501560562_f64),
            Cond::Ge(27, 0.032597623032_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 624 ethusdt_15m_rules_624: RED
    (
        false,
        &[
            Cond::Ge(24, -0.002433279017_f64),
            Cond::Ge(0, 14.249712652214_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 625 ethusdt_15m_rules_625: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(61, 44.439233481289_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 626 ethusdt_15m_rules_626: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.058231615_f64),
            Cond::Ge(30, 0.02558085724_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 627 ethusdt_15m_rules_627: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 628 ethusdt_15m_rules_628: RED
    (
        false,
        &[
            Cond::Ge(71, 94.73247534402_f64),
            Cond::Le(19, 0.562623940981_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 629 ethusdt_15m_rules_629: GREEN
    (
        true,
        &[
            Cond::Le(64, 24.393415791832_f64),
            Cond::Le(51, -1.033363918481_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 630 ethusdt_15m_rules_630: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 631 ethusdt_15m_rules_631: RED
    (
        false,
        &[
            Cond::Le(52, 0.0_f64),
            Cond::Le(44, -0.003731388935_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 632 ethusdt_15m_rules_632: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.006837778067_f64),
            Cond::Le(63, 23.29241515_f64),
            Cond::Ge(4, 0.08661248075_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 633 ethusdt_15m_rules_633: RED
    (
        false,
        &[
            Cond::Ge(71, 97.251019949653_f64),
            Cond::Ge(74, 0.001007899334_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 634 ethusdt_15m_rules_634: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.002900258506_f64),
            Cond::Ge(82, 0.015962036922_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 635 ethusdt_15m_rules_635: GREEN
    (
        true,
        &[
            Cond::Le(73, 3.661654758_f64),
            Cond::Le(48, 15.33077261_f64),
            Cond::Ge(57, -0.05828397287_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 636 ethusdt_15m_rules_636: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(6, 0.000012687105_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 637 ethusdt_15m_rules_637: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.04489019383_f64),
            Cond::Ge(7, 0.9987638412_f64),
            Cond::Ge(13, -201.1674785_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 638 ethusdt_15m_rules_638: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.025147324264_f64),
            Cond::Ge(12, -90.37920447042_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 639 ethusdt_15m_rules_639: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.767007525744_f64),
            Cond::Ge(15, 0.396308918755_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 640 ethusdt_15m_rules_640: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(79, -1.159346030872_f64),
            Cond::Eq(83, 6.0_f64),
        ],
    ),
    // 641 ethusdt_15m_rules_641: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Ge(81, 0.005560304646_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 642 ethusdt_15m_rules_642: GREEN
    (
        true,
        &[
            Cond::Le(71, 9.266584745766_f64),
            Cond::Le(19, 0.379321903939_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 643 ethusdt_15m_rules_643: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(6, 0.000012687105_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 644 ethusdt_15m_rules_644: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(77, 0.007974367829_f64),
            Cond::Eq(83, 5.0_f64),
        ],
    ),
    // 645 ethusdt_15m_rules_645: RED
    (
        false,
        &[
            Cond::Ge(4, 1.075503944993_f64),
            Cond::Le(51, -0.822178298076_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 646 ethusdt_15m_rules_646: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(15, 0.000045742666_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 647 ethusdt_15m_rules_647: GREEN
    (
        true,
        &[
            Cond::Le(64, 15.690633044821_f64),
            Cond::Ge(2, 0.018953226359_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 648 ethusdt_15m_rules_648: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.241184302191_f64),
            Cond::Ge(5, -0.008393357935_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 649 ethusdt_15m_rules_649: RED
    (
        false,
        &[
            Cond::Ge(63, 76.287641686518_f64),
            Cond::Le(79, -0.939474731553_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 650 ethusdt_15m_rules_650: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001086987122_f64),
            Cond::Ge(45, 0.001725415755_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 651 ethusdt_15m_rules_651: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.002900258506_f64),
            Cond::Ge(82, 0.015962036922_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 652 ethusdt_15m_rules_652: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Ge(33, 1.0_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 653 ethusdt_15m_rules_653: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.116199896442_f64),
            Cond::Ge(29, 0.017910613907_f64),
            Cond::Eq(67, 1.0_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 654 ethusdt_15m_rules_654: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(45, -0.010354562484_f64),
            Cond::Eq(83, 2.0_f64),
        ],
    ),
    // 655 ethusdt_15m_rules_655: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(2, 0.00260692919_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 656 ethusdt_15m_rules_656: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000717425385_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 657 ethusdt_15m_rules_657: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.00845307_f64),
            Cond::Ge(24, -0.01631766978_f64),
            Cond::Le(30, 0.0015569182_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 658 ethusdt_15m_rules_658: RED
    (
        false,
        &[
            Cond::Ge(63, 76.287641686518_f64),
            Cond::Le(79, -1.094780688725_f64),
            Cond::Eq(69, 1.0_f64),
        ],
    ),
    // 659 ethusdt_15m_rules_659: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Between(79, -0.470588798961_f64, -0.128994916769_f64),
            Cond::Eq(83, 4.0_f64),
        ],
    ),
    // 660 ethusdt_15m_rules_660: RED
    (
        false,
        &[
            Cond::Ge(71, 96.022441207446_f64),
            Cond::Ge(42, 0.006878572146_f64),
            Cond::Eq(83, 3.0_f64),
        ],
    ),
    // 661 ethusdt_15m_rules_661: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.190740935_f64),
            Cond::Ge(39, 0.7392377051_f64),
            Cond::Ge(28, 0.01135054861_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 662 ethusdt_15m_rules_662: GREEN
    (
        true,
        &[
            Cond::Le(63, 10.880992792283_f64),
            Cond::Ge(44, -0.001560612463_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 663 ethusdt_15m_rules_663: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.880226526547_f64),
            Cond::Ge(56, -0.002710522059_f64),
        ],
    ),
];

pub struct EthM15Rules663 {
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

impl EthM15Rules663 {
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

impl Strategy for EthM15Rules663 {
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
