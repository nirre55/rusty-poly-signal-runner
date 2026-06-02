use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "five_year_70pct_eth_1h_rules_632_min_votes_1";

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
        return None;
    }
    Some((c.high - c.low) / atr)
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
            return None;
        }
        Some(100.0 - 100.0 / (1.0 + gain / loss))
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
            return None;
        }
        Some(self.line? / close)
    }

    fn hist_pct(&self, close: f64) -> Option<f64> {
        if close == 0.0 {
            return None;
        }
        Some(self.hist? / close)
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
        return None;
    }
    Some(close / min_l - 1.0)
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
        return None;
    }
    Some(close / max_h - 1.0)
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
        return None;
    }
    Some(buf.back()?.volume / sma)
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
    let band = 4.0 * s;
    if band == 0.0 {
        return None;
    }
    Some((v[0] - (m - 2.0 * s)) / band)
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
        return None;
    }
    Some(current / shifted - 1.0)
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

fn reclaim_mid(buf: &VecDeque<Candle>, n: usize) -> Option<f64> {
    if buf.len() < n + 1 {
        return Some(0.0);
    }
    let cur_idx = buf.len() - 1;
    let start = cur_idx - n;
    let max_h = buf
        .iter()
        .skip(start)
        .take(n)
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_l = buf
        .iter()
        .skip(start)
        .take(n)
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    let mid = (max_h + min_l) / 2.0;
    Some(if buf.back()?.close > mid { 1.0 } else { 0.0 })
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

// Heikin-Ashi state — persists ha_open/close across candles
struct HaState {
    prev_open: Option<f64>,
    prev_close: Option<f64>,
    // cached current-candle values after update()
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

// Feature index:
// 0=absorption_down
// 1=absorption_up
// 2=atr14_pct
// 3=bb_pctb
// 4=body
// 5=body_abs_pct
// 6=body_ratio
// 7=body_sum3
// 8=body_sum6
// 9=body_sum12
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
// 23=donch_low12
// 24=donch_low24
// 25=failed_high12
// 26=failed_high24
// 27=failed_low12
// 28=failed_low24
// 29=flip_count6
// 30=flip_count12
// 31=green_count3
// 32=green_count6
// 33=green_streak
// 34=ha_body
// 35=ha_body_ratio
// 36=ha_close_position
// 37=hour
// 38=lower_wick
// 39=lower_wick_body
// 40=macd_hist_pct
// 41=macd_pct
// 42=mfi8
// 43=mfi14
// 44=minute_of_day
// 45=range_atr14
// 46=range_pct_z24
// 47=reclaim_mid12
// 48=red_count3
// 49=red_streak
// 50=ret1
// 51=ret3
// 52=ret6
// 53=ret12
// 54=rsi7
// 55=rsi8
// 56=rsi14
// 57=signed_volume_ratio20
// 58=stoch_k12
// 59=stoch_k24
// 60=upper_wick
// 61=upper_wick_body
// 62=volume_body_efficiency
// 63=volume_range_efficiency
// 64=volume_ratio20
// 65=volume_z24
// 66=vwap_slope24
// 67=vwap_slope72
// 68=weekday
// 69=red_count6
// 70=same_color_ratio12
// 71=session_overlap_london_us
struct Feats {
    f: [Option<f64>; 72],
}

impl Feats {
    fn get(&self, id: u8) -> Option<f64> {
        self.f[id as usize]
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_feats(
    buf: &VecDeque<Candle>,
    rsi7: &PyRsiState,
    rsi8: &PyRsiState,
    rsi14: &PyRsiState,
    atr14_ewm: &PyAtrEwmState,
    macd: &PyMacdState,
    ha: &HaState,
) -> Feats {
    let cur = match buf.back() {
        Some(c) => c,
        None => return Feats { f: [None; 72] },
    };
    let close = cur.close;
    let range = cur.high - cur.low;
    let body = if close == 0.0 {
        None
    } else {
        Some((cur.close - cur.open) / close)
    };
    let body_s = (cur.close - cur.open).abs();
    let body_r = if range == 0.0 {
        None
    } else {
        Some(body_s / range)
    };
    let cp = if range == 0.0 {
        None
    } else {
        Some((close - cur.low) / range)
    };
    let lw = if close == 0.0 {
        None
    } else {
        Some((cur.open.min(cur.close) - cur.low) / close)
    };
    let uw = if close == 0.0 {
        None
    } else {
        Some((cur.high - cur.open.max(cur.close)) / close)
    };
    let lwb = if body_s < 1e-10 {
        None
    } else {
        Some((cur.open.min(cur.close) - cur.low) / body_s)
    };
    let uwb = if body_s < 1e-10 {
        None
    } else {
        Some((cur.high - cur.open.max(cur.close)) / body_s)
    };
    let hour = cur.close_time.hour() as f64;
    let wday = cur.close_time.weekday().num_days_from_monday() as f64;

    let mut f: [Option<f64>; 72] = [None; 72];
    f[0] = absorption(buf, false);
    f[1] = absorption(buf, true);
    f[2] = atr_pct_sma(buf, 14, close);
    f[3] = bb_pctb(buf);
    f[4] = body;
    f[5] = if close == 0.0 {
        None
    } else {
        Some(body_s / close)
    };
    f[6] = body_r;
    f[7] = body_sum(buf, 3);
    f[8] = body_sum(buf, 6);
    f[9] = body_sum(buf, 12);
    f[10] = breakout_energy_f(buf);
    f[11] = cci_n(buf, 12);
    f[12] = cci_n(buf, 24);
    f[13] = cp;
    f[14] = close_z(buf, 12);
    f[15] = close_z(buf, 24);
    f[16] = compression_ratio(buf, 12, 72);
    f[17] = dist_sma(buf, 12, close);
    f[18] = dist_sma(buf, 24, close);
    f[19] = dist_vwap(buf, 24, close);
    f[20] = dist_vwap(buf, 72, close);
    f[21] = donch_high(buf, 12, close);
    f[22] = donch_high(buf, 24, close);
    f[23] = donch_low(buf, 12, close);
    f[24] = donch_low(buf, 24, close);
    f[25] = failed_high(buf, 12);
    f[26] = failed_high(buf, 24);
    f[27] = failed_low(buf, 12);
    f[28] = failed_low(buf, 24);
    f[29] = flip_count(buf, 6);
    f[30] = flip_count(buf, 12);
    f[31] = count_color(buf, 3, true);
    f[32] = count_color(buf, 6, true);
    f[33] = Some(green_streak(buf));
    f[34] = ha.ha_body;
    f[35] = ha.ha_body_ratio;
    f[36] = ha.ha_close_pos;
    f[37] = Some(hour);
    f[38] = lw;
    f[39] = lwb;
    f[40] = macd.hist_pct(close);
    f[41] = macd.line_pct(close);
    f[42] = mfi_n(buf, 8);
    f[43] = mfi_n(buf, 14);
    let minute_of_day = hour * 60.0 + cur.close_time.minute() as f64;
    f[44] = Some(minute_of_day);
    f[45] = range_atr14(buf, atr14_ewm.raw());
    f[46] = range_pct_z(buf, 24);
    f[47] = reclaim_mid(buf, 12);
    f[48] = count_color(buf, 3, false);
    f[49] = Some(red_streak(buf));
    f[50] = ret_n(buf, 1);
    f[51] = ret_n(buf, 3);
    f[52] = ret_n(buf, 6);
    f[53] = ret_n(buf, 12);
    f[54] = rsi7.get();
    f[55] = rsi8.get();
    f[56] = rsi14.get();
    f[57] = signed_vol_ratio(buf, 20);
    f[58] = stoch_k(buf, 12, close);
    f[59] = stoch_k(buf, 24, close);
    f[60] = uw;
    f[61] = uwb;
    f[62] = vol_body_eff(buf);
    f[63] = vol_range_eff(buf);
    f[64] = volume_ratio(buf, 20);
    f[65] = vol_z(buf, 24);
    f[66] = vwap_slope(buf, 24);
    f[67] = vwap_slope(buf, 72);
    f[68] = Some(wday);
    f[69] = count_color(buf, 6, false);
    f[70] = same_color_ratio(buf, 12);
    f[71] = Some(session_overlap_london_us(minute_of_day));
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
        false,
        &[
            (42, 0, 90.971006267369_f64, 0_f64),
            (63, 0, 0.028499754838_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 27.937723759111_f64, 0_f64),
            (63, 0, 0.046027537425_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.0494856271_f64, 0_f64),
            (8, 3, -0.002440276996_f64, 0.002598746782_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.001841392139_f64, 0_f64),
            (23, 0, 0.095760974066_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.977917225442_f64, 0_f64),
            (67, 0, 0.039861770193_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (6, 1, 0.045506715202_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.004419256154_f64, 0_f64),
            (0, 1, -41.794890628583_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (43, 0, 66.675796827867_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (2, 0, 0.025050223404_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(48, 1, 0_f64, 0_f64), (52, 1, -0.043364569302_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 83.706222662312_f64, 0_f64),
            (23, 1, 0.004099778821_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.0494856271_f64, 0_f64),
            (61, 1, 0.018666518665_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 98.632249177409_f64, 0_f64),
            (0, 0, 0.941851345832_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 87.21365662096_f64, 0_f64),
            (2, 1, 0.005354469061_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.001716222973_f64, 0_f64),
            (0, 0, 1.293775146209_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (21, 1, -0.103167040398_f64, 0_f64),
            (65, 0, 3.746261866779_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.561260986784_f64, 0_f64),
            (61, 0, 0.392199523066_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (23, 0, 0.095760974066_f64, 0_f64),
            (61, 1, 0.018666518665_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.025147324264_f64, 0_f64),
            (11, 0, -72.536398228737_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 82.454573967079_f64, 0_f64),
            (23, 1, 0.017971631655_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.005185093586_f64, 0_f64),
            (2, 1, 0.0031654888_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (13, 0, 0.968894736842_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (16, 1, 0.693883935668_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(19, 0, 0.058487164589_f64, 0_f64), (37, 2, 15_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 2.062588143616_f64, 0_f64),
            (43, 1, 9.622580642198_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 2.062588143616_f64, 0_f64),
            (66, 1, -0.028555905248_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (57, 1, -4.211939583565_f64, 0_f64),
            (5, 1, 0.000764123624_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.100215016184_f64, 0_f64),
            (52, 0, 0.065040574296_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 12.039352357177_f64, 0_f64),
            (12, 0, -111.395699072825_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 26.442162092557_f64, 0_f64),
            (43, 0, 60.696816665125_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (18, 0, 0.003178082774_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.185612973637_f64, 0_f64),
            (64, 1, 0.604709102916_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (17, 1, -0.042487639427_f64, 0_f64),
            (41, 0, 0.002976402628_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.001171005877_f64, 0_f64),
            (14, 0, -1.343857006248_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(42, 1, 0_f64, 0_f64), (67, 0, 0.018190418356_f64, 0_f64)],
    ),
    (
        true,
        &[
            (55, 1, 27.937723759111_f64, 0_f64),
            (66, 0, 0.014461038157_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 67.899626426361_f64, 0_f64),
            (43, 1, 33.487434256627_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(23, 0, 0.113311886667_f64, 0_f64), (37, 3, 8_f64, 11_f64)],
    ),
    (
        true,
        &[(58, 1, 3.503757061587_f64, 0_f64), (37, 2, 20_f64, 0_f64)],
    ),
    (
        false,
        &[
            (3, 0, 1.02132756984_f64, 0_f64),
            (60, 1, 0.000002944078_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 81.688139538765_f64, 0_f64),
            (0, 0, 2.347119575634_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 28.488182057264_f64, 0_f64),
            (43, 0, 60.696816665125_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(48, 1, 0_f64, 0_f64), (17, 1, -0.022574729712_f64, 0_f64)],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (0, 0, 2.347119575634_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 3.503757061587_f64, 0_f64),
            (40, 1, -0.008589755612_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (23, 0, 0.05741400062_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (61, 0, 10.072336134454_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.100215016184_f64, 0_f64),
            (52, 0, 0.053650017546_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(20, 0, 0.098883002663_f64, 0_f64), (37, 2, 0_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 3.503757061587_f64, 0_f64),
            (2, 1, 0.003674435615_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 5.418823304347_f64, 0_f64),
            (16, 1, 0.517302629848_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.286381306679_f64, 0_f64),
            (4, 0, -0.008393357935_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.087923069654_f64, 0_f64),
            (62, 0, 0.02427041781_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 12.039352357177_f64, 0_f64),
            (12, 0, -123.693327474426_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 12.370381786246_f64, 0_f64),
            (10, 1, -0.134109392588_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 5.418823304347_f64, 0_f64),
            (53, 1, -0.090923229416_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 81.688139538765_f64, 0_f64),
            (0, 0, 1.293775146209_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.0494856271_f64, 0_f64),
            (10, 1, -0.21429048384_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (38, 0, 0.010204835097_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000717425385_f64, 0_f64),
            (23, 0, 0.078922113522_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 98.632249177409_f64, 0_f64),
            (38, 0, 0.006878572146_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.060395753965_f64, 0_f64),
            (45, 1, 0.470794215282_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (38, 0, 0.008326115895_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.755383566354_f64, 0_f64),
            (0, 1, -7.332863959192_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (19, 1, -0.059419189352_f64, 0_f64),
            (36, 0, 0.555858847899_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (2, 1, 0.002837437065_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.185612973637_f64, 0_f64),
            (45, 1, 0.590930557954_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.365308166414_f64, 0_f64),
            (5, 0, 0.027907557487_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -3.241184302191_f64, 0_f64),
            (4, 0, -0.008393357935_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (17, 1, -0.051248070032_f64, 0_f64),
            (22, 0, -0.083808938102_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000643444193_f64, 0_f64),
            (0, 0, 0.941851345832_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.025827454359_f64, 0_f64),
            (52, 0, 0.035969421722_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(52, 1, -0.065293293674_f64, 0_f64), (30, 1, 2_f64, 0_f64)],
    ),
    (
        false,
        &[(17, 0, 0.039616991761_f64, 0_f64), (37, 2, 11_f64, 0_f64)],
    ),
    (
        false,
        &[
            (17, 0, 0.046722426273_f64, 0_f64),
            (52, 1, 0.041957845164_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(22, 0, -0.001113311961_f64, 0_f64), (37, 2, 6_f64, 0_f64)],
    ),
    (
        false,
        &[
            (19, 0, 0.058487164589_f64, 0_f64),
            (70, 0, 0.833333333333_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 5.418823304347_f64, 0_f64),
            (10, 1, -0.422866719649_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.77000366238_f64, 0_f64),
            (10, 1, -0.422866719649_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 5_f64, 0_f64), (67, 0, 0.033759209683_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 13.57158226389_f64, 0_f64),
            (2, 1, 0.001924913661_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.680586695736_f64, 0_f64),
            (42, 0, 70.426155549544_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 84.181207583347_f64, 0_f64),
            (18, 1, 0.012838900259_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(15, 1, -2.404114354323_f64, 0_f64), (29, 0, 6_f64, 0_f64)],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (20, 0, 0.040068069939_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (45, 1, 0.952041479441_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.008074882103_f64, 0_f64),
            (66, 0, 0.011708309601_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.365308166414_f64, 0_f64),
            (38, 0, 0.018854227163_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(21, 0, -0.000717425385_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        true,
        &[(24, 1, 0.00254012653_f64, 0_f64), (37, 2, 20_f64, 0_f64)],
    ),
    (
        false,
        &[(21, 0, -0.000399711895_f64, 0_f64), (37, 2, 3_f64, 0_f64)],
    ),
    (
        false,
        &[
            (19, 0, 0.0494856271_f64, 0_f64),
            (43, 1, 55.226633333302_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 97.75223967825_f64, 0_f64),
            (40, 1, -0.000433596303_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (15, 0, -1.412432258616_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000643444193_f64, 0_f64),
            (61, 0, 0.151279003962_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (51, 1, 0.001867688078_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.008177319388_f64, 0_f64),
            (17, 0, 0.014620999612_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (53, 1, -0.008350840003_f64, 0_f64)],
    ),
    (
        false,
        &[
            (54, 0, 87.21365662096_f64, 0_f64),
            (46, 1, -0.770081671424_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 18.578805575288_f64, 0_f64),
            (0, 0, 13.915733334115_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 5.418823304347_f64, 0_f64),
            (63, 1, 0.002890012136_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(21, 1, -0.103167040398_f64, 0_f64), (71, 0, 1_f64, 0_f64)],
    ),
    (
        true,
        &[(14, 1, -2.742641598641_f64, 0_f64), (37, 2, 20_f64, 0_f64)],
    ),
    (
        true,
        &[
            (15, 1, -2.126060052744_f64, 0_f64),
            (55, 0, 37.066174537088_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.087923069654_f64, 0_f64),
            (67, 1, -0.010375267305_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(15, 1, -2.60338060349_f64, 0_f64), (30, 0, 10_f64, 0_f64)],
    ),
    (
        true,
        &[
            (15, 1, -2.60338060349_f64, 0_f64),
            (35, 1, 0.238168112276_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 4_f64, 0_f64), (5, 1, 0.000026576198_f64, 0_f64)],
    ),
    (
        true,
        &[
            (13, 1, 0.065876434269_f64, 0_f64),
            (57, 0, 0.517790792137_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.091259020395_f64, 0_f64),
            (43, 0, 60.696816665125_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.004419256154_f64, 0_f64),
            (62, 0, 0.02427041781_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (56, 0, 79.132602591729_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (40, 0, 0.008650326859_f64, 0_f64),
            (2, 1, 0.014794336648_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (0, 1, -22.999073893274_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 82.454573967079_f64, 0_f64),
            (62, 0, 0.016903712558_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 82.454573967079_f64, 0_f64),
            (16, 1, 0.639508691164_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(7, 0, 0.044598294078_f64, 0_f64), (37, 2, 21_f64, 0_f64)],
    ),
    (false, &[(48, 1, 0_f64, 0_f64), (33, 1, 2_f64, 0_f64)]),
    (
        true,
        &[
            (36, 1, 0.407243075194_f64, 0_f64),
            (45, 0, 3.519022231122_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 3.695232436063_f64, 0_f64),
            (66, 1, -0.028555905248_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(52, 1, -0.065293293674_f64, 0_f64), (37, 2, 20_f64, 0_f64)],
    ),
    (
        false,
        &[
            (8, 0, 0.061916514734_f64, 0_f64),
            (10, 1, -0.070623222976_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 87.21365662096_f64, 0_f64),
            (2, 1, 0.005976537031_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000399711895_f64, 0_f64),
            (41, 1, -0.007157682445_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (52, 3, -0.002369659403_f64, 0.0026622929_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000717425385_f64, 0_f64),
            (23, 1, 0.004099778821_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.060395753965_f64, 0_f64),
            (63, 1, 0.002557809727_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (52, 1, -0.065293293674_f64, 0_f64),
            (66, 0, 0.001518218635_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (10, 1, -0.422866719649_f64, 0_f64)],
    ),
    (
        false,
        &[(8, 0, 0.061916514734_f64, 0_f64), (37, 2, 15_f64, 0_f64)],
    ),
    (
        false,
        &[
            (8, 0, 0.061916514734_f64, 0_f64),
            (66, 1, -0.018692230357_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.001841392139_f64, 0_f64),
            (62, 1, 0.000132092228_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (17, 0, 0.039616991761_f64, 0_f64),
            (0, 1, -1.973169584074_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 18.578805575288_f64, 0_f64),
            (34, 3, -0.003978261481_f64, 0.004053759322_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.001113311961_f64, 0_f64),
            (10, 1, -0.422866719649_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.684471745776_f64, 0_f64),
            (6, 1, 0.264860593835_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 16.441111837773_f64, 0_f64),
            (56, 0, 32.751338756074_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 22.509371455114_f64, 0_f64),
            (10, 1, -0.422866719649_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.003294721515_f64, 0_f64),
            (5, 0, 0.034547716221_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.742641598641_f64, 0_f64),
            (43, 0, 55.226633333302_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008323342559_f64, 0_f64),
            (51, 0, 0.046339158287_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 13.599250472473_f64, 0_f64),
            (42, 0, 70.426155549544_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(42, 0, 92.869319882182_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        false,
        &[
            (19, 0, 0.041798655327_f64, 0_f64),
            (60, 1, 0.000007437052_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 4_f64, 0_f64), (19, 1, -0.022743054412_f64, 0_f64)],
    ),
    (
        true,
        &[
            (14, 1, -2.187038250278_f64, 0_f64),
            (5, 1, 0.002236733024_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.006290713995_f64, 0_f64),
            (64, 1, 0.283114920997_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.006290713995_f64, 0_f64),
            (46, 1, -1.513028289708_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.404114354323_f64, 0_f64),
            (67, 0, 0.029695500337_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 5_f64, 0_f64), (6, 1, 0.009353487116_f64, 0_f64)],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (66, 0, 0.014461038157_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(40, 0, 0.008650326859_f64, 0_f64), (37, 2, 3_f64, 0_f64)],
    ),
    (
        true,
        &[
            (50, 1, -0.011045001747_f64, 0_f64),
            (12, 0, 178.957313232822_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 17.361256695854_f64, 0_f64),
            (35, 1, 0.008265122723_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (17, 1, -0.042487639427_f64, 0_f64),
            (5, 1, 0.000506599686_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.160811012751_f64, 0_f64),
            (53, 0, -0.020308471435_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.160811012751_f64, 0_f64),
            (1, 1, -0.75450257826_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (16, 1, 0.400893813069_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (57, 0, 1.504870560108_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 29.828518892658_f64, 0_f64),
            (16, 1, 0.487954125812_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 6_f64, 0_f64), (10, 1, -0.134109392588_f64, 0_f64)],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (38, 0, 0.010204835097_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 27.937723759111_f64, 0_f64),
            (16, 1, 0.517302629848_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 27.937723759111_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(48, 1, 0_f64, 0_f64), (21, 1, -0.070358681217_f64, 0_f64)],
    ),
    (
        true,
        &[
            (59, 1, 13.599250472473_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (5, 3, 0.002236733024_f64, 0.003968588912_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.001841392139_f64, 0_f64),
            (10, 1, -0.422866719649_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -3.241184302191_f64, 0_f64),
            (67, 1, -0.025444647829_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 98.683285404721_f64, 0_f64),
            (63, 1, 0.002890012136_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000717425385_f64, 0_f64),
            (43, 1, 33.487434256627_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(59, 0, 98.632249177409_f64, 0_f64), (37, 2, 0_f64, 0_f64)],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (42, 1, 70.426155549544_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (66, 3, -0.003184483329_f64, 0.003367010176_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 3.695232436063_f64, 0_f64),
            (1, 0, 3.927244001484_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.025827454359_f64, 0_f64),
            (16, 0, 1.995743208991_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.022441207446_f64, 0_f64),
            (56, 1, 47.277822488034_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(52, 1, -0.065293293674_f64, 0_f64), (37, 2, 22_f64, 0_f64)],
    ),
    (
        true,
        &[
            (52, 1, -0.065293293674_f64, 0_f64),
            (9, 0, -0.042951672527_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(19, 1, -0.059419189352_f64, 0_f64), (37, 2, 11_f64, 0_f64)],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (13, 1, 0.219060225017_f64, 0_f64)],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (43, 1, 29.891000233355_f64, 0_f64)],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (63, 0, 0.021404310273_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 3.503757061587_f64, 0_f64),
            (1, 1, -0.75450257826_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (8, 0, 0.061916514734_f64, 0_f64),
            (64, 1, 0.393261081156_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (17, 0, 0.039616991761_f64, 0_f64),
            (42, 1, 56.429411536562_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 18.578805575288_f64, 0_f64),
            (35, 1, 0.31888152245_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.058487164589_f64, 0_f64),
            (16, 1, 0.821959450341_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.127057896226_f64, 0_f64),
            (63, 0, 0.024659827828_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(65, 0, 4.082365664208_f64, 0_f64), (37, 2, 19_f64, 0_f64)],
    ),
    (
        true,
        &[
            (59, 1, 10.250078027922_f64, 0_f64),
            (43, 0, 60.696816665125_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.286381306679_f64, 0_f64),
            (56, 0, 37.712563201862_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.742641598641_f64, 0_f64),
            (46, 1, 0.609778900626_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (62, 0, 0.014296584568_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.869319882182_f64, 0_f64),
            (17, 1, 0.004404610817_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.971006267369_f64, 0_f64),
            (66, 1, -0.010067309537_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.041798655327_f64, 0_f64),
            (67, 1, -0.059176708791_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(55, 1, 12.039352357177_f64, 0_f64), (37, 2, 3_f64, 0_f64)],
    ),
    (
        true,
        &[
            (55, 1, 23.997196098696_f64, 0_f64),
            (1, 1, -21.606633337189_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 3_f64, 0_f64), (35, 1, 0.008265122723_f64, 0_f64)],
    ),
    (
        true,
        &[
            (14, 1, -2.187038250278_f64, 0_f64),
            (61, 0, 1.27376961958_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 26.442162092557_f64, 0_f64),
            (2, 1, 0.002316019964_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.404114354323_f64, 0_f64),
            (40, 0, 0.000411233834_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (61, 0, 0.151279003962_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.02132756984_f64, 0_f64),
            (65, 1, -0.82264325452_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.551972381464_f64, 0_f64),
            (4, 1, -0.007174703991_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.011045001747_f64, 0_f64),
            (64, 1, 0.311970906529_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (60, 0, 0.010478551636_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (16, 1, 0.562623940981_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(24, 0, 0.154339127652_f64, 0_f64), (37, 2, 3_f64, 0_f64)],
    ),
    (
        true,
        &[
            (55, 1, 29.828518892658_f64, 0_f64),
            (64, 1, 0.247637271351_f64, 0_f64),
        ],
    ),
    (true, &[(42, 1, 0_f64, 0_f64), (37, 2, 2_f64, 0_f64)]),
    (
        true,
        &[
            (11, 1, -227.797658840935_f64, 0_f64),
            (36, 0, 0.609974251707_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.379989986507_f64, 0_f64),
            (17, 0, 0.027303602215_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (2, 1, 0.002837437065_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 20.566606160548_f64, 0_f64),
            (35, 1, 0.004133007456_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.219060225017_f64, 0_f64),
            (0, 0, 28.212125507464_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 25.103448275861_f64, 0_f64),
            (43, 0, 79.69050284032_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (43, 0, 60.696816665125_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.022441207446_f64, 0_f64),
            (64, 1, 0.34972482193_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (61, 0, 17.001463414648_f64, 0_f64)],
    ),
    (
        false,
        &[
            (17, 0, 0.039616991761_f64, 0_f64),
            (46, 1, -0.864407170253_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 87.21365662096_f64, 0_f64),
            (66, 3, -0.003184483329_f64, 0.003367010176_f64),
        ],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (18, 0, 0.032891842078_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.041798655327_f64, 0_f64),
            (0, 0, 6.734015313961_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.005068052866_f64, 0_f64),
            (2, 0, 0.022335792029_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(23, 0, 0.113311886667_f64, 0_f64), (68, 2, 3_f64, 0_f64)],
    ),
    (
        true,
        &[
            (3, 1, -0.025147324264_f64, 0_f64),
            (11, 0, -90.37920447042_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (53, 1, -0.041498541944_f64, 0_f64)],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (38, 0, 0.006878572146_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (18, 0, 0.054731271045_f64, 0_f64),
            (1, 0, 6.159740467929_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000717425385_f64, 0_f64),
            (17, 1, 0.002014123904_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (17, 1, -0.051248070032_f64, 0_f64),
            (34, 0, -0.011986514488_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (57, 1, -1.574676055078_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 94.73247534402_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (34, 0, 0.007443274853_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 5_f64, 0_f64), (23, 0, 0.043161981738_f64, 0_f64)],
    ),
    (
        true,
        &[(65, 0, 4.082365664208_f64, 0_f64), (37, 3, 4_f64, 7_f64)],
    ),
    (
        true,
        &[
            (15, 1, -2.126060052744_f64, 0_f64),
            (67, 0, 0.033759209683_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(24, 1, 0.001171005877_f64, 0_f64), (37, 2, 16_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.365308166414_f64, 0_f64),
            (16, 1, 0.400893813069_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (2, 1, 0.0031654888_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 78.115236912005_f64, 0_f64),
            (2, 1, 0.002837437065_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(40, 0, 0.007181184238_f64, 0_f64), (37, 2, 23_f64, 0_f64)],
    ),
    (
        false,
        &[(17, 0, 0.046722426273_f64, 0_f64), (68, 2, 3_f64, 0_f64)],
    ),
    (
        false,
        &[
            (42, 0, 92.869319882182_f64, 0_f64),
            (36, 1, 0.407243075194_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 23.997196098696_f64, 0_f64),
            (63, 0, 0.032227797354_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (24, 0, 0.069815156187_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 29.828518892658_f64, 0_f64),
            (16, 1, 0.517302629848_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (1, 0, 1.161740673548_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(21, 0, -0.000399711895_f64, 0_f64), (37, 2, 5_f64, 0_f64)],
    ),
    (
        false,
        &[(58, 0, 98.683285404721_f64, 0_f64), (33, 0, 6_f64, 0_f64)],
    ),
    (
        false,
        &[
            (19, 0, 0.0494856271_f64, 0_f64),
            (2, 1, 0.011727397383_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (2, 0, 0.025050223404_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (46, 1, -1.129440954302_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(17, 0, 0.039616991761_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        false,
        &[
            (58, 0, 94.73247534402_f64, 0_f64),
            (1, 0, 0.834297868579_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.001538919285_f64, 0_f64),
            (67, 0, 0.025184469595_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 4_f64, 0_f64), (65, 1, -1.341363982825_f64, 0_f64)],
    ),
    (
        true,
        &[
            (40, 1, -0.008589755612_f64, 0_f64),
            (23, 1, 0.004099778821_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.0023840774_f64, 0_f64),
            (41, 0, 0.02102307851_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(43, 0, 90.605260301048_f64, 0_f64), (37, 2, 1_f64, 0_f64)],
    ),
    (
        false,
        &[
            (58, 0, 92.430123495695_f64, 0_f64),
            (64, 1, 0.283114920997_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.004197919734_f64, 0_f64),
            (45, 1, 0.329592884841_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (1, 1, -39.147158960848_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (17, 0, 0.014620999612_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.840110258471_f64, 0_f64),
            (1, 1, -10.991762317629_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.840110258471_f64, 0_f64),
            (35, 1, 0.040104438977_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (64, 1, 0.416983292711_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.379989986507_f64, 0_f64),
            (52, 0, 0.035969421722_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.002650174002_f64, 0_f64),
            (45, 0, 3.036678151198_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.0494856271_f64, 0_f64),
            (6, 1, 0.073463020111_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(58, 0, 94.73247534402_f64, 0_f64), (33, 1, 0_f64, 0_f64)],
    ),
    (
        true,
        &[(56, 1, 21.149731261368_f64, 0_f64), (37, 2, 16_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.00194583421_f64, 0_f64),
            (67, 0, 0.025184469595_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.404114354323_f64, 0_f64),
            (7, 0, -0.003738333088_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (64, 1, 0.468464018164_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (6, 3, 0.264860593835_f64, 0.58517531194_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (46, 3, -0.614786474045_f64, 0.207539773281_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.060395753965_f64, 0_f64),
            (60, 0, 0.003977366118_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.060395753965_f64, 0_f64),
            (63, 1, 0.002890012136_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (43, 1, 39.31642972534_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.365308166414_f64, 0_f64),
            (2, 1, 0.001924913661_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(58, 0, 98.683285404721_f64, 0_f64), (37, 2, 3_f64, 0_f64)],
    ),
    (
        false,
        &[
            (18, 0, 0.066229680906_f64, 0_f64),
            (66, 3, -0.005669694246_f64, 0.006070412698_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.008177319388_f64, 0_f64),
            (63, 0, 0.028499754838_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(56, 1, 18.578805575288_f64, 0_f64), (37, 2, 9_f64, 0_f64)],
    ),
    (
        true,
        &[(56, 1, 18.578805575288_f64, 0_f64), (68, 2, 2_f64, 0_f64)],
    ),
    (
        true,
        &[
            (21, 1, -0.103167040398_f64, 0_f64),
            (67, 3, -0.005880312126_f64, 0.006771478285_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.561260986784_f64, 0_f64),
            (3, 0, 0.302583639175_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(42, 1, 5.127057896226_f64, 0_f64), (37, 2, 7_f64, 0_f64)],
    ),
    (
        true,
        &[
            (54, 1, 16.441111837773_f64, 0_f64),
            (65, 1, -0.478087109912_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.100215016184_f64, 0_f64),
            (34, 0, 0.021538994561_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 12.039352357177_f64, 0_f64),
            (40, 0, -0.002122380767_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (19, 1, -0.049413631372_f64, 0_f64),
            (0, 1, -2.590382463299_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.005506890184_f64, 0_f64),
            (42, 0, 79.537380124647_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.025147324264_f64, 0_f64),
            (0, 1, -1.973169584074_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.02132756984_f64, 0_f64),
            (51, 3, -0.001576545149_f64, 0.001867688078_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (65, 1, -1.457487435051_f64, 0_f64)],
    ),
    (
        true,
        &[
            (50, 1, -0.011045001747_f64, 0_f64),
            (35, 1, 0.008265122723_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.160811012751_f64, 0_f64),
            (39, 0, 27.144615384633_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.160811012751_f64, 0_f64),
            (51, 0, -0.003698886648_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.160811012751_f64, 0_f64),
            (64, 1, 0.697599259542_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (12, 0, 124.413127609854_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.60245942582_f64, 0_f64),
            (42, 1, 29.338239708836_f64, 0_f64),
        ],
    ),
    (true, &[(42, 1, 0_f64, 0_f64), (30, 0, 9_f64, 0_f64)]),
    (true, &[(49, 0, 6_f64, 0_f64), (37, 2, 7_f64, 0_f64)]),
    (
        false,
        &[(7, 0, 0.044598294078_f64, 0_f64), (37, 2, 23_f64, 0_f64)],
    ),
    (
        false,
        &[(21, 0, -0.000399711895_f64, 0_f64), (37, 3, 0_f64, 3_f64)],
    ),
    (
        false,
        &[(20, 0, 0.098883002663_f64, 0_f64), (44, 1, 60_f64, 0_f64)],
    ),
    (
        false,
        &[
            (58, 0, 94.73247534402_f64, 0_f64),
            (64, 1, 0.34972482193_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.869319882182_f64, 0_f64),
            (38, 0, 0.006878572146_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.022441207446_f64, 0_f64),
            (38, 0, 0.006878572146_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(24, 1, 0.00254012653_f64, 0_f64), (37, 2, 8_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 7.081893123604_f64, 0_f64),
            (46, 1, -0.975669526392_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 7.366825932134_f64, 0_f64),
            (66, 0, 0.007976878246_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 22.509371455114_f64, 0_f64),
            (41, 0, 0.002976402628_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (1, 1, -1.127098909018_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.60338060349_f64, 0_f64),
            (18, 0, -0.00660351419_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.869319882182_f64, 0_f64),
            (60, 1, 0.000272339529_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.091259020395_f64, 0_f64),
            (20, 0, 0.023762891074_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -227.797658840935_f64, 0_f64),
            (67, 0, 0.018190418356_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 79.132602591729_f64, 0_f64),
            (10, 1, -0.134109392588_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (5, 0, 0.034547716221_f64, 0_f64),
            (63, 3, 0.008266567424_f64, 0.011166047019_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.187038250278_f64, 0_f64),
            (45, 3, 0.754270965462_f64, 0.952041479441_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.02132756984_f64, 0_f64),
            (65, 1, -0.613306838255_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.344526637064_f64, 0_f64),
            (53, 3, -0.003610257481_f64, 0.004009867371_f64),
        ],
    ),
    (true, &[(49, 0, 6_f64, 0_f64), (37, 2, 20_f64, 0_f64)]),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (2, 0, 0.014794336648_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (19, 1, -0.059419189352_f64, 0_f64),
            (14, 1, -2.742641598641_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 18.578805575288_f64, 0_f64),
            (8, 0, -0.013744726931_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 5.418823304347_f64, 0_f64),
            (59, 0, 62.058256213688_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(58, 1, 2.062588143616_f64, 0_f64), (37, 2, 6_f64, 0_f64)],
    ),
    (
        true,
        &[
            (59, 1, 5.77000366238_f64, 0_f64),
            (7, 3, -0.001606479749_f64, 0.001843361495_f64),
        ],
    ),
    (
        true,
        &[
            (40, 1, -0.008589755612_f64, 0_f64),
            (62, 1, 0.000529564504_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 21.149731261368_f64, 0_f64),
            (12, 0, -88.21972132586_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 21.149731261368_f64, 0_f64),
            (45, 1, 0.494425129905_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(65, 0, 4.082365664208_f64, 0_f64), (37, 2, 13_f64, 0_f64)],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (42, 1, 81.594378791001_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 13.57158226389_f64, 0_f64),
            (51, 0, 0.003969005461_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 15.545991535082_f64, 0_f64),
            (12, 0, -21.140333967991_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 82.454573967079_f64, 0_f64),
            (66, 3, -0.001328609306_f64, 0.001518218635_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.365308166414_f64, 0_f64),
            (9, 0, 0.070114758373_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (17, 0, 0.027303602215_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 12.370381786246_f64, 0_f64),
            (42, 0, 29.338239708836_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 87.917342833765_f64, 0_f64),
            (41, 1, -0.007157682445_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (15, 0, 1.834921291675_f64, 0_f64),
            (40, 1, -0.00093425444_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 98.683285404721_f64, 0_f64),
            (20, 3, -0.004935772745_f64, 0.006972713042_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.00254012653_f64, 0_f64),
            (64, 1, 0.468464018164_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.600813068941_f64, 0_f64),
            (12, 1, 89.545149191696_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.025827454359_f64, 0_f64),
            (64, 1, 0.247637271351_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 18.578805575288_f64, 0_f64),
            (52, 0, -0.018916226339_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(24, 1, 0.001716222973_f64, 0_f64), (37, 2, 11_f64, 0_f64)],
    ),
    (
        true,
        &[
            (59, 1, 7.366825932134_f64, 0_f64),
            (42, 0, 56.429411536562_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 7.366825932134_f64, 0_f64),
            (36, 0, 0.555858847899_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.004197919734_f64, 0_f64),
            (66, 0, 0.007976878246_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.187038250278_f64, 0_f64),
            (64, 1, 0.604709102916_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.011045001747_f64, 0_f64),
            (1, 1, -0.376852964393_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(32, 1, 1_f64, 0_f64), (65, 1, -1.457487435051_f64, 0_f64)],
    ),
    (
        true,
        &[
            (56, 1, 18.578805575288_f64, 0_f64),
            (6, 0, 0.841316476733_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(23, 1, 0.001538919285_f64, 0_f64), (37, 2, 22_f64, 0_f64)],
    ),
    (
        true,
        &[
            (13, 1, 0.039485687121_f64, 0_f64),
            (52, 0, 0.035969421722_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(22, 0, -0.0023840774_f64, 0_f64), (37, 2, 0_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 97.684471745776_f64, 0_f64),
            (13, 1, 0.848302105456_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 4_f64, 0_f64), (40, 1, -0.002824620023_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.379989986507_f64, 0_f64),
            (2, 1, 0.001924913661_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (1, 0, 0.834297868579_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.00152070846_f64, 0_f64),
            (43, 1, 33.487434256627_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.011045001747_f64, 0_f64),
            (8, 0, 0.040574464343_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (53, 0, 0.042504219742_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 98.683285404721_f64, 0_f64),
            (16, 1, 0.740047714875_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (5, 1, 0.000049170938_f64, 0_f64)],
    ),
    (
        true,
        &[
            (50, 1, -0.008323342559_f64, 0_f64),
            (52, 0, 0.053650017546_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.00254012653_f64, 0_f64),
            (16, 1, 0.44182204197_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.008177319388_f64, 0_f64),
            (38, 0, 0.000102169501_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 3.503757061587_f64, 0_f64),
            (39, 3, 0.276646203011_f64, 1.3696048831_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.60245942582_f64, 0_f64),
            (34, 1, -0.000858624762_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(42, 1, 0_f64, 0_f64), (11, 0, -72.536398228737_f64, 0_f64)],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (35, 1, 0.063903626236_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (65, 1, -0.917339193708_f64, 0_f64)],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (34, 0, 0.026410385548_f64, 0_f64)],
    ),
    (
        true,
        &[
            (65, 0, 4.082365664208_f64, 0_f64),
            (60, 0, 0.016082751957_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (65, 0, 4.082365664208_f64, 0_f64),
            (14, 3, -1.130209885596_f64, 1.148339337785_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.286381306679_f64, 0_f64),
            (67, 1, -0.025444647829_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (52, 0, 0.019270109235_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.005185093586_f64, 0_f64),
            (11, 0, 197.29361706083_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.005311716219_f64, 0_f64),
            (41, 0, 0.013210383149_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 98.632249177409_f64, 0_f64),
            (43, 0, 90.605260301048_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.755383566354_f64, 0_f64),
            (66, 1, -0.010067309537_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(13, 1, 0.000886273409_f64, 0_f64), (37, 2, 10_f64, 0_f64)],
    ),
    (
        false,
        &[
            (19, 0, 0.058487164589_f64, 0_f64),
            (42, 1, 56.429411536562_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(39, 0, 99.803555555201_f64, 0_f64), (37, 2, 6_f64, 0_f64)],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (13, 0, 0.944724032971_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.00152070846_f64, 0_f64),
            (0, 0, 4.34858605411_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(5, 0, 0.034547716221_f64, 0_f64), (37, 2, 13_f64, 0_f64)],
    ),
    (
        true,
        &[
            (5, 0, 0.034547716221_f64, 0_f64),
            (8, 3, -0.010096768528_f64, 0.010430496887_f64),
        ],
    ),
    (
        false,
        &[(43, 0, 90.605260301048_f64, 0_f64), (37, 2, 5_f64, 0_f64)],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (45, 1, 0.470794215282_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 4_f64, 0_f64), (24, 1, 0.006290713995_f64, 0_f64)],
    ),
    (
        true,
        &[
            (55, 1, 23.997196098696_f64, 0_f64),
            (45, 1, 0.391660049444_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 3_f64, 0_f64), (64, 1, 0.216880790466_f64, 0_f64)],
    ),
    (
        false,
        &[
            (40, 0, 0.007181184238_f64, 0_f64),
            (9, 1, -0.00859781896_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 20.330049175336_f64, 0_f64),
            (65, 1, -0.74850086188_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (70, 0, 0.833333333333_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(61, 0, 92.030000000006_f64, 0_f64), (37, 2, 7_f64, 0_f64)],
    ),
    (
        false,
        &[
            (58, 0, 87.60245942582_f64, 0_f64),
            (12, 1, -44.511965767527_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 15.30340668771_f64, 0_f64),
            (35, 1, 0.016307053175_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.037047849353_f64, 0_f64),
            (40, 1, -0.00093425444_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 18.659987150027_f64, 0_f64),
            (16, 1, 0.612626728912_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.005185093586_f64, 0_f64),
            (0, 0, 13.915733334115_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 12.370381786246_f64, 0_f64),
            (2, 1, 0.004669501157_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.351244737465_f64, 0_f64),
            (21, 1, -0.039152957487_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.351244737465_f64, 0_f64),
            (3, 1, 0.398786691967_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 83.212445361432_f64, 0_f64),
            (40, 1, -0.002122380767_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 20.566606160548_f64, 0_f64),
            (16, 1, 0.487954125812_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -196.455697922622_f64, 0_f64),
            (15, 0, -0.804112577713_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(59, 0, 96.060395753965_f64, 0_f64), (37, 2, 3_f64, 0_f64)],
    ),
    (
        true,
        &[(42, 1, 5.127057896226_f64, 0_f64), (70, 1, 0.5_f64, 0_f64)],
    ),
    (
        true,
        &[
            (40, 1, -0.008589755612_f64, 0_f64),
            (0, 1, -0.395621554253_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 22.509371455114_f64, 0_f64),
            (15, 0, -1.189485911056_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(21, 0, -0.00152070846_f64, 0_f64), (37, 2, 0_f64, 0_f64)],
    ),
    (
        true,
        &[
            (24, 1, 0.00254012653_f64, 0_f64),
            (38, 0, 0.002459555014_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.0023840774_f64, 0_f64),
            (62, 1, 0.000132092228_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.065876434269_f64, 0_f64),
            (36, 1, 0.216230850514_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(42, 1, 0_f64, 0_f64), (54, 0, 32.766297807582_f64, 0_f64)],
    ),
    (
        true,
        &[
            (11, 1, -196.455697922622_f64, 0_f64),
            (37, 2, 22_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (23, 0, 0.113311886667_f64, 0_f64),
            (45, 1, 0.545495059332_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 6_f64, 0_f64), (1, 1, -1.127098909018_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 98.632249177409_f64, 0_f64),
            (60, 0, 0.000972276508_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (0, 0, 0.228653572966_f64, 0_f64),
        ],
    ),
    (false, &[(69, 1, 0_f64, 0_f64), (37, 2, 12_f64, 0_f64)]),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (60, 0, 0.010478551636_f64, 0_f64)],
    ),
    (
        false,
        &[
            (22, 0, -0.001841392139_f64, 0_f64),
            (67, 1, -0.020395846802_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (58, 0, 89.433032932232_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.120614853726_f64, 0_f64),
            (53, 0, 0.088242640823_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.041798655327_f64, 0_f64),
            (10, 1, -0.422866719649_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 23.997196098696_f64, 0_f64),
            (0, 1, -11.652215766136_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (66, 0, 0.017835979054_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (0, 0, 13.915733334115_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.407243075194_f64, 0_f64),
            (52, 0, 0.041957845164_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (40, 1, -0.002122380767_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.187038250278_f64, 0_f64),
            (17, 0, -0.004060754253_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(59, 0, 96.060395753965_f64, 0_f64), (37, 2, 11_f64, 0_f64)],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (16, 1, 0.639508691164_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(64, 0, 4.86378068509_f64, 0_f64), (68, 2, 0_f64, 0_f64)],
    ),
    (
        false,
        &[(22, 0, -0.001841392139_f64, 0_f64), (37, 2, 23_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 5.418823304347_f64, 0_f64),
            (16, 0, 1.731253361596_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 16.441111837773_f64, 0_f64),
            (51, 3, -0.003698886648_f64, 0.003969005461_f64),
        ],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (9, 0, 0.03223102564_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.00152070846_f64, 0_f64),
            (42, 1, 36.602220742899_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.130209885596_f64, 0_f64),
            (20, 0, 0.065530755608_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -177.186804964052_f64, 0_f64),
            (10, 1, -0.286078754321_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 27.937723759111_f64, 0_f64),
            (20, 0, 0.014295696336_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.714252214364_f64, 0_f64),
            (46, 1, -1.129440954302_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 67.899626426361_f64, 0_f64),
            (41, 1, -0.009856122478_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (19, 1, -0.059419189352_f64, 0_f64),
            (38, 1, 0.000663724138_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(14, 1, -2.742641598641_f64, 0_f64), (37, 2, 10_f64, 0_f64)],
    ),
    (
        true,
        &[
            (15, 1, -2.126060052744_f64, 0_f64),
            (46, 1, -0.975669526392_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.087923069654_f64, 0_f64),
            (42, 1, 29.338239708836_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 13.599250472473_f64, 0_f64),
            (1, 1, -21.606633337189_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 13.57158226389_f64, 0_f64),
            (1, 1, -21.606633337189_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.005506890184_f64, 0_f64),
            (43, 0, 70.060688839056_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.735894374538_f64, 0_f64),
            (43, 0, 74.05027843037_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (64, 1, 0.216880790466_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 89.551972381464_f64, 0_f64),
            (13, 1, 0.065876434269_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.044672212756_f64, 0_f64),
            (51, 0, 0.001867688078_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (65, 1, -1.159346030872_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.081073940507_f64, 0_f64),
            (65, 1, -1.159346030872_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 12.370381786246_f64, 0_f64),
            (36, 0, 0.638203582875_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.952653717272_f64, 0_f64),
            (45, 1, 0.298268023925_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 83.706222662312_f64, 0_f64),
            (57, 1, -2.782739943152_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(22, 0, -0.000643444193_f64, 0_f64), (37, 3, 0_f64, 3_f64)],
    ),
    (
        false,
        &[(19, 0, 0.0494856271_f64, 0_f64), (37, 2, 2_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.001538919285_f64, 0_f64),
            (36, 0, 0.555858847899_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 21.149731261368_f64, 0_f64),
            (1, 1, -0.376852964393_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 4_f64, 0_f64), (61, 0, 50.208999999976_f64, 0_f64)],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (52, 3, -0.002369659403_f64, 0.0026622929_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.025827454359_f64, 0_f64),
            (58, 0, 83.212445361432_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (51, 1, -0.015005191316_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 4_f64, 0_f64), (67, 0, 0.039861770193_f64, 0_f64)],
    ),
    (
        true,
        &[
            (3, 1, -0.286381306679_f64, 0_f64),
            (40, 0, -0.000433596303_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 12.370381786246_f64, 0_f64),
            (40, 0, -0.00093425444_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.127057896226_f64, 0_f64),
            (1, 1, -1.893164059008_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 90.605260301048_f64, 0_f64),
            (46, 0, 2.14921727869_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(42, 0, 92.869319882182_f64, 0_f64), (37, 2, 8_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (56, 0, 67.911991596762_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(11, 1, -227.797658840935_f64, 0_f64), (37, 2, 0_f64, 0_f64)],
    ),
    (
        true,
        &[
            (14, 1, -1.343857006248_f64, 0_f64),
            (18, 0, 0.012838900259_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(48, 1, 0_f64, 0_f64), (2, 1, 0.001924913661_f64, 0_f64)],
    ),
    (
        true,
        &[
            (64, 0, 4.86378068509_f64, 0_f64),
            (14, 3, -1.130209885596_f64, 1.148339337785_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 20.19209860754_f64, 0_f64),
            (64, 1, 0.216880790466_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.001841392139_f64, 0_f64),
            (39, 0, 3.219116515314_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (18, 0, 0.054731271045_f64, 0_f64),
            (10, 1, -0.044892645486_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (65, 1, -0.965996072928_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.404114354323_f64, 0_f64),
            (8, 0, -0.005516138794_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(13, 0, 0.981752292899_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        true,
        &[(24, 1, 0.008074882103_f64, 0_f64), (26, 0, 1_f64, 0_f64)],
    ),
    (
        true,
        &[
            (13, 1, 0.008177319388_f64, 0_f64),
            (63, 0, 0.021404310273_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(58, 1, 2.062588143616_f64, 0_f64), (68, 2, 6_f64, 0_f64)],
    ),
    (
        true,
        &[
            (43, 1, 15.30340668771_f64, 0_f64),
            (35, 1, 0.040104438977_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.120614853726_f64, 0_f64),
            (9, 0, 0.070114758373_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.091259020395_f64, 0_f64),
            (8, 0, -0.005516138794_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (66, 0, 0.014461038157_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(19, 0, 0.037047849353_f64, 0_f64), (37, 2, 10_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 89.551972381464_f64, 0_f64),
            (13, 1, 0.17007379085_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.003294721515_f64, 0_f64),
            (62, 1, 0.000132092228_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (39, 0, 99.803555555201_f64, 0_f64),
            (64, 3, 0.697599259542_f64, 0.930270598004_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 94.73247534402_f64, 0_f64),
            (16, 1, 0.562623940981_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.680586695736_f64, 0_f64),
            (34, 0, 0.001112431058_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (19, 1, -0.049413631372_f64, 0_f64),
            (1, 1, -1.127098909018_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.605810138122_f64, 0_f64),
            (11, 3, -20.291648932434_f64, 24.041740853877_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (65, 1, -1.341363982825_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 83.706222662312_f64, 0_f64),
            (0, 0, 28.212125507464_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.219060225017_f64, 0_f64),
            (52, 0, 0.065040574296_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (62, 1, 0.000990969677_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(48, 1, 0_f64, 0_f64), (65, 1, -1.457487435051_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.005311716219_f64, 0_f64),
            (0, 0, 28.212125507464_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (15, 0, 1.834921291675_f64, 0_f64),
            (41, 1, -0.005301040898_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.417455072988_f64, 0_f64),
            (51, 0, -0.009145355373_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (52, 1, -0.065293293674_f64, 0_f64),
            (2, 1, 0.014794336648_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(42, 1, 0_f64, 0_f64), (23, 1, 0.001007431938_f64, 0_f64)],
    ),
    (
        false,
        &[
            (54, 0, 78.115236912005_f64, 0_f64),
            (23, 1, 0.011447152483_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 78.115236912005_f64, 0_f64),
            (20, 1, -0.004935772745_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.755383566354_f64, 0_f64),
            (57, 1, -0.942445884865_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.025827454359_f64, 0_f64),
            (62, 1, 0.000330481587_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(13, 1, 0.000886273409_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        true,
        &[(19, 1, -0.059419189352_f64, 0_f64), (37, 2, 5_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.001538919285_f64, 0_f64),
            (0, 1, -2.590382463299_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (5, 0, 0.034547716221_f64, 0_f64),
            (15, 3, -0.408127730548_f64, 0.467070327595_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 92.430123495695_f64, 0_f64),
            (63, 1, 0.002075087929_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 0.951946482376_f64, 0_f64),
            (66, 1, -0.015005818998_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.005068052866_f64, 0_f64),
            (66, 1, -0.010067309537_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(69, 0, 5_f64, 0_f64), (11, 0, 73.371261360937_f64, 0_f64)],
    ),
    (
        false,
        &[
            (7, 0, 0.044598294078_f64, 0_f64),
            (2, 1, 0.010328064626_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(18, 0, 0.054731271045_f64, 0_f64), (37, 2, 6_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 92.499098560538_f64, 0_f64),
            (43, 1, 39.31642972534_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.120614853726_f64, 0_f64),
            (57, 0, 1.850542798185_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(42, 0, 92.869319882182_f64, 0_f64), (30, 0, 10_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 13.57158226389_f64, 0_f64),
            (62, 1, 0.000034905632_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.971006267369_f64, 0_f64),
            (65, 1, -1.22650223573_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (11, 0, 262.3666355551_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (42, 1, 8.017659971847_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 0.951946482376_f64, 0_f64),
            (46, 1, -1.418396616757_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (19, 0, 0.037047849353_f64, 0_f64),
            (17, 1, -0.004060754253_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 6_f64, 0_f64), (1, 0, 6.159740467929_f64, 0_f64)],
    ),
    (
        false,
        &[(22, 0, -0.005068052866_f64, 0_f64), (28, 0, 1_f64, 0_f64)],
    ),
    (
        true,
        &[
            (3, 1, 0.149794157738_f64, 0_f64),
            (13, 0, 0.999698623782_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 20.19209860754_f64, 0_f64),
            (46, 1, -1.513028289708_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 83.706222662312_f64, 0_f64),
            (51, 1, -0.012881693805_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.219060225017_f64, 0_f64),
            (3, 0, 1.141318737455_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.971006267369_f64, 0_f64),
            (60, 0, 0.010478551636_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.006290713995_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.005185093586_f64, 0_f64),
            (11, 0, 178.260877568223_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.011045001747_f64, 0_f64),
            (56, 0, 67.911991596762_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.008074882103_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (21, 1, -0.103167040398_f64, 0_f64),
            (60, 1, 0.000972276508_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.00194583421_f64, 0_f64),
            (13, 0, 0.804606830818_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 89.433032932232_f64, 0_f64),
            (1, 0, 6.159740467929_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.17007379085_f64, 0_f64),
            (38, 0, 0.006878572146_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (40, 1, -0.001638248858_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(13, 0, 0.981752292899_f64, 0_f64), (29, 1, 1_f64, 0_f64)],
    ),
    (
        false,
        &[
            (20, 0, 0.098883002663_f64, 0_f64),
            (35, 0, 0.712962554434_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (20, 0, 0.087923069654_f64, 0_f64),
            (46, 0, 2.14921727869_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 15.30340668771_f64, 0_f64),
            (35, 1, 0.063903626236_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(48, 1, 0_f64, 0_f64), (62, 1, 0.000034905632_f64, 0_f64)],
    ),
    (
        false,
        &[
            (21, 0, -0.002909696646_f64, 0_f64),
            (42, 1, 25.185728924508_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 5_f64, 0_f64), (65, 1, -1.159346030872_f64, 0_f64)],
    ),
    (
        false,
        &[
            (3, 0, 1.02132756984_f64, 0_f64),
            (16, 1, 0.44182204197_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (24, 0, 0.154339127652_f64, 0_f64),
            (40, 1, -0.001638248858_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(32, 1, 1_f64, 0_f64), (12, 0, 112.53717582858_f64, 0_f64)],
    ),
    (
        false,
        &[(59, 0, 83.706222662312_f64, 0_f64), (49, 0, 4_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.002650174002_f64, 0_f64),
            (16, 0, 1.593071747057_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.00194583421_f64, 0_f64),
            (10, 1, -0.286078754321_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001155514485_f64, 0_f64),
            (23, 1, 0.004099778821_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (18, 1, -0.026259092371_f64, 0_f64)],
    ),
    (
        false,
        &[
            (19, 0, 0.037047849353_f64, 0_f64),
            (61, 1, 0.018666518665_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (65, 1, -1.159346030872_f64, 0_f64)],
    ),
    (
        false,
        &[(55, 0, 82.454573967079_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        true,
        &[(13, 1, 0.008177319388_f64, 0_f64), (32, 0, 5_f64, 0_f64)],
    ),
    (false, &[(69, 1, 0_f64, 0_f64), (37, 2, 0_f64, 0_f64)]),
    (
        true,
        &[
            (23, 1, 0.001538919285_f64, 0_f64),
            (11, 0, -20.291648932434_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 1_f64, 0_f64), (42, 1, 20.342848323295_f64, 0_f64)],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.343857006248_f64, 0_f64),
            (55, 0, 54.346520261538_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 25.103448275861_f64, 0_f64),
            (53, 0, 0.008885057326_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(54, 0, 87.21365662096_f64, 0_f64), (37, 2, 19_f64, 0_f64)],
    ),
    (
        true,
        &[
            (13, 1, 0.000886273409_f64, 0_f64),
            (42, 0, 81.594378791001_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 81.688139538765_f64, 0_f64),
            (65, 0, 3.746261866779_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 13.599250472473_f64, 0_f64),
            (34, 0, 0.002306019819_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (42, 1, 10.43007987269_f64, 0_f64),
        ],
    ),
    (true, &[(32, 1, 1_f64, 0_f64), (30, 0, 9_f64, 0_f64)]),
    (
        false,
        &[(54, 0, 87.21365662096_f64, 0_f64), (68, 2, 6_f64, 0_f64)],
    ),
    (
        false,
        &[
            (58, 0, 94.73247534402_f64, 0_f64),
            (2, 1, 0.0031654888_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(55, 0, 82.454573967079_f64, 0_f64), (37, 2, 19_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.005311716219_f64, 0_f64),
            (10, 1, -0.60399701237_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.869319882182_f64, 0_f64),
            (22, 3, -0.022597196786_f64, -0.014066516312_f64),
        ],
    ),
    (
        true,
        &[(15, 1, -2.60338060349_f64, 0_f64), (37, 2, 8_f64, 0_f64)],
    ),
    (
        true,
        &[
            (61, 0, 92.030000000006_f64, 0_f64),
            (17, 0, 0.014620999612_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008323342559_f64, 0_f64),
            (12, 0, 178.957313232822_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (58, 0, 70.74716204227_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002137372529_f64, 0_f64),
            (18, 1, -0.00660351419_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.971006267369_f64, 0_f64),
            (36, 1, 0.365308166414_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.014325177909_f64, 0_f64),
            (60, 0, 0.016082751957_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(22, 0, -0.005068052866_f64, 0_f64), (69, 0, 5_f64, 0_f64)],
    ),
    (
        false,
        &[
            (15, 0, 1.834921291675_f64, 0_f64),
            (46, 1, -1.302770069233_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 78.115236912005_f64, 0_f64),
            (12, 1, 47.198310116171_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000399711895_f64, 0_f64),
            (43, 1, 44.701987979408_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001155514485_f64, 0_f64),
            (1, 0, 0.416367086866_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 84.181207583347_f64, 0_f64),
            (13, 1, 0.17007379085_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.336010955575_f64, 0_f64),
            (64, 1, 0.247637271351_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008323342559_f64, 0_f64),
            (23, 0, 0.078922113522_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (65, 0, 4.082365664208_f64, 0_f64),
            (24, 0, 0.064133440082_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.025827454359_f64, 0_f64),
            (6, 1, 0.092564520481_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(65, 0, 4.082365664208_f64, 0_f64), (68, 2, 5_f64, 0_f64)],
    ),
    (
        true,
        &[
            (43, 1, 15.30340668771_f64, 0_f64),
            (1, 0, 25.855242512076_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000399711895_f64, 0_f64),
            (70, 0, 0.666666666667_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.006290713995_f64, 0_f64),
            (43, 0, 66.675796827867_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.130209885596_f64, 0_f64),
            (12, 0, 47.198310116171_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (64, 0, 4.86378068509_f64, 0_f64),
            (20, 0, 0.040068069939_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.039485687121_f64, 0_f64),
            (62, 1, 0.000330481587_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.594894995073_f64, 0_f64),
            (54, 1, 54.691946764945_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 4_f64, 0_f64), (40, 0, 0.005797178045_f64, 0_f64)],
    ),
    (
        true,
        &[
            (65, 0, 4.082365664208_f64, 0_f64),
            (43, 3, 44.701987979408_f64, 55.226633333302_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 10.023452950725_f64, 0_f64),
            (0, 1, -7.332863959192_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.120614853726_f64, 0_f64),
            (11, 0, 232.14555986203_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 89.433032932232_f64, 0_f64),
            (20, 1, -0.050503677536_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 20.541344875787_f64, 0_f64),
            (36, 0, 0.682113836631_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (13, 0, 0.981752292899_f64, 0_f64),
            (16, 0, 1.995743208991_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(32, 0, 5_f64, 0_f64), (58, 1, 20.541344875787_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 89.551972381464_f64, 0_f64),
            (57, 1, -1.939490804131_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 83.706222662312_f64, 0_f64),
            (16, 1, 0.400893813069_f64, 0_f64),
        ],
    ),
];

pub struct FiveYear70PctEthH1Rules632 {
    buffer: VecDeque<Candle>,
    min_votes: u32,
    rsi7: PyRsiState,
    rsi8: PyRsiState,
    rsi14: PyRsiState,
    atr14_ewm: PyAtrEwmState,
    macd: PyMacdState,
    ha: HaState,
    last_votes: (u32, u32),
}

impl FiveYear70PctEthH1Rules632 {
    pub fn new(min_votes: u32) -> Self {
        Self {
            buffer: VecDeque::with_capacity(MAX_WINDOW + 1),
            min_votes,
            rsi7: PyRsiState::new(7),
            rsi8: PyRsiState::new(8),
            rsi14: PyRsiState::new(14),
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

impl Strategy for FiveYear70PctEthH1Rules632 {
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
