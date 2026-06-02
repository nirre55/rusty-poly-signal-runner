use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "five_year_70pct_btc_15m_rules_176_min_votes_1";

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
            (22, 0, -0.000596058362_f64, 0_f64),
            (0, 0, 13.480756029953_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.354298009924_f64, 0_f64),
            (40, 0, 0.000140769183_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.005204708323_f64, 0_f64),
            (42, 0, 88.487367755168_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000940217931_f64, 0_f64),
            (16, 0, 2.222113870768_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.812368993449_f64, 0_f64),
            (17, 0, -0.000598615933_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (43, 1, 45.114842308581_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 2.254214593777_f64, 0_f64),
            (2, 0, 0.010292432286_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.673958918983_f64, 0_f64),
            (40, 0, 0.000552259724_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.354298009924_f64, 0_f64),
            (16, 1, 0.457331193089_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 0.912053646031_f64, 0_f64),
            (15, 1, 0.453097119828_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 5_f64, 0_f64), (10, 1, -0.743751369455_f64, 0_f64)],
    ),
    (
        true,
        &[(24, 1, 0.000123157637_f64, 0_f64), (44, 1, 30_f64, 0_f64)],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (17, 1, 0.000731601776_f64, 0_f64)],
    ),
    (
        true,
        &[
            (3, 1, -0.012972255848_f64, 0_f64),
            (22, 0, -0.00215355613_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.354298009924_f64, 0_f64),
            (2, 1, 0.000647938293_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(24, 1, 0.000123157637_f64, 0_f64), (37, 2, 22_f64, 0_f64)],
    ),
    (
        true,
        &[(24, 1, 0.000123157637_f64, 0_f64), (28, 0, 1_f64, 0_f64)],
    ),
    (
        false,
        &[
            (3, 0, 0.944943849905_f64, 0_f64),
            (2, 0, 0.014808079873_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 68.704859813649_f64, 0_f64),
            (65, 1, -1.459564445306_f64, 0_f64),
        ],
    ),
    (true, &[(42, 1, 0_f64, 0_f64), (44, 1, 15_f64, 0_f64)]),
    (
        true,
        &[
            (11, 1, -219.821108418032_f64, 0_f64),
            (1, 3, -0.121500890086_f64, -0.000002721736_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000123157637_f64, 0_f64),
            (1, 1, -0.904211298052_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.847408686743_f64, 0_f64),
            (66, 1, -0.004046947059_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (17, 1, -0.004905605597_f64, 0_f64),
            (46, 1, -1.563227737634_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.847408686743_f64, 0_f64),
            (35, 1, 0.017282150796_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -219.821108418032_f64, 0_f64),
            (0, 3, -0.127754869275_f64, -0.00002245468_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.129202691063_f64, 0_f64),
            (2, 1, 0.000647938293_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 30.319177476136_f64, 0_f64),
            (16, 1, 0.368391267019_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -188.547426560093_f64, 0_f64),
            (34, 0, -0.000805259118_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(11, 1, -169.388968825084_f64, 0_f64), (13, 0, 1_f64, 0_f64)],
    ),
    (
        true,
        &[
            (7, 1, -0.005375380189_f64, 0_f64),
            (56, 0, 72.313585043492_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.051658096366_f64, 0_f64),
            (40, 0, 0.000552259724_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000317708634_f64, 0_f64),
            (13, 1, 0.124796612801_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (42, 1, 43.719050398098_f64, 0_f64)],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (63, 0, 0.01004429117_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -249.930452912772_f64, 0_f64),
            (42, 3, 43.719050398098_f64, 56.000133414606_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 70.589612029705_f64, 0_f64),
            (43, 1, 40.166548899026_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000024352929_f64, 0_f64),
            (70, 0, 0.833333333333_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 5_f64, 0_f64), (39, 0, 44.037371685378_f64, 0_f64)],
    ),
    (
        true,
        &[
            (24, 1, 0.000518047549_f64, 0_f64),
            (42, 0, 62.524777166172_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 1.40832871207_f64, 0_f64),
            (38, 3, 0.000423226302_f64, 0.000806509475_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 5_f64, 0_f64), (67, 1, -0.023171858787_f64, 0_f64)],
    ),
    (
        true,
        &[(24, 1, 0.001086987122_f64, 0_f64), (32, 0, 5_f64, 0_f64)],
    ),
    (
        true,
        &[
            (15, 1, -2.880226526547_f64, 0_f64),
            (53, 0, -0.002710522059_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.004596587699_f64, 0_f64),
            (56, 0, 72.313585043492_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000024352929_f64, 0_f64),
            (58, 0, 0.306654763017_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.660005226124_f64, 0_f64),
            (2, 0, 0.014808079873_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.469218562694_f64, 0_f64),
            (40, 0, 0.000140769183_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000518047549_f64, 0_f64),
            (40, 1, -0.003453836366_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 99.995600480172_f64, 0_f64),
            (44, 0, 1395_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.075503944993_f64, 0_f64),
            (46, 1, -0.822178298076_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.005348262374_f64, 0_f64),
            (56, 0, 69.060618393279_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 6_f64, 0_f64), (13, 1, 0.000045742666_f64, 0_f64)],
    ),
    (
        true,
        &[
            (15, 1, -2.354298009924_f64, 0_f64),
            (21, 0, -0.002580027013_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 72.019291926381_f64, 0_f64),
            (12, 1, 24.33220298786_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.077124389957_f64, 0_f64),
            (40, 0, 0.000140769183_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000120857876_f64, 0_f64),
            (0, 1, -0.127754869275_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.210641246093_f64, 0_f64),
            (20, 0, 0.007795884974_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 2.111335702606_f64, 0_f64),
            (62, 1, 0.00024891366_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 88.470757284927_f64, 0_f64),
            (60, 0, 0.005420124579_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 4.796469453139_f64, 0_f64),
            (1, 1, -8.733281848762_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (56, 1, 44.801323047453_f64, 0_f64)],
    ),
    (
        false,
        &[
            (14, 0, 1.908788195242_f64, 0_f64),
            (45, 1, 0.47715417535_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -169.388968825084_f64, 0_f64),
            (62, 1, 0.000010977951_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (10, 3, 0.163926820707_f64, 0.45738367187_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -188.547426560093_f64, 0_f64),
            (46, 1, -0.822178298076_f64, 0_f64),
        ],
    ),
    (false, &[(48, 1, 0_f64, 0_f64), (30, 0, 10_f64, 0_f64)]),
    (
        false,
        &[
            (11, 0, 169.847668321743_f64, 0_f64),
            (65, 1, -0.873606765831_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(24, 1, 0.000123157637_f64, 0_f64), (44, 1, 105_f64, 0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 89.597440746859_f64, 0_f64),
            (60, 0, 0.005420124579_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.35554424836_f64, 0_f64),
            (40, 1, -0.000547643196_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 8.683865767064_f64, 0_f64),
            (18, 0, -0.000859095584_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.00041072089_f64, 0_f64),
            (61, 0, 3.979483531844_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.556303099059_f64, 0_f64),
            (16, 1, 0.486857216636_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (66, 1, -0.000993286689_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.644678507722_f64, 0_f64),
            (5, 0, 0.010655328013_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -169.388968825084_f64, 0_f64),
            (2, 0, 0.014808079873_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (46, 1, 0.245066079634_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(31, 1, 0_f64, 0_f64), (17, 0, 0.010119255562_f64, 0_f64)],
    ),
    (
        true,
        &[
            (55, 1, 26.021412739387_f64, 0_f64),
            (20, 0, 0.007795884974_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.007057262978_f64, 0_f64),
            (12, 0, 151.60195878472_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 5_f64, 0_f64), (43, 1, 25.454349113993_f64, 0_f64)],
    ),
    (
        true,
        &[
            (14, 1, -1.705007578106_f64, 0_f64),
            (45, 1, 0.387508369955_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(11, 1, -249.930452912772_f64, 0_f64), (31, 0, 2_f64, 0_f64)],
    ),
    (
        true,
        &[
            (23, 1, 0.000634394237_f64, 0_f64),
            (43, 0, 74.833194899826_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(21, 0, -2.58479e-7_f64, 0_f64), (37, 2, 23_f64, 0_f64)],
    ),
    (
        false,
        &[(22, 0, -0.000191900674_f64, 0_f64), (37, 2, 12_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.210641246093_f64, 0_f64),
            (24, 0, 0.01508702537_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (15, 0, 2.562798212558_f64, 0_f64),
            (63, 1, 0.000628644634_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 8.24623315708_f64, 0_f64),
            (67, 1, -0.023171858787_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (66, 1, -0.000430553703_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 0.937967556179_f64, 0_f64),
            (2, 1, 0.000800746324_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 6_f64, 0_f64), (22, 0, -0.003833684053_f64, 0_f64)],
    ),
    (
        true,
        &[
            (3, 1, -0.124295441041_f64, 0_f64),
            (46, 3, -0.464230165361_f64, -0.047403807875_f64),
        ],
    ),
    (
        false,
        &[
            (15, 0, 1.806249404164_f64, 0_f64),
            (67, 1, -0.014146164354_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -188.547426560093_f64, 0_f64),
            (46, 1, -0.638402427824_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 99.185382275768_f64, 0_f64),
            (67, 1, -0.009431026424_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.001549259691_f64, 0_f64),
            (66, 0, 0.003599409445_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(69, 1, 0_f64, 0_f64), (43, 1, 34.522447623121_f64, 0_f64)],
    ),
    (
        false,
        &[
            (21, 0, -2.58479e-7_f64, 0_f64),
            (53, 0, 0.019271469794_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(59, 1, 4.056578989833_f64, 0_f64), (29, 0, 6_f64, 0_f64)],
    ),
    (
        true,
        &[
            (24, 1, 0.001086987122_f64, 0_f64),
            (41, 0, 0.001725415755_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.772240951118_f64, 0_f64),
            (10, 1, -1.02601582424_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 3_f64, 0_f64), (52, 1, -0.016063109427_f64, 0_f64)],
    ),
    (
        false,
        &[
            (36, 0, 0.660005226124_f64, 0_f64),
            (41, 1, -0.012487152072_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.07407381255_f64, 0_f64),
            (19, 0, 0.025131808047_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000634394237_f64, 0_f64),
            (43, 0, 72.869843421606_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(24, 1, 0.000123157637_f64, 0_f64), (44, 1, 135_f64, 0_f64)],
    ),
    (
        true,
        &[
            (3, 1, -0.267047060819_f64, 0_f64),
            (46, 1, 1.413382543016_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.469218562694_f64, 0_f64),
            (2, 1, 0.000909758452_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (15, 1, 2.080158043835_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000201199042_f64, 0_f64),
            (46, 1, -1.563227737634_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 2.254214593777_f64, 0_f64),
            (64, 1, 0.555582600345_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 37.285529400902_f64, 0_f64),
            (67, 0, 0.021524632953_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 23.666491751372_f64, 0_f64),
            (56, 0, 64.275660580875_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.00007268176_f64, 0_f64),
            (61, 3, 0.180174725858_f64, 1.076237346725_f64),
        ],
    ),
    (true, &[(31, 1, 0_f64, 0_f64), (49, 1, 1_f64, 0_f64)]),
    (
        false,
        &[
            (54, 0, 63.311963948745_f64, 0_f64),
            (53, 1, -0.004977456214_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -249.930452912772_f64, 0_f64),
            (42, 3, 37.154692990356_f64, 62.524777166172_f64),
        ],
    ),
    (
        true,
        &[(59, 1, 4.056578989833_f64, 0_f64), (30, 0, 10_f64, 0_f64)],
    ),
    (
        false,
        &[
            (15, 0, 2.359739080046_f64, 0_f64),
            (9, 1, 0.001347532606_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 35.443039978605_f64, 0_f64),
            (15, 0, -0.372375456891_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(14, 1, -2.469218562694_f64, 0_f64), (37, 2, 22_f64, 0_f64)],
    ),
    (
        true,
        &[
            (36, 1, 0.273882464262_f64, 0_f64),
            (16, 1, 0.368391267019_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.267047060819_f64, 0_f64),
            (51, 0, -0.003325085454_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.004596587699_f64, 0_f64),
            (2, 1, 0.001646300512_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 88.470757284927_f64, 0_f64),
            (34, 1, -0.000358476716_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -249.930452912772_f64, 0_f64),
            (37, 2, 12_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 99.995600480172_f64, 0_f64),
            (21, 1, -2.58479e-7_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.077124389957_f64, 0_f64),
            (46, 1, -0.638402427824_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -169.388968825084_f64, 0_f64),
            (0, 1, -3.508923036121_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 28.288679424359_f64, 0_f64),
            (12, 0, -19.900866452413_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(3, 1, -0.267047060819_f64, 0_f64), (29, 1, 0_f64, 0_f64)],
    ),
    (
        true,
        &[
            (58, 1, 3.170746597814_f64, 0_f64),
            (61, 0, 3.979483531844_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.077124389957_f64, 0_f64),
            (35, 1, 0.172008319709_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.705007578106_f64, 0_f64),
            (57, 0, 3.241967620561_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 20.625140058973_f64, 0_f64),
            (67, 0, 0.009956278533_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (15, 0, 2.562798212558_f64, 0_f64),
            (18, 1, 0.002256340125_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (3, 0, 1.202805912201_f64, 0_f64),
            (57, 1, 1.323958574628_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.908788195242_f64, 0_f64),
            (65, 1, -1.1448685998_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.342144035663_f64, 0_f64),
            (9, 0, 0.006843139792_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000120857876_f64, 0_f64),
            (1, 0, 2.301340395721_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.242308839024_f64, 0_f64),
            (52, 0, 0.00097606558_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.273882464262_f64, 0_f64),
            (24, 0, 0.036483970215_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 83.773682781036_f64, 0_f64),
            (34, 1, -0.000805259118_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (15, 0, 2.562798212558_f64, 0_f64),
            (63, 1, 0.000742956712_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000066775201_f64, 0_f64),
            (16, 0, 1.743568164167_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000917948329_f64, 0_f64),
            (12, 1, -19.900866452413_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(49, 0, 4_f64, 0_f64), (43, 0, 78.511633328839_f64, 0_f64)],
    ),
    (
        true,
        &[(31, 1, 0_f64, 0_f64), (20, 0, 0.043522704487_f64, 0_f64)],
    ),
    (
        true,
        &[(7, 1, -0.007111471533_f64, 0_f64), (25, 0, 1_f64, 0_f64)],
    ),
    (
        true,
        &[
            (59, 1, 0.937967556179_f64, 0_f64),
            (7, 0, -0.000631568573_f64, 0_f64),
        ],
    ),
    (
        true,
        &[(32, 1, 0_f64, 0_f64), (0, 1, -6.053498735868_f64, 0_f64)],
    ),
    (
        false,
        &[
            (21, 0, -0.000982674303_f64, 0_f64),
            (18, 1, -0.00383570624_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 39.218425501329_f64, 0_f64),
            (20, 0, 0.007795884974_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000120857876_f64, 0_f64),
            (6, 1, 0.302247845896_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 97.683323069458_f64, 0_f64),
            (43, 1, 31.262587312934_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(59, 0, 99.510770296207_f64, 0_f64), (37, 2, 20_f64, 0_f64)],
    ),
    (
        true,
        &[
            (59, 1, 8.683865767064_f64, 0_f64),
            (56, 0, 47.660407199097_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.00041072089_f64, 0_f64),
            (41, 0, 0.009375325403_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.908848436953_f64, 0_f64),
            (56, 1, 55.688271144659_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.273882464262_f64, 0_f64),
            (24, 0, 0.030155709967_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.273882464262_f64, 0_f64),
            (22, 0, -0.001334494357_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (7, 1, -0.005375380189_f64, 0_f64),
            (18, 0, 0.029677055093_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000268702787_f64, 0_f64),
            (16, 0, 1.89989821024_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000268702787_f64, 0_f64),
            (66, 1, -0.008392877382_f64, 0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.689124791178_f64, 0_f64),
            (66, 1, -0.011103809101_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 8.683865767064_f64, 0_f64),
            (14, 0, -0.753237336396_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.210641246093_f64, 0_f64),
            (40, 0, 0.000310296605_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.242308839024_f64, 0_f64),
            (58, 0, 61.27763335905_f64, 0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 5_f64, 0_f64), (11, 1, -21.007131050442_f64, 0_f64)],
    ),
    (
        false,
        &[
            (11, 0, 169.847668321743_f64, 0_f64),
            (67, 0, 0.021524632953_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.005348262374_f64, 0_f64),
            (0, 1, -37.834563176653_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.124295441041_f64, 0_f64),
            (20, 0, 0.0103205015_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.273882464262_f64, 0_f64),
            (64, 1, 0.258964702845_f64, 0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.267047060819_f64, 0_f64),
            (16, 1, 0.656844883173_f64, 0_f64),
        ],
    ),
];

pub struct FiveYear70PctBtcM15Rules176 {
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

impl FiveYear70PctBtcM15Rules176 {
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

impl Strategy for FiveYear70PctBtcM15Rules176 {
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
