use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "five_year_70pct_btc_1h_rules_586_min_votes_1";

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
struct Feats {
    f: [Option<f64>; 69],
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
        None => return Feats { f: [None; 69] },
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

    let mut f: [Option<f64>; 69] = [None; 69];
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
    f[44] = Some(hour * 60.0);
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
            (24, 1, 0.000960780049_f64, 0.0_f64),
            (54, 1, 10.880992792283_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000960780049_f64, 0.0_f64),
            (40, 1, -0.00540043628_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.030383565182_f64, 0.0_f64),
            (46, 1, -1.536658910705_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.003848934884_f64, 0.0_f64),
            (12, 0, 293.229864434571_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.405699915549_f64, 0.0_f64),
            (5, 0, 0.027039498754_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.836158938595_f64, 0.0_f64),
            (56, 0, 47.524239591808_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (43, 1, 26.672446581558_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (63, 0, 0.012990335975_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000960780049_f64, 0.0_f64),
            (67, 0, 0.015962036922_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.030383565182_f64, 0.0_f64),
            (40, 0, 0.005581053296_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000544949204_f64, 0.0_f64),
            (20, 0, 0.018572337802_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (20, 1, -0.008468540427_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 0.0_f64, 0.0_f64),
            (63, 1, 0.00198697707_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000727402067_f64, 0.0_f64),
            (10, 1, -0.479441712554_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.282482101409_f64, 0.0_f64),
            (62, 0, 0.008999088235_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 22.87928198245_f64, 0.0_f64),
            (29, 0, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 10.339569656362_f64, 0.0_f64),
            (38, 1, 0.000468970418_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 1.0_f64, 0.0_f64),
            (2, 1, 0.001367718303_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.005049606963_f64, 0.0_f64),
            (18, 0, 0.055600520317_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.785356550293_f64, 0.0_f64),
            (29, 0, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000394386125_f64, 0.0_f64),
            (46, 1, -0.781972702142_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.251600440171_f64, 0.0_f64),
            (58, 0, 12.958361968728_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.030383565182_f64, 0.0_f64),
            (11, 0, 199.552574965572_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.056247011082_f64, 0.0_f64),
            (27, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (46, 1, -0.979734943474_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000547061338_f64, 0.0_f64),
            (37, 2, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.3831114696_f64, 0.0_f64),
            (21, 1, -0.002433279017_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.014926060763_f64, 0.0_f64),
            (45, 1, 0.41501807801_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (12, 1, -296.373970792508_f64, 0.0_f64),
            (37, 2, 22.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.378420082698_f64, 0.0_f64),
            (66, 0, 0.025174710705_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.001665889857_f64, 0.0_f64),
            (36, 1, 0.405699915549_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (19, 1, -0.008372624821_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 12.502131887069_f64, 0.0_f64),
            (17, 0, -0.016588506863_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (64, 1, 0.485298641061_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 2.667588714436_f64, 0.0_f64),
            (37, 2, 20.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001486921254_f64, 0.0_f64),
            (20, 0, 0.069654406809_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000394386125_f64, 0.0_f64),
            (45, 1, 0.663177938908_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (53, 1, -0.010982608219_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.00121384214_f64, 0.0_f64),
            (10, 1, -0.479441712554_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 1.724176425078_f64, 0.0_f64),
            (67, 0, 0.010852199486_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.056247011082_f64, 0.0_f64),
            (46, 1, -1.536658910705_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 4.0_f64, 0.0_f64),
            (16, 1, 0.379321903939_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.829683244288_f64, 0.0_f64),
            (43, 0, 44.954532800507_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (1, 1, -2.392471671249_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (0, 0, 7.069171215076_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.353463333651_f64, 0.0_f64),
            (29, 0, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (37, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000193374386_f64, 0.0_f64),
            (64, 1, 0.427551130261_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000547061338_f64, 0.0_f64),
            (68, 2, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.110521327309_f64, 0.0_f64),
            (28, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (12, 1, 24.604797846729_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (41, 1, -0.005247823951_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.443623461596_f64, 0.0_f64),
            (16, 1, 0.712660732397_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.21492658429_f64, 0.0_f64),
            (10, 1, -0.479441712554_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (41, 1, -0.015508648028_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000544949204_f64, 0.0_f64),
            (42, 1, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (20, 0, 0.079980230055_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 10.880992792283_f64, 0.0_f64),
            (40, 0, -0.001560612463_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (61, 0, 15.903107859896_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000547061338_f64, 0.0_f64),
            (43, 1, 16.087074533329_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.398919564528_f64, 0.0_f64),
            (62, 0, 0.021833334011_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 4.0_f64, 0.0_f64),
            (2, 1, 0.001367718303_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 95.010861843374_f64, 0.0_f64),
            (43, 1, 30.621116950057_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 19.291134799525_f64, 0.0_f64),
            (52, 0, -0.003871656787_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 87.326571431094_f64, 0.0_f64),
            (37, 2, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (57, 3, -0.530538416865_f64, 0.540841360867_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (16, 1, 0.712660732397_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.000960780049_f64, 0.0_f64),
            (56, 0, 44.439233481289_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000544949204_f64, 0.0_f64),
            (37, 2, 20.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000905802239_f64, 0.0_f64),
            (63, 0, 0.021513684939_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (11, 1, 24.061550080429_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 1.724176425078_f64, 0.0_f64),
            (37, 2, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (37, 2, 19.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 0.0_f64, 0.0_f64),
            (2, 1, 0.00260692919_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.398919564528_f64, 0.0_f64),
            (56, 1, 53.275547235097_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.246542685688_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.307122278049_f64, 0.0_f64),
            (46, 1, -1.536658910705_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000298754317_f64, 0.0_f64),
            (34, 1, -0.018308813609_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.002878374423_f64, 0.0_f64),
            (67, 0, 0.032872077208_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (65, 0, 4.071608244105_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.003848934884_f64, 0.0_f64),
            (15, 0, 2.418422530824_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001486921254_f64, 0.0_f64),
            (19, 0, 0.050318074037_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 87.326571431094_f64, 0.0_f64),
            (37, 2, 8.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (37, 2, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (39, 0, 16.649945784544_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (67, 1, -0.018783731743_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (64, 1, 0.226931525872_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 19.168638833873_f64, 0.0_f64),
            (37, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 82.623793495996_f64, 0.0_f64),
            (0, 1, -1.167889331231_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.962352079221_f64, 0.0_f64),
            (2, 1, 0.001367718303_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (65, 1, -0.845591632774_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.251559689203_f64, 0.0_f64),
            (20, 0, 0.041237689058_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (19, 0, 0.050318074037_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (1, 1, -6.984363537212_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 2.667588714436_f64, 0.0_f64),
            (2, 1, 0.002253984365_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000193374386_f64, 0.0_f64),
            (64, 0, 3.216266332393_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000193374386_f64, 0.0_f64),
            (35, 1, 0.238970973439_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 89.80044543946_f64, 0.0_f64),
            (23, 3, 0.008375974151_f64, 0.013233652106_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 13.103119286977_f64, 0.0_f64),
            (1, 1, -42.545668662262_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (22, 1, -0.029226430803_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (58, 1, 61.254716857094_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (46, 1, -1.12961546566_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.251559689203_f64, 0.0_f64),
            (52, 0, 0.001941450114_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.251559689203_f64, 0.0_f64),
            (1, 1, -42.545668662262_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.006258679323_f64, 0.0_f64),
            (42, 0, 94.143448336176_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000409020996_f64, 0.0_f64),
            (35, 1, 0.159137292685_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (34, 0, 0.005473009889_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.3831114696_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.0107807827_f64, 0.0_f64),
            (9, 0, 0.06947460341_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.0107807827_f64, 0.0_f64),
            (64, 1, 0.401411759521_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.582148468611_f64, 0.0_f64),
            (34, 1, -0.000649924195_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.334336613744_f64, 0.0_f64),
            (64, 1, 0.294692937497_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 16.815653800848_f64, 0.0_f64),
            (46, 1, -0.781972702142_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.970296088074_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.0088103787_f64, 0.0_f64),
            (15, 0, 0.838389255232_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (48, 1, 0.0_f64, 0.0_f64),
            (34, 1, -0.001555592433_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.013135821846_f64, 0.0_f64),
            (11, 0, 114.801444660689_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.325740640992_f64, 0.0_f64),
            (15, 1, 1.703673786689_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.325740640992_f64, 0.0_f64),
            (12, 1, 114.585522147979_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (7, 1, -0.006812659013_f64, 0.0_f64),
            (35, 1, 0.003892759088_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 2.0_f64, 0.0_f64),
            (18, 0, 0.055600520317_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.55850139458_f64, 0.0_f64),
            (40, 0, 0.000677516823_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (23, 0, 0.089607310064_f64, 0.0_f64),
            (16, 1, 1.048736347697_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.186848887508_f64, 0.0_f64),
            (67, 0, 0.032872077208_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.083867390324_f64, 0.0_f64),
            (66, 0, 0.014137917108_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.083867390324_f64, 0.0_f64),
            (45, 1, 0.41501807801_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.962352079221_f64, 0.0_f64),
            (42, 1, 37.017750359136_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 64.191554432408_f64, 0.0_f64),
            (23, 1, 0.002878374423_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (38, 1, 0.000330626902_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.089213320949_f64, 0.0_f64),
            (27, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.325740640992_f64, 0.0_f64),
            (12, 1, 127.802445543786_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002077937651_f64, 0.0_f64),
            (20, 0, 0.069654406809_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 4.845802158605_f64, 0.0_f64),
            (1, 1, -1.081483750837_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 95.010861843374_f64, 0.0_f64),
            (66, 0, 0.025174710705_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (46, 1, -1.536658910705_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.785356550293_f64, 0.0_f64),
            (0, 0, 7.069171215076_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (61, 1, 0.035918324407_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 2.667588714436_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.251600440171_f64, 0.0_f64),
            (1, 0, 6.855156552576_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (39, 0, 50.815714285668_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 89.80044543946_f64, 0.0_f64),
            (51, 1, -0.005049606963_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 89.80044543946_f64, 0.0_f64),
            (56, 1, 60.198393960611_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 5.0_f64, 0.0_f64),
            (41, 1, -0.010354562484_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.633462182502_f64, 0.0_f64),
            (66, 1, -0.007337505493_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.633462182502_f64, 0.0_f64),
            (45, 1, 0.41501807801_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 10.880992792283_f64, 0.0_f64),
            (62, 0, 0.014569887971_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000409020996_f64, 0.0_f64),
            (41, 1, -0.005247823951_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 0.0_f64, 0.0_f64),
            (62, 0, 0.012698614211_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 0.0_f64, 0.0_f64),
            (41, 0, 0.007101123994_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 4.0_f64, 0.0_f64),
            (16, 0, 2.255084993412_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.307122278049_f64, 0.0_f64),
            (51, 0, 0.002986725743_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.307122278049_f64, 0.0_f64),
            (66, 0, 0.016352132612_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (43, 1, 13.050476937067_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (43, 3, 44.954532800507_f64, 54.831328521965_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (46, 1, 0.216746335689_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 89.541313522713_f64, 0.0_f64),
            (65, 1, -1.385238691491_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 89.541313522713_f64, 0.0_f64),
            (0, 0, 30.625040783455_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.282482101409_f64, 0.0_f64),
            (18, 0, 0.017136786069_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.582148468611_f64, 0.0_f64),
            (51, 1, -0.002741635191_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 8.551406555269_f64, 0.0_f64),
            (0, 1, -10.65253216544_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 8.551406555269_f64, 0.0_f64),
            (35, 1, 0.039537406427_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.334336613744_f64, 0.0_f64),
            (20, 0, 0.05731139405_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (12, 1, -296.373970792508_f64, 0.0_f64),
            (34, 0, -0.003927732991_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 10.339569656362_f64, 0.0_f64),
            (62, 1, 0.000248581366_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.586231489434_f64, 0.0_f64),
            (43, 0, 73.639202986603_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.735382052348_f64, 0.0_f64),
            (29, 0, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.378420082698_f64, 0.0_f64),
            (14, 1, -2.733254693763_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001486921254_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.013135821846_f64, 0.0_f64),
            (42, 0, 84.565462141233_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.325740640992_f64, 0.0_f64),
            (40, 1, 0.001174029944_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 24.975510474295_f64, 0.0_f64),
            (43, 0, 39.788932118881_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (16, 1, 0.346497778931_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 1.0_f64, 0.0_f64),
            (16, 1, 0.346497778931_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 87.326571431094_f64, 0.0_f64),
            (64, 1, 0.319900011809_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.293693136323_f64, 0.0_f64),
            (61, 0, 0.389314900769_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.002906015874_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.829683244288_f64, 0.0_f64),
            (37, 2, 23.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 98.026144238666_f64, 0.0_f64),
            (16, 1, 0.346497778931_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 80.15965415611_f64, 0.0_f64),
            (47, 1, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.118942180943_f64, 0.0_f64),
            (40, 0, 0.000677516823_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.045800786039_f64, 0.0_f64),
            (65, 1, -1.100475105203_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 37.670808563448_f64, 0.0_f64),
            (41, 0, 0.009953537523_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.271737232917_f64, 0.0_f64),
            (24, 1, 0.007117316977_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.005579021174_f64, 0.0_f64),
            (12, 0, 114.585522147979_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 70.510977130054_f64, 0.0_f64),
            (30, 0, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 76.610575342271_f64, 0.0_f64),
            (35, 1, 0.024403590639_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 4.845802158605_f64, 0.0_f64),
            (37, 2, 20.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 9.266584745766_f64, 0.0_f64),
            (2, 1, 0.001611324521_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (2, 0, 0.015565536959_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.003883345889_f64, 0.0_f64),
            (0, 0, 30.625040783455_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.114097501656_f64, 0.0_f64),
            (20, 0, 0.05731139405_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 24.430142129257_f64, 0.0_f64),
            (20, 0, 0.05731139405_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (23, 0, 0.089607310064_f64, 0.0_f64),
            (2, 1, 0.013586365309_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (59, 1, 24.168754528768_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (37, 2, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.110521327309_f64, 0.0_f64),
            (27, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 95.010861843374_f64, 0.0_f64),
            (40, 1, -0.000710524492_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001486921254_f64, 0.0_f64),
            (0, 0, 7.069171215076_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (48, 1, 0.0_f64, 0.0_f64),
            (17, 1, -0.016588506863_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (51, 1, -0.002741635191_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (10, 1, -0.227167506527_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (52, 1, -0.013915095326_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 19.168638833873_f64, 0.0_f64),
            (68, 2, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.003848934884_f64, 0.0_f64),
            (12, 0, 249.550501703559_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (22, 1, -0.022318725425_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (16, 1, 0.421734393474_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 0.0_f64, 0.0_f64),
            (54, 0, 46.363173508285_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 4.0_f64, 0.0_f64),
            (62, 1, 2.4786183e-05_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 6.0_f64, 0.0_f64),
            (21, 0, -0.007401969759_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (67, 1, -0.014757646695_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (37, 2, 16.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 16.815653800848_f64, 0.0_f64),
            (65, 1, -0.625244346095_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.0088103787_f64, 0.0_f64),
            (17, 0, 0.00316609386_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 27.185662222686_f64, 0.0_f64),
            (12, 0, -43.533872572536_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 10.831501560562_f64, 0.0_f64),
            (23, 0, 0.032597623032_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.002900258506_f64, 0.0_f64),
            (67, 0, 0.020288489414_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 92.58675389879_f64, 0.0_f64),
            (40, 1, -0.000710524492_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.160650711145_f64, 0.0_f64),
            (0, 0, 4.657215509588_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.282482101409_f64, 0.0_f64),
            (18, 0, 0.009380892054_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.282482101409_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 2.0_f64, 0.0_f64),
            (20, 0, 0.079980230055_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 98.026144238666_f64, 0.0_f64),
            (64, 1, 0.485298641061_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (23, 0, 0.089607310064_f64, 0.0_f64),
            (38, 3, 0.001043051246_f64, 0.00182351847_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (19, 1, -0.004457586342_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.002900258506_f64, 0.0_f64),
            (67, 0, 0.015962036922_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.001601366876_f64, 0.0_f64),
            (67, 0, 0.014065228263_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 1.371097431342_f64, 0.0_f64),
            (67, 0, 0.010852199486_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 21.973808882615_f64, 0.0_f64),
            (37, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 1.724176425078_f64, 0.0_f64),
            (34, 0, -0.002897737883_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.110521327309_f64, 0.0_f64),
            (0, 0, 1.373119792908_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.005049606963_f64, 0.0_f64),
            (35, 1, 0.003892759088_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.156166218685_f64, 0.0_f64),
            (1, 1, -42.545668662262_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 3.156069283152_f64, 0.0_f64),
            (37, 2, 21.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000193374386_f64, 0.0_f64),
            (38, 1, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.016780876652_f64, 0.0_f64),
            (40, 1, -0.004351170326_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.00121384214_f64, 0.0_f64),
            (13, 1, 0.408114754734_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000905802239_f64, 0.0_f64),
            (14, 0, -0.73657496892_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (1, 1, -1.081483750837_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.056247011082_f64, 0.0_f64),
            (57, 0, 0.540841360867_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.006258679323_f64, 0.0_f64),
            (16, 1, 0.499150169133_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.00834711336_f64, 0.0_f64),
            (30, 1, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 19.291134799525_f64, 0.0_f64),
            (10, 1, -0.313680587227_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (31, 1, 0.0_f64, 0.0_f64),
            (66, 0, 0.025174710705_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.353463333651_f64, 0.0_f64),
            (46, 3, -0.464821755383_f64, -0.073200228374_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 10.339569656362_f64, 0.0_f64),
            (30, 1, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (34, 1, -0.003927732991_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 24.430142129257_f64, 0.0_f64),
            (11, 0, 47.005414521394_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.405699915549_f64, 0.0_f64),
            (12, 0, 207.033158725627_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.003927732991_f64, 0.0_f64),
            (46, 1, -1.422984964535_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.55850139458_f64, 0.0_f64),
            (37, 2, 20.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 37.670808563448_f64, 0.0_f64),
            (20, 0, 0.033661984868_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 84.366649993316_f64, 0.0_f64),
            (36, 1, 0.335678247138_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -3.275620005277_f64, 0.0_f64),
            (4, 0, -0.006258679323_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 10.831501560562_f64, 0.0_f64),
            (16, 0, 2.255084993412_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 89.80044543946_f64, 0.0_f64),
            (63, 1, 0.003091757798_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.251559689203_f64, 0.0_f64),
            (61, 0, 102.022412698827_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (37, 2, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (34, 0, 0.003961522568_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.722377357787_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -3.275620005277_f64, 0.0_f64),
            (6, 3, 0.348371370028_f64, 0.50546791832_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 82.208371283464_f64, 0.0_f64),
            (36, 1, 0.335678247138_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 76.610575342271_f64, 0.0_f64),
            (62, 1, 2.4786183e-05_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 18.570891908685_f64, 0.0_f64),
            (37, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 82.623793495996_f64, 0.0_f64),
            (2, 3, 0.006117730378_f64, 0.007861395294_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000409020996_f64, 0.0_f64),
            (14, 1, 1.343326918368_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.012975327157_f64, 0.0_f64),
            (43, 0, 73.639202986603_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 36.800776725066_f64, 0.0_f64),
            (20, 0, 0.033661984868_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (46, 1, -1.536658910705_f64, 0.0_f64),
            (37, 2, 3.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000193374386_f64, 0.0_f64),
            (3, 0, 1.300789052776_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.016780876652_f64, 0.0_f64),
            (10, 1, -0.672155355901_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.398919564528_f64, 0.0_f64),
            (57, 1, -1.54535539948_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (65, 1, -1.385238691491_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 92.58675389879_f64, 0.0_f64),
            (4, 1, -0.001640394711_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.443623461596_f64, 0.0_f64),
            (37, 2, 3.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 19.291134799525_f64, 0.0_f64),
            (39, 0, 103.358300394673_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.004499095898_f64, 0.0_f64),
            (57, 0, 2.22226616351_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.002344473015_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (48, 1, 0.0_f64, 0.0_f64),
            (40, 1, -0.00540043628_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.013135821846_f64, 0.0_f64),
            (52, 0, 0.042397489333_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (7, 1, -0.005085891719_f64, 0.0_f64),
            (56, 0, 76.641583089522_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.293693136323_f64, 0.0_f64),
            (40, 3, -0.00032682283_f64, 0.000294242144_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.005802879975_f64, 0.0_f64),
            (64, 1, 0.226931525872_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002077937651_f64, 0.0_f64),
            (12, 1, -70.584535914177_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (23, 0, 0.089607310064_f64, 0.0_f64),
            (11, 1, -71.601769964354_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.156166218685_f64, 0.0_f64),
            (11, 0, -19.619345111905_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 34.5377351189_f64, 0.0_f64),
            (15, 0, -0.382943541103_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 70.510977130054_f64, 0.0_f64),
            (16, 1, 0.421734393474_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.023052580473_f64, 0.0_f64),
            (46, 1, -0.781972702142_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 64.191554432408_f64, 0.0_f64),
            (41, 1, -0.010354562484_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 24.168754528768_f64, 0.0_f64),
            (14, 0, 1.343326918368_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 5.0_f64, 0.0_f64),
            (56, 1, 44.439233481289_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000544949204_f64, 0.0_f64),
            (62, 0, 0.012698614211_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 19.992833874478_f64, 0.0_f64),
            (4, 0, 0.008104133898_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 10.339569656362_f64, 0.0_f64),
            (45, 1, 0.663177938908_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.586231489434_f64, 0.0_f64),
            (45, 1, 0.41501807801_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.69192343888_f64, 0.0_f64),
            (45, 1, 0.41501807801_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 36.800776725066_f64, 0.0_f64),
            (46, 1, -1.422984964535_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000544949204_f64, 0.0_f64),
            (60, 0, 0.004468097235_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 15.180011010737_f64, 0.0_f64),
            (61, 0, 102.022412698827_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (61, 0, 5.937114574536_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 4.0_f64, 0.0_f64),
            (41, 0, 0.012219697177_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000727402067_f64, 0.0_f64),
            (37, 2, 9.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (56, 1, 53.275547235097_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.405699915549_f64, 0.0_f64),
            (52, 0, 0.03248572488_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.212888338411_f64, 0.0_f64),
            (20, 0, 0.033661984868_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (24, 0, 0.06199942319_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.595452540363_f64, 0.0_f64),
            (0, 0, 30.625040783455_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 5.0_f64, 0.0_f64),
            (66, 1, -0.007337505493_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[(32, 1, 0.0_f64, 0.0_f64), (68, 2, 5.0_f64, 0.0_f64)],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (2, 1, 0.003315567219_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 87.326571431094_f64, 0.0_f64),
            (20, 3, -0.003483247059_f64, 0.005497807331_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.398919564528_f64, 0.0_f64),
            (45, 1, 0.375660841825_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.005317545063_f64, 0.0_f64),
            (5, 1, 0.005359445082_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 10.339569656362_f64, 0.0_f64),
            (68, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.022037703312_f64, 0.0_f64),
            (37, 2, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (37, 2, 13.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 80.15965415611_f64, 0.0_f64),
            (46, 1, -0.979734943474_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 82.623793495996_f64, 0.0_f64),
            (35, 1, 0.238970973439_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.962352079221_f64, 0.0_f64),
            (16, 1, 0.346497778931_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.001239879895_f64, 0.0_f64),
            (14, 0, -0.362813601211_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 15.251559689203_f64, 0.0_f64),
            (9, 0, 0.002735464365_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 10.880992792283_f64, 0.0_f64),
            (5, 1, 0.000391630717_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.246542685688_f64, 0.0_f64),
            (22, 0, -0.006079005042_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (40, 0, 0.002443200772_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 13.443623461596_f64, 0.0_f64),
            (13, 0, 0.810041941282_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.3831114696_f64, 0.0_f64),
            (64, 1, 0.357082553591_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (32, 0, 5.0_f64, 0.0_f64),
            (56, 1, 30.070530512101_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (7, 1, -0.009512380842_f64, 0.0_f64),
            (36, 0, 0.637963905121_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 22.87928198245_f64, 0.0_f64),
            (20, 0, 0.010985554175_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 87.588360619543_f64, 0.0_f64),
            (32, 1, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 87.588360619543_f64, 0.0_f64),
            (20, 1, -0.027044536727_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 83.360348764515_f64, 0.0_f64),
            (52, 1, -0.007193698402_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.022563546352_f64, 0.0_f64),
            (64, 1, 0.536537521914_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 24.430142129257_f64, 0.0_f64),
            (3, 0, 0.615571013053_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.785356550293_f64, 0.0_f64),
            (37, 2, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.118942180943_f64, 0.0_f64),
            (16, 1, 0.379321903939_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -0.73657496892_f64, 0.0_f64),
            (54, 0, 58.891307380605_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (29, 0, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 4.845802158605_f64, 0.0_f64),
            (46, 1, -0.781972702142_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.0088103787_f64, 0.0_f64),
            (1, 1, -1.819858189307_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.012975327157_f64, 0.0_f64),
            (8, 0, 0.016422674249_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.006258679323_f64, 0.0_f64),
            (42, 0, 90.21492658429_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 4.0_f64, 0.0_f64),
            (20, 1, -0.038371582198_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000727402067_f64, 0.0_f64),
            (31, 1, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.21492658429_f64, 0.0_f64),
            (37, 2, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 13.103119286977_f64, 0.0_f64),
            (58, 3, 42.802729931752_f64, 61.254716857094_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 1.724176425078_f64, 0.0_f64),
            (43, 1, 16.087074533329_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (34, 0, 0.003961522568_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 24.430142129257_f64, 0.0_f64),
            (45, 1, 0.277103422926_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (66, 1, -0.002285631107_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.21492658429_f64, 0.0_f64),
            (6, 1, 0.019842055015_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.003883345889_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.712764986117_f64, 0.0_f64),
            (6, 0, 0.854118381787_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 98.026144238666_f64, 0.0_f64),
            (51, 1, 0.002986725743_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.156166218685_f64, 0.0_f64),
            (1, 1, -20.780423400716_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 20.878329763064_f64, 0.0_f64),
            (10, 1, -0.141889659972_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (37, 2, 16.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000193374386_f64, 0.0_f64),
            (60, 0, 0.000176479147_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 97.799245309998_f64, 0.0_f64),
            (11, 1, 47.005414521394_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.006258679323_f64, 0.0_f64),
            (59, 0, 87.588360619543_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (65, 1, -0.845591632774_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.398919564528_f64, 0.0_f64),
            (36, 1, 0.426292967876_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (62, 0, 0.012698614211_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (2, 0, 0.027526795523_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.003883345889_f64, 0.0_f64),
            (26, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (1, 1, -20.780423400716_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 80.15965415611_f64, 0.0_f64),
            (20, 1, 0.005497807331_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.836158938595_f64, 0.0_f64),
            (25, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (46, 1, -0.979734943474_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 4.0_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002433279017_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (62, 1, 0.000248581366_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.962352079221_f64, 0.0_f64),
            (16, 1, 0.379321903939_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008267982551_f64, 0.0_f64),
            (64, 1, 0.536537521914_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.735382052348_f64, 0.0_f64),
            (61, 0, 24.524084565759_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[(32, 1, 0.0_f64, 0.0_f64), (68, 2, 3.0_f64, 0.0_f64)],
    ),
    (
        false,
        &[
            (48, 1, 0.0_f64, 0.0_f64),
            (40, 1, -0.003731388935_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 6.452676568325_f64, 0.0_f64),
            (40, 0, 0.000677516823_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 89.80044543946_f64, 0.0_f64),
            (68, 2, 3.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.016780876652_f64, 0.0_f64),
            (10, 1, -0.479441712554_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (0, 0, 4.657215509588_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (32, 1, 0.0_f64, 0.0_f64),
            (14, 0, -0.73657496892_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.005317545063_f64, 0.0_f64),
            (36, 0, 0.712764986117_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.282482101409_f64, 0.0_f64),
            (37, 2, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (7, 1, -0.006812659013_f64, 0.0_f64),
            (24, 0, 0.117516911206_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 6.452676568325_f64, 0.0_f64),
            (0, 1, -1.915883231776_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 12.958361968728_f64, 0.0_f64),
            (65, 1, -1.385238691491_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.633462182502_f64, 0.0_f64),
            (22, 1, -0.006079005042_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 4.0_f64, 0.0_f64),
            (65, 1, -1.385238691491_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (1, 0, 4.325244011802_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 92.58675389879_f64, 0.0_f64),
            (13, 1, 0.306261093449_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001005628718_f64, 0.0_f64),
            (62, 1, 4.8914233e-05_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.582148468611_f64, 0.0_f64),
            (13, 1, 0.056247011082_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (32, 0, 5.0_f64, 0.0_f64),
            (43, 1, 17.976545306636_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.586231489434_f64, 0.0_f64),
            (36, 0, 0.637963905121_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.735382052348_f64, 0.0_f64),
            (43, 1, 24.709740053435_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.970296088074_f64, 0.0_f64),
            (57, 0, 1.841596579591_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (14, 1, -1.114097501656_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 83.042281112572_f64, 0.0_f64),
            (36, 1, 0.335678247138_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.114097501656_f64, 0.0_f64),
            (3, 0, 0.615571013053_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 24.430142129257_f64, 0.0_f64),
            (24, 0, 0.085104407075_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.829683244288_f64, 0.0_f64),
            (37, 2, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (36, 1, 0.378420082698_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.55850139458_f64, 0.0_f64),
            (65, 3, -0.481713759114_f64, -0.108072495769_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.785356550293_f64, 0.0_f64),
            (56, 1, 74.332784822998_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 85.785356550293_f64, 0.0_f64),
            (66, 1, 0.002475653874_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.836158938595_f64, 0.0_f64),
            (20, 0, 0.030139343022_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 70.510977130054_f64, 0.0_f64),
            (15, 1, 0.838389255232_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (37, 2, 20.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.595452540363_f64, 0.0_f64),
            (0, 1, -4.139667243924_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.056247011082_f64, 0.0_f64),
            (10, 1, -0.479441712554_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008267982551_f64, 0.0_f64),
            (16, 0, 2.255084993412_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.582148468611_f64, 0.0_f64),
            (40, 1, -0.000710524492_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 37.670808563448_f64, 0.0_f64),
            (15, 0, -0.382943541103_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (56, 0, 79.821411093328_f64, 0.0_f64),
            (0, 0, 4.657215509588_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 76.610575342271_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (61, 0, 24.524084565759_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.722377357787_f64, 0.0_f64),
            (1, 0, 25.309101221933_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 24.168754528768_f64, 0.0_f64),
            (20, 0, 0.030139343022_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (7, 0, 0.001375151652_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 1.371097431342_f64, 0.0_f64),
            (66, 0, 0.005828474127_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.030383565182_f64, 0.0_f64),
            (62, 1, 0.000147895196_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (37, 2, 16.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (46, 1, -1.422984964535_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.011063550376_f64, 0.0_f64),
            (12, 0, 114.585522147979_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.334336613744_f64, 0.0_f64),
            (43, 0, 75.524132937806_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (43, 1, 10.339569656362_f64, 0.0_f64),
            (34, 3, -0.001555592433_f64, 0.001707746585_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 88.325740640992_f64, 0.0_f64),
            (59, 1, 83.360348764515_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 83.360348764515_f64, 0.0_f64),
            (11, 1, -19.619345111905_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, -0.022563546352_f64, 0.0_f64),
            (60, 0, 0.012910358989_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.829683244288_f64, 0.0_f64),
            (20, 0, 0.010985554175_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.55850139458_f64, 0.0_f64),
            (16, 1, 0.499150169133_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.016780876652_f64, 0.0_f64),
            (37, 2, 16.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.80253046606_f64, 0.0_f64),
            (6, 1, 0.076960033045_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.712764986117_f64, 0.0_f64),
            (37, 2, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 5.0_f64, 0.0_f64),
            (40, 1, -0.001211964591_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 6.0_f64, 0.0_f64), (37, 2, 20.0_f64, 0.0_f64)],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (6, 1, 0.029598827543_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (37, 2, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.363830865862_f64, 0.0_f64),
            (40, 0, 0.004414767382_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.582148468611_f64, 0.0_f64),
            (1, 1, -20.780423400716_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.586231489434_f64, 0.0_f64),
            (0, 1, -7.107013931768_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.970296088074_f64, 0.0_f64),
            (35, 1, 0.118286862474_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (42, 1, 5.829683244288_f64, 0.0_f64),
            (30, 0, 9.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 6.452676568325_f64, 0.0_f64),
            (65, 1, -0.947922061053_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.011063550376_f64, 0.0_f64),
            (42, 0, 80.653106179443_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 70.510977130054_f64, 0.0_f64),
            (16, 1, 0.451141769134_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.251600440171_f64, 0.0_f64),
            (37, 2, 13.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (2, 0, 0.023002995475_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 24.975510474295_f64, 0.0_f64),
            (38, 1, 0.000193642335_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (13, 1, 0.089213320949_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.712764986117_f64, 0.0_f64),
            (37, 2, 12.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 6.452676568325_f64, 0.0_f64),
            (37, 2, 21.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.011063550376_f64, 0.0_f64),
            (42, 0, 73.738981176679_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 6.452676568325_f64, 0.0_f64),
            (37, 2, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001005628718_f64, 0.0_f64),
            (40, 1, -0.00032682283_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 87.588360619543_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (37, 2, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (29, 0, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (0, 1, -1.167889331231_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 6.0_f64, 0.0_f64),
            (16, 1, 0.590766072643_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000394386125_f64, 0.0_f64),
            (51, 1, 0.002986725743_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[(33, 0, 6.0_f64, 0.0_f64), (37, 2, 23.0_f64, 0.0_f64)],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (51, 1, -0.002741635191_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (11, 1, -89.913738186233_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (36, 1, 0.405699915549_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.212888338411_f64, 0.0_f64),
            (64, 1, 0.294692937497_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 27.185662222686_f64, 0.0_f64),
            (17, 0, -0.005522142294_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.001555592433_f64, 0.0_f64),
            (55, 0, 72.617548923107_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 70.510977130054_f64, 0.0_f64),
            (1, 1, -20.780423400716_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 9.266584745766_f64, 0.0_f64),
            (16, 1, 0.379321903939_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 89.80044543946_f64, 0.0_f64),
            (68, 2, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (64, 1, 1.57370732418_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.089213320949_f64, 0.0_f64),
            (52, 0, 0.042397489333_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 83.360348764515_f64, 0.0_f64),
            (65, 1, -1.509021822217_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.002897737883_f64, 0.0_f64),
            (12, 0, 127.802445543786_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (3, 1, 0.083867390324_f64, 0.0_f64),
            (65, 1, -1.100475105203_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 94.633462182502_f64, 0.0_f64),
            (38, 0, 0.009143642117_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008267982551_f64, 0.0_f64),
            (30, 0, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.008267982551_f64, 0.0_f64),
            (22, 0, -0.010784453702_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (35, 1, 0.008590107324_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.160650711145_f64, 0.0_f64),
            (33, 0, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.002878374423_f64, 0.0_f64),
            (24, 0, 0.052481730152_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.207690553993_f64, 0.0_f64),
            (0, 1, -21.225581979105_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 15.184312318203_f64, 0.0_f64),
            (46, 1, -0.464821755383_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.405699915549_f64, 0.0_f64),
            (34, 0, 0.008514598019_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 92.538838851052_f64, 0.0_f64),
            (37, 2, 12.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 90.21492658429_f64, 0.0_f64),
            (6, 0, 0.949867547166_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 82.208371283464_f64, 0.0_f64),
            (13, 1, 0.000186502931_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.000186502931_f64, 0.0_f64),
            (37, 2, 12.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (11, 1, -264.440276366037_f64, 0.0_f64),
            (36, 0, 0.588083928324_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.004499095898_f64, 0.0_f64),
            (16, 0, 2.255084993412_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 24.393415791832_f64, 0.0_f64),
            (46, 1, -1.033363918481_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.003202986598_f64, 0.0_f64),
            (20, 1, -0.020295034267_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.022037703312_f64, 0.0_f64),
            (44, 0, 1320.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.55850139458_f64, 0.0_f64),
            (37, 2, 7.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002077937651_f64, 0.0_f64),
            (61, 0, 102.022412698827_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -2.118942180943_f64, 0.0_f64),
            (66, 0, 0.01121863691_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 78.148726107272_f64, 0.0_f64),
            (43, 1, 44.954532800507_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (54, 0, 78.148726107272_f64, 0.0_f64),
            (42, 1, 55.944196476296_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (24, 0, 0.052481730152_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.002456946598_f64, 0.0_f64),
            (16, 0, 1.804032915518_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.022037703312_f64, 0.0_f64),
            (16, 0, 1.906223811824_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.962352079221_f64, 0.0_f64),
            (5, 1, 0.000391630717_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (55, 0, 76.610575342271_f64, 0.0_f64),
            (1, 0, 13.438071751687_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.001239879895_f64, 0.0_f64),
            (37, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.000727402067_f64, 0.0_f64),
            (57, 1, 0.719948712472_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.001555592433_f64, 0.0_f64),
            (56, 0, 74.332784822998_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (43, 0, 82.208371283464_f64, 0.0_f64),
            (1, 1, -10.610519151485_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 96.0335550175_f64, 0.0_f64),
            (37, 2, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 2.667588714436_f64, 0.0_f64),
            (37, 2, 3.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.251600440171_f64, 0.0_f64),
            (62, 1, 0.000495743204_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 13.103119286977_f64, 0.0_f64),
            (16, 0, 2.255084993412_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.000409020996_f64, 0.0_f64),
            (37, 2, 12.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.307122278049_f64, 0.0_f64),
            (51, 0, 0.00138724474_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 96.3831114696_f64, 0.0_f64),
            (43, 1, 33.955352912075_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (56, 1, 21.973808882615_f64, 0.0_f64),
            (45, 1, 0.577761317438_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.271737232917_f64, 0.0_f64),
            (47, 1, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 12.958361968728_f64, 0.0_f64),
            (65, 1, -0.99874379869_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.595452540363_f64, 0.0_f64),
            (1, 1, -1.081483750837_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.016780876652_f64, 0.0_f64),
            (37, 2, 2.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 6.452676568325_f64, 0.0_f64),
            (62, 0, 0.017899964951_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.006258679323_f64, 0.0_f64),
            (42, 0, 88.271737232917_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (32, 0, 5.0_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (49, 0, 2.0_f64, 0.0_f64),
            (41, 0, 0.020512579868_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (37, 2, 0.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (14, 0, 1.722377357787_f64, 0.0_f64),
            (61, 0, 15.903107859896_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 5.0_f64, 0.0_f64),
            (54, 3, 46.363173508285_f64, 54.618381008027_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.001005628718_f64, 0.0_f64),
            (62, 1, 0.000147895196_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (7, 1, -0.009512380842_f64, 0.0_f64),
            (0, 1, -10.65253216544_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (50, 1, -0.003848934884_f64, 0.0_f64),
            (3, 0, 1.023461413393_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.353463333651_f64, 0.0_f64),
            (37, 2, 3.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (54, 1, 22.87928198245_f64, 0.0_f64),
            (16, 1, 0.499150169133_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.735382052348_f64, 0.0_f64),
            (37, 2, 13.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.005802879975_f64, 0.0_f64),
            (36, 0, 0.735382052348_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002077937651_f64, 0.0_f64),
            (11, 1, -71.601769964354_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (23, 1, 0.000905802239_f64, 0.0_f64),
            (66, 0, 0.005828474127_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 1.371097431342_f64, 0.0_f64),
            (43, 1, 21.227386631273_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 89.373499384241_f64, 0.0_f64),
            (58, 1, 70.210545155072_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (32, 0, 5.0_f64, 0.0_f64),
            (55, 1, 34.5377351189_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 83.042281112572_f64, 0.0_f64),
            (30, 0, 11.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.0088103787_f64, 0.0_f64),
            (65, 1, -0.99874379869_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (21, 0, -0.002433279017_f64, 0.0_f64),
            (28, 0, 1.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (42, 0, 88.271737232917_f64, 0.0_f64),
            (35, 1, 0.039537406427_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (17, 1, -0.002920351157_f64, 0.0_f64),
            (63, 1, 0.00138839162_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.056247011082_f64, 0.0_f64),
            (30, 1, 3.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.002090782791_f64, 0.0_f64),
            (4, 0, 0.000828634832_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 87.582148468611_f64, 0.0_f64),
            (13, 1, 0.110521327309_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (34, 1, -0.0088103787_f64, 0.0_f64),
            (8, 3, -0.001669407072_f64, 0.001904120338_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.712764986117_f64, 0.0_f64),
            (62, 1, 0.00010233672_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.735382052348_f64, 0.0_f64),
            (6, 0, 0.784092186466_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.160650711145_f64, 0.0_f64),
            (5, 1, 3.6818679e-05_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.69192343888_f64, 0.0_f64),
            (65, 1, -1.100475105203_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (37, 2, 10.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.218185817798_f64, 0.0_f64),
            (44, 0, 1320.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.251600440171_f64, 0.0_f64),
            (14, 0, -1.114097501656_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 92.398919564528_f64, 0.0_f64),
            (58, 1, 78.724397723401_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 89.541313522713_f64, 0.0_f64),
            (36, 1, 0.405699915549_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -2.353463333651_f64, 0.0_f64),
            (37, 2, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (59, 0, 87.588360619543_f64, 0.0_f64),
            (42, 1, 30.043429254626_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.735382052348_f64, 0.0_f64),
            (10, 0, 2.419184730137_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (7, 0, 0.02269168662_f64, 0.0_f64),
            (63, 1, 0.003091757798_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (33, 0, 4.0_f64, 0.0_f64),
            (64, 1, 0.319900011809_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.335678247138_f64, 0.0_f64),
            (40, 0, 0.002443200772_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (51, 1, -0.014926060763_f64, 0.0_f64),
            (29, 0, 6.0_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (22, 0, -0.003202986598_f64, 0.0_f64),
            (40, 1, -0.000710524492_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.030383565182_f64, 0.0_f64),
            (62, 0, 0.021833334011_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 20.878329763064_f64, 0.0_f64),
            (65, 1, -0.765737264252_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (15, 1, -1.397952097938_f64, 0.0_f64),
            (32, 0, 5.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 5.251600440171_f64, 0.0_f64),
            (50, 0, -0.000749714289_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.013135821846_f64, 0.0_f64),
            (16, 0, 2.080444997831_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.682488787105_f64, 0.0_f64),
            (0, 0, 14.249712652214_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (32, 0, 5.0_f64, 0.0_f64),
            (46, 1, -1.422984964535_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (58, 1, 2.667588714436_f64, 0.0_f64),
            (37, 2, 9.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (59, 1, 9.711755951452_f64, 0.0_f64),
            (64, 1, 0.357082553591_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (24, 1, 0.002900258506_f64, 0.0_f64),
            (66, 0, 0.004380073053_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (4, 1, -0.013135821846_f64, 0.0_f64),
            (30, 0, 10.0_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (13, 1, 0.056247011082_f64, 0.0_f64),
            (51, 0, 0.019257684619_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (14, 1, -1.334336613744_f64, 0.0_f64),
            (22, 0, -0.003202986598_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (36, 0, 0.712764986117_f64, 0.0_f64),
            (42, 1, 21.010779204058_f64, 0.0_f64),
        ],
    ),
    (
        false,
        &[
            (58, 0, 83.042281112572_f64, 0.0_f64),
            (52, 1, -0.003871656787_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (55, 1, 30.415962356833_f64, 0.0_f64),
            (36, 0, 0.682488787105_f64, 0.0_f64),
        ],
    ),
    (
        true,
        &[
            (36, 1, 0.426292967876_f64, 0.0_f64),
            (4, 0, 0.021420937506_f64, 0.0_f64),
        ],
    ),
];

pub struct FiveYear70PctBtcH1Rules586 {
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

impl FiveYear70PctBtcH1Rules586 {
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

impl Strategy for FiveYear70PctBtcH1Rules586 {
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
