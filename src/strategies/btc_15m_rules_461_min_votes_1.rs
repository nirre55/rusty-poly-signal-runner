use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "btc_15m_rules_461_min_votes_1";
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
// 31=failed_high24
// 32=failed_low12
// 33=failed_low24
// 34=flip_count12
// 35=flip_count6
// 36=green_count3
// 37=green_count6
// 38=green_streak
// 39=ha_body
// 40=ha_body_ratio
// 41=ha_close_position
// 42=hour
// 43=lower_wick
// 44=lower_wick_body
// 45=macd_hist_pct
// 46=macd_pct
// 47=mfi14
// 48=mfi21
// 49=mfi8
// 50=minute_of_day
// 51=range_atr14
// 52=range_pct_z24
// 53=red_count6
// 54=red_streak
// 55=ret1
// 56=ret12
// 57=ret24
// 58=ret3
// 59=ret6
// 60=rsi14
// 61=rsi21
// 62=rsi7
// 63=rsi8
// 64=same_color_ratio12
// 65=session_asia
// 66=session_london
// 67=session_overlap_london_us
// 68=session_us
// 69=signed_volume_ratio20
// 70=stoch_k12
// 71=stoch_k24
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
    f[31] = failed_high(buf, 24);
    f[32] = failed_low(buf, 12);
    f[33] = failed_low(buf, 24);
    f[34] = flip_count(buf, 12);
    f[35] = flip_count(buf, 6);
    f[36] = count_color(buf, 3, true);
    f[37] = count_color(buf, 6, true);
    f[38] = Some(green_streak(buf));
    f[39] = ha.ha_body;
    f[40] = ha.ha_body_ratio;
    f[41] = ha.ha_close_pos;
    f[42] = Some(hour);
    f[43] = lower_wick;
    f[44] = lower_wick_body;
    f[45] = macd.hist_pct(close);
    f[46] = macd.line_pct(close);
    f[47] = mfi_n(buf, 14);
    f[48] = mfi_n(buf, 21);
    f[49] = mfi_n(buf, 8);
    f[50] = Some(minute_of_day);
    f[51] = range_atr14(buf, atr14_ewm.raw());
    f[52] = range_pct_z(buf, 24);
    f[53] = count_color(buf, 6, false);
    f[54] = Some(red_streak(buf));
    f[55] = ret_n(buf, 1);
    f[56] = ret_n(buf, 12);
    f[57] = ret_n(buf, 24);
    f[58] = ret_n(buf, 3);
    f[59] = ret_n(buf, 6);
    f[60] = rsi14.get();
    f[61] = rsi21.get();
    f[62] = rsi7.get();
    f[63] = rsi8.get();
    f[64] = same_color_ratio(buf, 12);
    f[65] = Some(session_asia(minute_of_day));
    f[66] = Some(session_london(minute_of_day));
    f[67] = Some(session_overlap_london_us(minute_of_day));
    f[68] = Some(session_us(minute_of_day));
    f[69] = signed_vol_ratio(buf, 20);
    f[70] = stoch_k(buf, 12, close);
    f[71] = stoch_k(buf, 24, close);
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
    // 1 btcusdt_15m_rules_1: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Le(19, 0.457331193089_f64),
            Cond::In(
                42,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 2 btcusdt_15m_rules_2: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000430553703_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 3 btcusdt_15m_rules_3: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.679463493_f64),
            Cond::Ge(6, 0.008140445126_f64),
            Cond::Le(46, -0.00345019142_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 4 btcusdt_15m_rules_4: GREEN
    (
        true,
        &[
            Cond::Le(71, 13.599250472473_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 5 btcusdt_15m_rules_5: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Ge(57, -0.01042349892_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 6 btcusdt_15m_rules_6: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Ge(2, 0.015683885774_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 7 btcusdt_15m_rules_7: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000917948329_f64),
            Cond::Le(13, -19.900866452413_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 8 btcusdt_15m_rules_8: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000430553703_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 9 btcusdt_15m_rules_9: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.667588714436_f64),
            Cond::In(42, &[3.0_f64]),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 10 btcusdt_15m_rules_10: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Le(15, 0.002359360549_f64),
            Cond::Ge(62, 28.1055143_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 11 btcusdt_15m_rules_11: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.012972255848_f64),
            Cond::Ge(25, -0.00215355613_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 12 btcusdt_15m_rules_12: GREEN
    (
        true,
        &[
            Cond::Le(47, 21.44107346_f64),
            Cond::Le(15, 0.1223925466_f64),
            Cond::Ge(44, 0.06465758156_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 13 btcusdt_15m_rules_13: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000518047549_f64),
            Cond::Ge(49, 62.524777166172_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 14 btcusdt_15m_rules_14: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000596058362_f64),
            Cond::Ge(0, 13.480756029953_f64),
        ],
    ),
    // 15 btcusdt_15m_rules_15: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.210641246093_f64),
            Cond::Ge(29, 0.01508702537_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 16 btcusdt_15m_rules_16: RED
    (
        false,
        &[
            Cond::Ge(41, 0.660005226124_f64),
            Cond::Ge(2, 0.014808079873_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 17 btcusdt_15m_rules_17: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.286381306679_f64),
            Cond::Ge(45, -0.000433596303_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 18 btcusdt_15m_rules_18: RED
    (
        false,
        &[
            Cond::Ge(63, 80.15965415611_f64),
            Cond::Le(52, -0.979734943474_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 19 btcusdt_15m_rules_19: RED
    (
        false,
        &[
            Cond::Ge(70, 98.683285404721_f64),
            Cond::Ge(38, 6.0_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 20 btcusdt_15m_rules_20: RED
    (
        false,
        &[Cond::Le(53, 0.0_f64), Cond::Le(20, 0.000731601776_f64)],
    ),
    // 21 btcusdt_15m_rules_21: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.26951785_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(12, -191.4788279_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 22 btcusdt_15m_rules_22: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.77000366238_f64),
            Cond::Between(9, -0.001606479749_f64, 0.001843361495_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 23 btcusdt_15m_rules_23: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Ge(20, -0.004060754253_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 24 btcusdt_15m_rules_24: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000409020996_f64),
            Cond::In(42, &[12.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 25 btcusdt_15m_rules_25: RED
    (
        false,
        &[
            Cond::Ge(41, 0.644678507722_f64),
            Cond::Ge(6, 0.010655328013_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 26 btcusdt_15m_rules_26: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.724176425078_f64),
            Cond::Ge(39, -0.002897737883_f64),
            Cond::Eq(42, 0.0_f64),
        ],
    ),
    // 27 btcusdt_15m_rules_27: RED
    (
        false,
        &[
            Cond::Ge(4, 0.912053646031_f64),
            Cond::Le(17, 0.453097119828_f64),
        ],
    ),
    // 28 btcusdt_15m_rules_28: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.005348262374_f64),
            Cond::Ge(60, 69.060618393279_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 29 btcusdt_15m_rules_29: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(52, 0.245066079634_f64),
        ],
    ),
    // 30 btcusdt_15m_rules_30: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.503757061587_f64),
            Cond::Between(44, 0.276646203011_f64, 1.3696048831_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 31 btcusdt_15m_rules_31: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Le(2, 0.002837437065_f64),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 32 btcusdt_15m_rules_32: GREEN
    (
        true,
        &[
            Cond::Le(62, 37.285529400902_f64),
            Cond::Ge(80, 0.021524632953_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 33 btcusdt_15m_rules_33: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.69192343888_f64),
            Cond::Le(77, -1.100475105203_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 34 btcusdt_15m_rules_34: GREEN
    (
        true,
        &[
            Cond::Le(62, 11.111136496641_f64),
            Cond::Le(80, -0.024921049724_f64),
        ],
    ),
    // 35 btcusdt_15m_rules_35: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202478662_f64),
            Cond::Le(76, 1.758262671_f64),
            Cond::Le(78, 0.7249823529_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 36 btcusdt_15m_rules_36: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000940217931_f64),
            Cond::Ge(19, 2.222113870768_f64),
        ],
    ),
    // 37 btcusdt_15m_rules_37: RED
    (
        false,
        &[
            Cond::Ge(17, 2.475880255608_f64),
            Cond::Le(80, -0.008414798179_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 38 btcusdt_15m_rules_38: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(44, 0.01349188119_f64),
            Cond::Le(72, 0.00008848352749_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 39 btcusdt_15m_rules_39: RED
    (
        false,
        &[
            Cond::Ge(62, 64.191554432408_f64),
            Cond::Le(27, 0.002878374423_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 40 btcusdt_15m_rules_40: RED
    (
        false,
        &[
            Cond::Ge(49, 90.21492658429_f64),
            Cond::In(42, &[5.0_f64]),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 41 btcusdt_15m_rules_41: GREEN
    (
        true,
        &[
            Cond::Le(70, 6.452676568325_f64),
            Cond::In(42, &[21.0_f64]),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 42 btcusdt_15m_rules_42: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(79, -0.003994395698_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 43 btcusdt_15m_rules_43: RED
    (
        false,
        &[
            Cond::Ge(16, 1.962352079221_f64),
            Cond::Le(2, 0.001367718303_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 44 btcusdt_15m_rules_44: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.353463333651_f64),
            Cond::In(42, &[5.0_f64]),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 45 btcusdt_15m_rules_45: RED
    (
        false,
        &[
            Cond::Ge(70, 87.582148468611_f64),
            Cond::Le(45, -0.000710524492_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 46 btcusdt_15m_rules_46: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.062588143616_f64),
            Cond::In(81, &[6.0_f64]),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 47 btcusdt_15m_rules_47: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(6, 0.000049170938_f64),
            Cond::Eq(42, 19.0_f64),
        ],
    ),
    // 48 btcusdt_15m_rules_48: GREEN
    (
        true,
        &[
            Cond::Le(47, 10.339569656362_f64),
            Cond::Le(43, 0.000468970418_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 49 btcusdt_15m_rules_49: GREEN
    (
        true,
        &[
            Cond::Le(12, -168.9532813_f64),
            Cond::Le(44, 0.01349188119_f64),
            Cond::Ge(21, -0.005575910157_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 50 btcusdt_15m_rules_50: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(75, 0.007974367829_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 51 btcusdt_15m_rules_51: RED
    (
        false,
        &[
            Cond::Ge(38, 6.0_f64),
            Cond::Le(52, -1.12961546566_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 52 btcusdt_15m_rules_52: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.056247011082_f64),
            Cond::Le(34, 3.0_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 53 btcusdt_15m_rules_53: RED
    (
        false,
        &[
            Cond::Ge(24, -0.001486921254_f64),
            Cond::Ge(0, 14.249712652214_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 54 btcusdt_15m_rules_54: GREEN
    (
        true,
        &[Cond::Le(12, -169.388968825084_f64), Cond::Ge(15, 1.0_f64)],
    ),
    // 55 btcusdt_15m_rules_55: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 56 btcusdt_15m_rules_56: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 57 btcusdt_15m_rules_57: GREEN
    (
        true,
        &[
            Cond::Le(48, 20.65922058_f64),
            Cond::Eq(81, 2.0_f64),
            Cond::Le(62, 33.60114385_f64),
            Cond::Eq(42, 17.0_f64),
        ],
    ),
    // 58 btcusdt_15m_rules_58: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000394386125_f64),
            Cond::Le(51, 0.663177938908_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 59 btcusdt_15m_rules_59: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.359680944_f64),
            Cond::Le(2, 0.001795740443_f64),
            Cond::Le(4, -0.07487622772_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 60 btcusdt_15m_rules_60: GREEN
    (
        true,
        &[
            Cond::Le(70, 0.5066458518_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(43, 9.29093662300000e-8_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 61 btcusdt_15m_rules_61: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(60, 35.01130025_f64),
            Cond::Le(21, -0.003142104413_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 62 btcusdt_15m_rules_62: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.679463493_f64),
            Cond::Ge(6, 0.008140445126_f64),
            Cond::Le(46, -0.00345019142_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 63 btcusdt_15m_rules_63: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.008267982551_f64),
            Cond::Le(76, 0.536537521914_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 64 btcusdt_15m_rules_64: GREEN
    (
        true,
        &[
            Cond::Le(45, -0.002650404386_f64),
            Cond::Le(70, 5.152344313_f64),
            Cond::Le(71, 3.478803314_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 65 btcusdt_15m_rules_65: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.005802879975_f64),
            Cond::Le(76, 0.226931525872_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 66 btcusdt_15m_rules_66: GREEN
    (
        true,
        &[
            Cond::Le(49, 9.919091826186_f64),
            Cond::Ge(8, 0.003864538504_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 67 btcusdt_15m_rules_67: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(44, 0.01349188119_f64),
            Cond::Ge(63, 29.60116771_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 68 btcusdt_15m_rules_68: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000123157637_f64),
            Cond::Le(1, -0.904211298052_f64),
        ],
    ),
    // 69 btcusdt_15m_rules_69: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Ge(23, 0.043522704487_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 70 btcusdt_15m_rules_70: RED
    (
        false,
        &[
            Cond::Ge(70, 92.430123495695_f64),
            Cond::Le(76, 0.283114920997_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 71 btcusdt_15m_rules_71: RED
    (
        false,
        &[
            Cond::Ge(38, 5.0_f64),
            Cond::Ge(74, 0.006286214631_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 72 btcusdt_15m_rules_72: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000727402067_f64),
            Cond::Le(11, -0.479441712554_f64),
            Cond::Eq(42, 2.0_f64),
        ],
    ),
    // 73 btcusdt_15m_rules_73: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Between(77, -0.470588798961_f64, -0.128994916769_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 74 btcusdt_15m_rules_74: RED
    (
        false,
        &[
            Cond::Ge(47, 89.80044543946_f64),
            Cond::In(81, &[3.0_f64]),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 75 btcusdt_15m_rules_75: GREEN
    (
        true,
        &[
            Cond::Le(52, -1.536658910705_f64),
            Cond::Le(71, 24.168754528768_f64),
            Cond::Eq(42, 20.0_f64),
        ],
    ),
    // 76 btcusdt_15m_rules_76: RED
    (
        false,
        &[
            Cond::Ge(63, 82.454573967079_f64),
            Cond::Between(79, -0.001328609306_f64, 0.001518218635_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 77 btcusdt_15m_rules_77: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.091431392_f64),
            Cond::Le(10, -0.01392502169_f64),
            Cond::Le(37, 1.0_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 78 btcusdt_15m_rules_78: RED
    (
        false,
        &[
            Cond::Ge(71, 96.0335550175_f64),
            Cond::In(42, &[11.0_f64]),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 79 btcusdt_15m_rules_79: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.005731108736_f64),
            Cond::Ge(30, 0.02923394723_f64),
            Cond::Ge(28, 0.04226528076_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 80 btcusdt_15m_rules_80: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.334336613744_f64),
            Cond::Ge(25, -0.003202986598_f64),
            Cond::Eq(42, 14.0_f64),
        ],
    ),
    // 81 btcusdt_15m_rules_81: GREEN
    (
        true,
        &[
            Cond::Le(62, 30.319177476136_f64),
            Cond::Le(19, 0.368391267019_f64),
        ],
    ),
    // 82 btcusdt_15m_rules_82: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.00834711336_f64),
            Cond::Le(34, 2.0_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 83 btcusdt_15m_rules_83: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.336010955575_f64),
            Cond::Le(76, 0.247637271351_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 84 btcusdt_15m_rules_84: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.503757061587_f64),
            Cond::Between(44, 0.276646203011_f64, 1.3696048831_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 85 btcusdt_15m_rules_85: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.048300974027_f64),
            Cond::Le(11, -0.887448173015_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 86 btcusdt_15m_rules_86: RED
    (
        false,
        &[
            Cond::Ge(41, 0.735382052348_f64),
            Cond::Ge(7, 0.784092186466_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 87 btcusdt_15m_rules_87: RED
    (
        false,
        &[
            Cond::Ge(70, 97.75223967825_f64),
            Cond::Le(45, -0.000433596303_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 88 btcusdt_15m_rules_88: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000053521742_f64),
            Cond::Ge(73, 0.056497175141_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 89 btcusdt_15m_rules_89: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.212888338411_f64),
            Cond::Le(76, 0.294692937497_f64),
            Cond::Eq(42, 20.0_f64),
        ],
    ),
    // 90 btcusdt_15m_rules_90: GREEN
    (
        true,
        &[
            Cond::Le(47, 12.370381786246_f64),
            Cond::Ge(45, -0.00093425444_f64),
            Cond::Eq(42, 2.0_f64),
        ],
    ),
    // 91 btcusdt_15m_rules_91: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Between(56, -0.003610257481_f64, 0.004009867371_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 92 btcusdt_15m_rules_92: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.667588714436_f64),
            Cond::In(42, &[9.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 93 btcusdt_15m_rules_93: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.218185817798_f64),
            Cond::Le(19, 0.712660732397_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 94 btcusdt_15m_rules_94: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000024352929_f64),
            Cond::Ge(64, 0.833333333333_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 95 btcusdt_15m_rules_95: RED
    (
        false,
        &[
            Cond::Ge(41, 0.712764986117_f64),
            Cond::Le(74, 0.00010233672_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 96 btcusdt_15m_rules_96: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000634394237_f64),
            Cond::Ge(47, 72.869843421606_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 97 btcusdt_15m_rules_97: GREEN
    (
        true,
        &[
            Cond::Le(60, 27.902869321396_f64),
            Cond::Ge(79, -0.001373496039_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 98 btcusdt_15m_rules_98: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Le(19, 0.457331193089_f64),
        ],
    ),
    // 99 btcusdt_15m_rules_99: RED
    (
        false,
        &[
            Cond::Ge(16, 1.908788195242_f64),
            Cond::Le(77, -1.1448685998_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 100 btcusdt_15m_rules_100: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.051658096366_f64),
            Cond::Ge(45, 0.000552259724_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 101 btcusdt_15m_rules_101: RED
    (
        false,
        &[
            Cond::Ge(17, 2.562798212558_f64),
            Cond::Le(75, 0.000742956712_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 102 btcusdt_15m_rules_102: RED
    (
        false,
        &[Cond::Le(53, 0.0_f64), Cond::Le(60, 44.801323047453_f64)],
    ),
    // 103 btcusdt_15m_rules_103: GREEN
    (
        true,
        &[
            Cond::Le(52, -1.536658910705_f64),
            Cond::Le(22, -0.004457586342_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 104 btcusdt_15m_rules_104: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000547061338_f64),
            Cond::In(81, &[5.0_f64]),
            Cond::Eq(42, 7.0_f64),
        ],
    ),
    // 105 btcusdt_15m_rules_105: GREEN
    (
        true,
        &[
            Cond::Le(47, 15.30340668771_f64),
            Cond::Le(40, 0.040104438977_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 106 btcusdt_15m_rules_106: RED
    (
        false,
        &[
            Cond::Ge(38, 5.0_f64),
            Cond::Ge(44, 44.037371685378_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 107 btcusdt_15m_rules_107: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.210641246093_f64),
            Cond::Ge(23, 0.007795884974_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 108 btcusdt_15m_rules_108: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Ge(1, 4.325244011802_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 109 btcusdt_15m_rules_109: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.124295441041_f64),
            Cond::Between(52, -0.464230165361_f64, -0.047403807875_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 110 btcusdt_15m_rules_110: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.724176425078_f64),
            Cond::Ge(39, -0.002897737883_f64),
            Cond::Eq(42, 17.0_f64),
        ],
    ),
    // 111 btcusdt_15m_rules_111: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.004596587699_f64),
            Cond::Le(2, 0.001646300512_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 112 btcusdt_15m_rules_112: RED
    (
        false,
        &[
            Cond::Ge(16, 2.082088491926_f64),
            Cond::Le(46, -0.004456144174_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 113 btcusdt_15m_rules_113: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.008267982551_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 114 btcusdt_15m_rules_114: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(51, 1.5_f64),
            Cond::Ge(7, 0.6_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 115 btcusdt_15m_rules_115: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202478662_f64),
            Cond::Le(76, 1.758262671_f64),
            Cond::Le(78, 0.7249823529_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 116 btcusdt_15m_rules_116: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Between(56, -0.003610257481_f64, 0.004009867371_f64),
            Cond::Eq(42, 15.0_f64),
        ],
    ),
    // 117 btcusdt_15m_rules_117: GREEN
    (
        true,
        &[
            Cond::Le(49, 0.0_f64),
            Cond::Ge(12, -72.536398228737_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 118 btcusdt_15m_rules_118: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00121384214_f64),
            Cond::Le(11, -0.479441712554_f64),
            Cond::Eq(42, 16.0_f64),
        ],
    ),
    // 119 btcusdt_15m_rules_119: RED
    (
        false,
        &[
            Cond::Ge(60, 82.623793495996_f64),
            Cond::Between(2, 0.006117730378_f64, 0.007861395294_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 120 btcusdt_15m_rules_120: RED
    (
        false,
        &[
            Cond::Ge(41, 0.682488787105_f64),
            Cond::Le(1, -20.780423400716_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 121 btcusdt_15m_rules_121: RED
    (
        false,
        &[
            Cond::Ge(70, 87.60245942582_f64),
            Cond::Le(13, -44.511965767527_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 122 btcusdt_15m_rules_122: GREEN
    (
        true,
        &[
            Cond::Le(63, 12.502131887069_f64),
            Cond::Ge(20, -0.016588506863_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 123 btcusdt_15m_rules_123: RED
    (
        false,
        &[
            Cond::Ge(16, 1.595452540363_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 124 btcusdt_15m_rules_124: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.404114354323_f64),
            Cond::Ge(10, -0.005516138794_f64),
            Cond::Eq(42, 18.0_f64),
        ],
    ),
    // 125 btcusdt_15m_rules_125: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(79, -0.000993286689_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 126 btcusdt_15m_rules_126: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000685074848_f64),
            Cond::Le(69, -1.35609473535_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 127 btcusdt_15m_rules_127: GREEN
    (
        true,
        &[
            Cond::Le(60, 34.187315401094_f64),
            Cond::Le(2, 0.000913763684_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 128 btcusdt_15m_rules_128: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.812368993449_f64),
            Cond::Ge(20, -0.000598615933_f64),
        ],
    ),
    // 129 btcusdt_15m_rules_129: RED
    (
        false,
        &[
            Cond::Ge(71, 97.684471745776_f64),
            Cond::Le(7, 0.264860593835_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 130 btcusdt_15m_rules_130: GREEN
    (
        true,
        &[
            Cond::Le(63, 26.021412739387_f64),
            Cond::Ge(23, 0.007795884974_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 131 btcusdt_15m_rules_131: GREEN
    (
        true,
        &[
            Cond::Le(52, -1.536658910705_f64),
            Cond::Le(22, -0.004457586342_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 132 btcusdt_15m_rules_132: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.030383565182_f64),
            Cond::Le(74, 0.000147895196_f64),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 133 btcusdt_15m_rules_133: RED
    (
        false,
        &[
            Cond::Ge(63, 68.704859813649_f64),
            Cond::Le(77, -1.459564445306_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 134 btcusdt_15m_rules_134: RED
    (
        false,
        &[
            Cond::Ge(17, 1.834921291675_f64),
            Cond::Le(52, -1.302770069233_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 135 btcusdt_15m_rules_135: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Between(49, 37.154692990356_f64, 62.524777166172_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 136 btcusdt_15m_rules_136: RED
    (
        false,
        &[
            Cond::Ge(25, -0.003202986598_f64),
            Cond::Le(45, -0.000710524492_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 137 btcusdt_15m_rules_137: RED
    (
        false,
        &[
            Cond::Ge(70, 95.36043284_f64),
            Cond::Ge(2, 0.005489115066_f64),
            Cond::Le(3, 0.003517992609_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 138 btcusdt_15m_rules_138: RED
    (
        false,
        &[
            Cond::Ge(71, 92.847408686743_f64),
            Cond::Le(40, 0.017282150796_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 139 btcusdt_15m_rules_139: GREEN
    (
        true,
        &[Cond::Le(29, 0.000123157637_f64), Cond::In(42, &[22.0_f64])],
    ),
    // 140 btcusdt_15m_rules_140: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.008177319388_f64),
            Cond::Ge(37, 5.0_f64),
            Cond::Eq(42, 11.0_f64),
        ],
    ),
    // 141 btcusdt_15m_rules_141: RED
    (
        false,
        &[
            Cond::Ge(38, 5.0_f64),
            Cond::Le(77, -1.159346030872_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 142 btcusdt_15m_rules_142: RED
    (
        false,
        &[
            Cond::Ge(38, 6.0_f64),
            Cond::Ge(2, 0.007385036543_f64),
            Cond::Eq(42, 16.0_f64),
        ],
    ),
    // 143 btcusdt_15m_rules_143: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.840110258471_f64),
            Cond::Le(40, 0.040104438977_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 144 btcusdt_15m_rules_144: RED
    (
        false,
        &[
            Cond::Ge(71, 92.499098560538_f64),
            Cond::Le(47, 39.31642972534_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 145 btcusdt_15m_rules_145: RED
    (
        false,
        &[
            Cond::Ge(70, 95.36043284_f64),
            Cond::Ge(8, 0.02432417868_f64),
            Cond::Le(43, 0.001638794532_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 146 btcusdt_15m_rules_146: GREEN
    (
        true,
        &[
            Cond::Le(62, 16.441111837773_f64),
            Cond::Between(58, -0.003698886648_f64, 0.003969005461_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 147 btcusdt_15m_rules_147: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(26, -0.001004677022_f64),
            Cond::Eq(42, 21.0_f64),
        ],
    ),
    // 148 btcusdt_15m_rules_148: RED
    (
        false,
        &[
            Cond::Ge(70, 89.541313522713_f64),
            Cond::Ge(0, 30.625040783455_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 149 btcusdt_15m_rules_149: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Between(6, 0.002236733024_f64, 0.003968588912_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 150 btcusdt_15m_rules_150: GREEN
    (
        true,
        &[
            Cond::Le(49, 8.24623315708_f64),
            Cond::Le(80, -0.023171858787_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 151 btcusdt_15m_rules_151: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.016780876652_f64),
            Cond::Le(45, -0.004351170326_f64),
        ],
    ),
    // 152 btcusdt_15m_rules_152: GREEN
    (
        true,
        &[
            Cond::Le(49, 0.0_f64),
            Cond::Le(27, 0.001007431938_f64),
            Cond::Eq(42, 15.0_f64),
        ],
    ),
    // 153 btcusdt_15m_rules_153: RED
    (
        false,
        &[
            Cond::Ge(49, 90.21492658429_f64),
            Cond::Le(11, -0.479441712554_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 154 btcusdt_15m_rules_154: GREEN
    (
        true,
        &[
            Cond::Le(61, 29.1932855_f64),
            Cond::Le(15, 0.1724014402_f64),
            Cond::Ge(41, 0.3840913291_f64),
            Cond::Eq(42, 7.0_f64),
        ],
    ),
    // 155 btcusdt_15m_rules_155: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.335678247138_f64),
            Cond::Le(52, -1.536658910705_f64),
            Cond::Eq(42, 7.0_f64),
        ],
    ),
    // 156 btcusdt_15m_rules_156: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.503757061587_f64),
            Cond::Le(1, -0.75450257826_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 157 btcusdt_15m_rules_157: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.005558113318_f64),
            Cond::Le(45, -0.003023911405_f64),
            Cond::Ge(18, -2.500795018_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 158 btcusdt_15m_rules_158: RED
    (
        false,
        &[
            Cond::Ge(49, 92.869319882182_f64),
            Cond::Le(20, 0.004404610817_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 159 btcusdt_15m_rules_159: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.005204708323_f64),
            Cond::Ge(49, 88.487367755168_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 160 btcusdt_15m_rules_160: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.027370425915_f64),
            Cond::Ge(50, 1320.0_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 161 btcusdt_15m_rules_161: GREEN
    (
        true,
        &[
            Cond::Le(71, 9.711755951452_f64),
            Cond::Ge(35, 6.0_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 162 btcusdt_15m_rules_162: RED
    (
        false,
        &[
            Cond::Ge(41, 0.735382052348_f64),
            Cond::Ge(7, 0.784092186466_f64),
            Cond::Eq(42, 10.0_f64),
        ],
    ),
    // 163 btcusdt_15m_rules_163: GREEN
    (
        true,
        &[
            Cond::Le(49, 0.0_f64),
            Cond::Le(27, 0.001007431938_f64),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 164 btcusdt_15m_rules_164: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(47, 45.114842308581_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 165 btcusdt_15m_rules_165: GREEN
    (
        true,
        &[
            Cond::Le(48, 20.65922058_f64),
            Cond::Eq(81, 2.0_f64),
            Cond::Le(62, 33.60114385_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 166 btcusdt_15m_rules_166: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Ge(13, -43.945068697837_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 167 btcusdt_15m_rules_167: RED
    (
        false,
        &[
            Cond::Ge(62, 85.681824259181_f64),
            Cond::Ge(74, 0.0067308277_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 168 btcusdt_15m_rules_168: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000409020996_f64),
            Cond::Le(40, 0.159137292685_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 169 btcusdt_15m_rules_169: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.405699915549_f64),
            Cond::Ge(39, 0.008514598019_f64),
        ],
    ),
    // 170 btcusdt_15m_rules_170: RED
    (
        false,
        &[
            Cond::Ge(71, 97.684471745776_f64),
            Cond::Le(15, 0.848302105456_f64),
            Cond::Eq(42, 6.0_f64),
        ],
    ),
    // 171 btcusdt_15m_rules_171: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.26951785_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(12, -191.4788279_f64),
            Cond::Eq(42, 11.0_f64),
        ],
    ),
    // 172 btcusdt_15m_rules_172: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000317708634_f64),
            Cond::Le(15, 0.124796612801_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 173 btcusdt_15m_rules_173: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001549259691_f64),
            Cond::Ge(79, 0.003599409445_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 174 btcusdt_15m_rules_174: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000191900674_f64),
            Cond::In(42, &[12.0_f64]),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 175 btcusdt_15m_rules_175: GREEN
    (
        true,
        &[
            Cond::Le(37, 0.0_f64),
            Cond::In(81, &[5.0_f64]),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 176 btcusdt_15m_rules_176: RED
    (
        false,
        &[
            Cond::Ge(17, 1.806249404164_f64),
            Cond::Le(80, -0.014146164354_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 177 btcusdt_15m_rules_177: RED
    (
        false,
        &[
            Cond::Ge(71, 99.030619805153_f64),
            Cond::In(42, &[20.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 178 btcusdt_15m_rules_178: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.129202691063_f64),
            Cond::Le(2, 0.000647938293_f64),
        ],
    ),
    // 179 btcusdt_15m_rules_179: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(43, 0.00002645309807_f64),
            Cond::Ge(44, 0.00005001062838_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 180 btcusdt_15m_rules_180: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000024352929_f64),
            Cond::Ge(70, 0.306654763017_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 181 btcusdt_15m_rules_181: RED
    (
        false,
        &[
            Cond::Ge(4, 1.075503944993_f64),
            Cond::Le(52, -0.822178298076_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 182 btcusdt_15m_rules_182: GREEN
    (
        true,
        &[
            Cond::Le(12, -219.821108418032_f64),
            Cond::Between(0, -0.127754869275_f64, -0.00002245468_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 183 btcusdt_15m_rules_183: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Le(51, 1.586225659_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 184 btcusdt_15m_rules_184: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Le(6, 0.002236733024_f64),
            Cond::Eq(42, 0.0_f64),
        ],
    ),
    // 185 btcusdt_15m_rules_185: RED
    (
        false,
        &[
            Cond::Ge(38, 6.0_f64),
            Cond::Le(19, 0.590766072643_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 186 btcusdt_15m_rules_186: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(44, 0.01349188119_f64),
            Cond::Ge(63, 29.60116771_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 187 btcusdt_15m_rules_187: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Between(52, -0.614786474045_f64, 0.207539773281_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 188 btcusdt_15m_rules_188: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(62, 25.0_f64),
            Cond::Ge(51, 1.2_f64),
            Cond::Ge(7, 0.45_f64),
            Cond::Eq(81, 6.0_f64),
            Cond::Eq(42, 0.0_f64),
        ],
    ),
    // 189 btcusdt_15m_rules_189: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Le(15, 0.002359360549_f64),
            Cond::Ge(62, 28.1055143_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 190 btcusdt_15m_rules_190: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000518047549_f64),
            Cond::Le(45, -0.003453836366_f64),
        ],
    ),
    // 191 btcusdt_15m_rules_191: RED
    (
        false,
        &[
            Cond::Ge(47, 89.80044543946_f64),
            Cond::In(81, &[3.0_f64]),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 192 btcusdt_15m_rules_192: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Between(6, 0.002236733024_f64, 0.003968588912_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 193 btcusdt_15m_rules_193: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.160811012751_f64),
            Cond::Ge(56, -0.020308471435_f64),
            Cond::Eq(42, 17.0_f64),
        ],
    ),
    // 194 btcusdt_15m_rules_194: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Ge(17, -1.412432258616_f64),
            Cond::Eq(42, 18.0_f64),
        ],
    ),
    // 195 btcusdt_15m_rules_195: GREEN
    (
        true,
        &[
            Cond::Le(49, 5.127057896226_f64),
            Cond::Le(64, 0.5_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 196 btcusdt_15m_rules_196: RED
    (
        false,
        &[
            Cond::Ge(41, 0.772240951118_f64),
            Cond::Le(11, -1.02601582424_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 197 btcusdt_15m_rules_197: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000547061338_f64),
            Cond::Le(47, 16.087074533329_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 198 btcusdt_15m_rules_198: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.077124389957_f64),
            Cond::Ge(45, 0.000140769183_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 199 btcusdt_15m_rules_199: RED
    (
        false,
        &[
            Cond::Ge(70, 98.621001727441_f64),
            Cond::Le(15, 0.926451224707_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 200 btcusdt_15m_rules_200: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.581548268_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(28, 0.006105932389_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 201 btcusdt_15m_rules_201: GREEN
    (
        true,
        &[
            Cond::Le(63, 35.443039978605_f64),
            Cond::Ge(17, -0.372375456891_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 202 btcusdt_15m_rules_202: GREEN
    (
        true,
        &[
            Cond::Le(71, 7.980198437_f64),
            Cond::Le(49, 18.38030246_f64),
            Cond::Le(63, 12.50217521_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 203 btcusdt_15m_rules_203: RED
    (
        false,
        &[
            Cond::Ge(4, 0.944943849905_f64),
            Cond::Ge(2, 0.014808079873_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 204 btcusdt_15m_rules_204: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.246542685688_f64),
            Cond::Ge(25, -0.006079005042_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 205 btcusdt_15m_rules_205: RED
    (
        false,
        &[
            Cond::Ge(17, 2.287561321199_f64),
            Cond::Le(46, -0.000898726231_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 206 btcusdt_15m_rules_206: RED
    (
        false,
        &[
            Cond::Ge(71, 92.847408686743_f64),
            Cond::Le(40, 0.017282150796_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 207 btcusdt_15m_rules_207: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.006258679323_f64),
            Cond::Le(19, 0.499150169133_f64),
        ],
    ),
    // 208 btcusdt_15m_rules_208: RED
    (
        false,
        &[
            Cond::Ge(70, 97.683323069458_f64),
            Cond::Le(47, 31.262587312934_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 209 btcusdt_15m_rules_209: GREEN
    (
        true,
        &[
            Cond::Ge(54, 2.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(51, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(42, 6.0_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 210 btcusdt_15m_rules_210: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.335678247138_f64),
            Cond::Ge(77, 4.071608244105_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 211 btcusdt_15m_rules_211: GREEN
    (
        true,
        &[
            Cond::Ge(76, 4.86378068509_f64),
            Cond::Between(16, -1.130209885596_f64, 1.148339337785_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 212 btcusdt_15m_rules_212: RED
    (
        false,
        &[
            Cond::Ge(45, 0.001865948161_f64),
            Cond::Ge(70, 95.36043284_f64),
            Cond::Le(71, 96.70846245_f64),
            Cond::Eq(42, 2.0_f64),
        ],
    ),
    // 213 btcusdt_15m_rules_213: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(30, 0.03468189691_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(42, 8.0_f64),
        ],
    ),
    // 214 btcusdt_15m_rules_214: RED
    (
        false,
        &[
            Cond::Ge(38, 4.0_f64),
            Cond::Ge(73, 50.208999999976_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 215 btcusdt_15m_rules_215: RED
    (
        false,
        &[
            Cond::Ge(63, 68.704859813649_f64),
            Cond::Le(77, -1.459564445306_f64),
        ],
    ),
    // 216 btcusdt_15m_rules_216: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000546936819_f64),
            Cond::Le(7, 0.109269006706_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 217 btcusdt_15m_rules_217: GREEN
    (
        true,
        &[
            Cond::Le(70, 12.958361968728_f64),
            Cond::Le(77, -1.385238691491_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 218 btcusdt_15m_rules_218: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.26951785_f64),
            Cond::Le(3, 0.002457878466_f64),
            Cond::Le(70, 12.12790869_f64),
            Cond::Eq(42, 21.0_f64),
        ],
    ),
    // 219 btcusdt_15m_rules_219: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001086987122_f64),
            Cond::Ge(37, 5.0_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 220 btcusdt_15m_rules_220: RED
    (
        false,
        &[
            Cond::Ge(70, 98.621001727441_f64),
            Cond::In(42, &[12.0_f64]),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 221 btcusdt_15m_rules_221: RED
    (
        false,
        &[
            Cond::Ge(71, 98.632249177409_f64),
            Cond::Ge(0, 0.941851345832_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 222 btcusdt_15m_rules_222: RED
    (
        false,
        &[
            Cond::Ge(45, 0.001865948161_f64),
            Cond::Ge(70, 95.36043284_f64),
            Cond::Le(71, 96.70846245_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 223 btcusdt_15m_rules_223: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.124295441041_f64),
            Cond::Between(52, -0.464230165361_f64, -0.047403807875_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 224 btcusdt_15m_rules_224: GREEN
    (
        true,
        &[
            Cond::Le(62, 14.028360392873_f64),
            Cond::Ge(21, -0.010226964393_f64),
            Cond::Eq(42, 7.0_f64),
        ],
    ),
    // 225 btcusdt_15m_rules_225: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 226 btcusdt_15m_rules_226: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.007057262978_f64),
            Cond::Ge(13, 151.60195878472_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 227 btcusdt_15m_rules_227: GREEN
    (
        true,
        &[
            Cond::Le(70, 0.5066458518_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Ge(43, 9.29093662300000e-8_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 228 btcusdt_15m_rules_228: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Ge(36, 2.0_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 229 btcusdt_15m_rules_229: RED
    (
        false,
        &[
            Cond::Ge(41, 0.644678507722_f64),
            Cond::Ge(6, 0.010655328013_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 230 btcusdt_15m_rules_230: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(17, 2.080158043835_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 231 btcusdt_15m_rules_231: GREEN
    (
        true,
        &[
            Cond::Le(63, 12.722439334531_f64),
            Cond::Le(2, 0.003127045792_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 232 btcusdt_15m_rules_232: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Between(7, 0.348371370028_f64, 0.50546791832_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 233 btcusdt_15m_rules_233: RED
    (
        false,
        &[
            Cond::Ge(4, 1.242274122_f64),
            Cond::Eq(81, 3.0_f64),
            Cond::Le(78, 2.098463339_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 234 btcusdt_15m_rules_234: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.005204708323_f64),
            Cond::Ge(49, 88.487367755168_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 235 btcusdt_15m_rules_235: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.120614853726_f64),
            Cond::Ge(69, 1.850542798185_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 236 btcusdt_15m_rules_236: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.242308839024_f64),
            Cond::Ge(59, 0.00097606558_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 237 btcusdt_15m_rules_237: GREEN
    (
        true,
        &[
            Cond::Le(60, 27.902869321396_f64),
            Cond::Ge(79, -0.001373496039_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 238 btcusdt_15m_rules_238: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202478662_f64),
            Cond::Le(76, 1.758262671_f64),
            Cond::Le(78, 0.7249823529_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 239 btcusdt_15m_rules_239: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Between(7, 0.264860593835_f64, 0.58517531194_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 240 btcusdt_15m_rules_240: GREEN
    (
        true,
        &[
            Cond::Le(70, 6.452676568325_f64),
            Cond::Le(0, -1.915883231776_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 241 btcusdt_15m_rules_241: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000685074848_f64),
            Cond::Le(80, -0.012633865095_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 242 btcusdt_15m_rules_242: GREEN
    (
        true,
        &[
            Cond::Le(62, 20.625140058973_f64),
            Cond::Ge(80, 0.009956278533_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 243 btcusdt_15m_rules_243: RED
    (
        false,
        &[
            Cond::Ge(16, 2.254214593777_f64),
            Cond::Le(76, 0.555582600345_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 244 btcusdt_15m_rules_244: RED
    (
        false,
        &[
            Cond::Ge(16, 1.714252214364_f64),
            Cond::Le(52, -1.129440954302_f64),
            Cond::Eq(42, 6.0_f64),
        ],
    ),
    // 245 btcusdt_15m_rules_245: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.014325177909_f64),
            Cond::Between(59, -0.002369659403_f64, 0.0026622929_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 246 btcusdt_15m_rules_246: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.016780876652_f64),
            Cond::Le(11, -0.672155355901_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 247 btcusdt_15m_rules_247: RED
    (
        false,
        &[
            Cond::Ge(16, 1.908788195242_f64),
            Cond::Le(51, 0.47715417535_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 248 btcusdt_15m_rules_248: GREEN
    (
        true,
        &[
            Cond::Le(63, 37.670808563448_f64),
            Cond::Ge(17, -0.382943541103_f64),
            Cond::Eq(42, 21.0_f64),
        ],
    ),
    // 249 btcusdt_15m_rules_249: RED
    (
        false,
        &[
            Cond::Ge(47, 87.326571431094_f64),
            Cond::Between(23, -0.003483247059_f64, 0.005497807331_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 250 btcusdt_15m_rules_250: RED
    (
        false,
        &[
            Cond::Ge(38, 6.0_f64),
            Cond::Ge(2, 0.007385036543_f64),
            Cond::Eq(42, 21.0_f64),
        ],
    ),
    // 251 btcusdt_15m_rules_251: RED
    (
        false,
        &[
            Cond::Ge(45, 0.001865948161_f64),
            Cond::Ge(70, 95.36043284_f64),
            Cond::Ge(48, 73.95404425_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 252 btcusdt_15m_rules_252: RED
    (
        false,
        &[
            Cond::Ge(71, 97.799245309998_f64),
            Cond::Le(52, -0.979734943474_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 253 btcusdt_15m_rules_253: RED
    (
        false,
        &[
            Cond::Ge(62, 63.311963948745_f64),
            Cond::Le(56, -0.004977456214_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 254 btcusdt_15m_rules_254: RED
    (
        false,
        &[
            Cond::Ge(71, 96.0335550175_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 255 btcusdt_15m_rules_255: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(76, 0.216880790466_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 256 btcusdt_15m_rules_256: RED
    (
        false,
        &[
            Cond::Ge(49, 88.952653717272_f64),
            Cond::Le(51, 0.298268023925_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 257 btcusdt_15m_rules_257: RED
    (
        false,
        &[
            Cond::Ge(49, 90.971006267369_f64),
            Cond::Le(77, -1.22650223573_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 258 btcusdt_15m_rules_258: GREEN
    (
        true,
        &[
            Cond::Le(71, 9.711755951452_f64),
            Cond::Ge(9, 0.001375151652_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 259 btcusdt_15m_rules_259: RED
    (
        false,
        &[
            Cond::Ge(41, 0.735382052348_f64),
            Cond::Ge(7, 0.784092186466_f64),
            Cond::Eq(42, 17.0_f64),
        ],
    ),
    // 260 btcusdt_15m_rules_260: RED
    (
        false,
        &[
            Cond::Ge(70, 88.470757284927_f64),
            Cond::Le(39, -0.000358476716_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 261 btcusdt_15m_rules_261: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.00194583421_f64),
            Cond::Ge(80, 0.025184469595_f64),
        ],
    ),
    // 262 btcusdt_15m_rules_262: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000394386125_f64),
            Cond::Le(51, 0.663177938908_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 263 btcusdt_15m_rules_263: RED
    (
        false,
        &[
            Cond::Ge(49, 92.869319882182_f64),
            Cond::Le(72, 0.000272339529_f64),
            Cond::Eq(42, 10.0_f64),
        ],
    ),
    // 264 btcusdt_15m_rules_264: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::Eq(42, 17.0_f64),
        ],
    ),
    // 265 btcusdt_15m_rules_265: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.160650711145_f64),
            Cond::Le(6, 0.000036818679_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 266 btcusdt_15m_rules_266: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.013135821846_f64),
            Cond::Ge(12, 114.801444660689_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 267 btcusdt_15m_rules_267: RED
    (
        false,
        &[
            Cond::Ge(49, 92.538838851052_f64),
            Cond::Ge(44, 16.649945784544_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 268 btcusdt_15m_rules_268: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.251600440171_f64),
            Cond::Ge(1, 6.855156552576_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 269 btcusdt_15m_rules_269: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(77, -1.341363982825_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 270 btcusdt_15m_rules_270: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::In(42, &[11.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 271 btcusdt_15m_rules_271: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(24, -0.007401969759_f64),
            Cond::Eq(42, 16.0_f64),
        ],
    ),
    // 272 btcusdt_15m_rules_272: RED
    (
        false,
        &[
            Cond::Ge(17, 2.562798212558_f64),
            Cond::Le(75, 0.000742956712_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 273 btcusdt_15m_rules_273: RED
    (
        false,
        &[
            Cond::Ge(24, -0.001005628718_f64),
            Cond::Le(45, -0.00032682283_f64),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 274 btcusdt_15m_rules_274: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Le(19, 0.693883935668_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 275 btcusdt_15m_rules_275: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(42, 14.0_f64),
        ],
    ),
    // 276 btcusdt_15m_rules_276: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000120857876_f64),
            Cond::Le(7, 0.302247845896_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 277 btcusdt_15m_rules_277: RED
    (
        false,
        &[
            Cond::Ge(63, 88.325740640992_f64),
            Cond::Le(45, 0.001174029944_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 278 btcusdt_15m_rules_278: RED
    (
        false,
        &[
            Cond::Ge(10, 0.008390907843_f64),
            Cond::Ge(24, -0.0003773777571_f64),
            Cond::Le(43, 0.0_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 279 btcusdt_15m_rules_279: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00041072089_f64),
            Cond::Ge(73, 3.979483531844_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 280 btcusdt_15m_rules_280: GREEN
    (
        true,
        &[
            Cond::Le(71, 8.683865767064_f64),
            Cond::Ge(16, -0.753237336396_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 281 btcusdt_15m_rules_281: RED
    (
        false,
        &[
            Cond::Ge(62, 72.019291926381_f64),
            Cond::Le(13, 24.33220298786_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 282 btcusdt_15m_rules_282: RED
    (
        false,
        &[
            Cond::Ge(70, 99.995600480172_f64),
            Cond::Le(24, -2.58479000000000e-7_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 283 btcusdt_15m_rules_283: RED
    (
        false,
        &[
            Cond::Ge(63, 82.454573967079_f64),
            Cond::In(42, &[12.0_f64]),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 284 btcusdt_15m_rules_284: RED
    (
        false,
        &[
            Cond::Ge(70, 95.36043284_f64),
            Cond::Ge(8, 0.01911639022_f64),
            Cond::Ge(14, 301.1917591_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 285 btcusdt_15m_rules_285: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.004905605597_f64),
            Cond::Le(52, -1.563227737634_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 286 btcusdt_15m_rules_286: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(42, 9.0_f64),
            Cond::Le(18, 3.429563387_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 287 btcusdt_15m_rules_287: RED
    (
        false,
        &[
            Cond::Ge(71, 92.847408686743_f64),
            Cond::Le(79, -0.004046947059_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 288 btcusdt_15m_rules_288: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.353463333651_f64),
            Cond::In(42, &[5.0_f64]),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 289 btcusdt_15m_rules_289: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.371343923784_f64),
            Cond::Ge(58, 0.00710213073_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 290 btcusdt_15m_rules_290: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 291 btcusdt_15m_rules_291: RED
    (
        false,
        &[
            Cond::Ge(71, 92.398919564528_f64),
            Cond::Le(60, 53.275547235097_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 292 btcusdt_15m_rules_292: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000239825686_f64),
            Cond::Ge(56, 0.01939351557_f64),
            Cond::Le(43, 0.001638794532_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 293 btcusdt_15m_rules_293: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.218185817798_f64),
            Cond::Le(52, -0.979734943474_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 294 btcusdt_15m_rules_294: GREEN
    (
        true,
        &[
            Cond::Le(61, 29.1932855_f64),
            Cond::Le(15, 0.1724014402_f64),
            Cond::Ge(41, 0.3840913291_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 295 btcusdt_15m_rules_295: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.002650174002_f64),
            Cond::Ge(51, 3.036678151198_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 296 btcusdt_15m_rules_296: RED
    (
        false,
        &[
            Cond::Ge(63, 79.78754453_f64),
            Cond::Eq(42, 21.0_f64),
            Cond::Ge(53, 2.0_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 297 btcusdt_15m_rules_297: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::In(42, &[7.0_f64]),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 298 btcusdt_15m_rules_298: GREEN
    (
        true,
        &[
            Cond::Le(37, 1.0_f64),
            Cond::Ge(34, 9.0_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 299 btcusdt_15m_rules_299: RED
    (
        false,
        &[
            Cond::Ge(63, 88.325740640992_f64),
            Cond::Le(13, 127.802445543786_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 300 btcusdt_15m_rules_300: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Ge(5, -0.006258679323_f64),
            Cond::Eq(42, 10.0_f64),
        ],
    ),
    // 301 btcusdt_15m_rules_301: RED
    (
        false,
        &[
            Cond::Ge(16, 1.714252214364_f64),
            Cond::Le(52, -1.129440954302_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 302 btcusdt_15m_rules_302: RED
    (
        false,
        &[
            Cond::Ge(38, 4.0_f64),
            Cond::Le(77, -1.385238691491_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 303 btcusdt_15m_rules_303: RED
    (
        false,
        &[
            Cond::Ge(41, 0.735382052348_f64),
            Cond::Le(47, 24.709740053435_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 304 btcusdt_15m_rules_304: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(77, -1.341363982825_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 305 btcusdt_15m_rules_305: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Le(76, 0.604709102916_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 306 btcusdt_15m_rules_306: GREEN
    (
        true,
        &[
            Cond::Le(47, 15.30340668771_f64),
            Cond::Le(40, 0.063903626236_f64),
            Cond::Eq(42, 16.0_f64),
        ],
    ),
    // 307 btcusdt_15m_rules_307: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.110521327309_f64),
            Cond::Ge(32, 1.0_f64),
            Cond::Eq(42, 18.0_f64),
        ],
    ),
    // 308 btcusdt_15m_rules_308: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.003883345889_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 309 btcusdt_15m_rules_309: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.002456946598_f64),
            Cond::Ge(19, 1.804032915518_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 310 btcusdt_15m_rules_310: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.005558113318_f64),
            Cond::Le(45, -0.002259446914_f64),
            Cond::Ge(4, 0.04033662233_f64),
            Cond::Eq(42, 21.0_f64),
        ],
    ),
    // 311 btcusdt_15m_rules_311: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.00254012653_f64),
            Cond::Ge(43, 0.002459555014_f64),
            Cond::Eq(42, 15.0_f64),
        ],
    ),
    // 312 btcusdt_15m_rules_312: GREEN
    (
        true,
        &[
            Cond::Le(12, -227.797658840935_f64),
            Cond::In(42, &[0.0_f64]),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 313 btcusdt_15m_rules_313: GREEN
    (
        true,
        &[
            Cond::Le(71, 5.77000366238_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 314 btcusdt_15m_rules_314: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Ge(58, -0.004437668774_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 315 btcusdt_15m_rules_315: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.170746597814_f64),
            Cond::Ge(73, 3.979483531844_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 316 btcusdt_15m_rules_316: GREEN
    (
        true,
        &[
            Cond::Le(12, -188.547426560093_f64),
            Cond::Le(52, -0.822178298076_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 317 btcusdt_15m_rules_317: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.002900258506_f64),
            Cond::Ge(80, 0.015962036922_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 318 btcusdt_15m_rules_318: RED
    (
        false,
        &[
            Cond::Ge(62, 67.899626426361_f64),
            Cond::Le(47, 33.487434256627_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 319 btcusdt_15m_rules_319: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000120857876_f64),
            Cond::Le(0, -0.127754869275_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 320 btcusdt_15m_rules_320: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.724176425078_f64),
            Cond::In(42, &[11.0_f64]),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 321 btcusdt_15m_rules_321: GREEN
    (
        true,
        &[
            Cond::Le(49, 0.0_f64),
            Cond::Le(27, 0.001007431938_f64),
            Cond::Eq(42, 17.0_f64),
        ],
    ),
    // 322 btcusdt_15m_rules_322: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.273882464262_f64),
            Cond::Ge(25, -0.001334494357_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 323 btcusdt_15m_rules_323: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(42, 9.0_f64),
            Cond::Le(18, 3.429563387_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 324 btcusdt_15m_rules_324: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.705007578106_f64),
            Cond::Ge(69, 3.241967620561_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 325 btcusdt_15m_rules_325: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000053521742_f64),
            Cond::Ge(50, 1395.0_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 326 btcusdt_15m_rules_326: RED
    (
        false,
        &[
            Cond::Ge(70, 87.582148468611_f64),
            Cond::Le(45, -0.000710524492_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 327 btcusdt_15m_rules_327: RED
    (
        false,
        &[
            Cond::Ge(71, 89.373499384241_f64),
            Cond::Le(60, 53.275547235097_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 328 btcusdt_15m_rules_328: RED
    (
        false,
        &[
            Cond::Ge(63, 70.589612029705_f64),
            Cond::Le(47, 40.166548899026_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 329 btcusdt_15m_rules_329: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.65908861946_f64),
            Cond::In(42, &[22.0_f64]),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 330 btcusdt_15m_rules_330: GREEN
    (
        true,
        &[
            Cond::Le(62, 19.291134799525_f64),
            Cond::Le(11, -0.313680587227_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 331 btcusdt_15m_rules_331: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000905802239_f64),
            Cond::Ge(79, 0.005828474127_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 332 btcusdt_15m_rules_332: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.0107807827_f64),
            Cond::Ge(8, 0.06947460341_f64),
        ],
    ),
    // 333 btcusdt_15m_rules_333: RED
    (
        false,
        &[
            Cond::Ge(63, 85.600813068941_f64),
            Cond::Between(79, -0.003184483329_f64, 0.003367010176_f64),
            Cond::Eq(42, 11.0_f64),
        ],
    ),
    // 334 btcusdt_15m_rules_334: GREEN
    (
        true,
        &[
            Cond::Le(37, 0.0_f64),
            Cond::In(81, &[5.0_f64]),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 335 btcusdt_15m_rules_335: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.004596587699_f64),
            Cond::Ge(60, 72.313585043492_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 336 btcusdt_15m_rules_336: RED
    (
        false,
        &[
            Cond::Ge(17, 2.562798212558_f64),
            Cond::Le(21, 0.002256340125_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 337 btcusdt_15m_rules_337: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.592157413_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::Eq(42, 6.0_f64),
        ],
    ),
    // 338 btcusdt_15m_rules_338: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(76, 0.216880790466_f64),
            Cond::Eq(42, 21.0_f64),
        ],
    ),
    // 339 btcusdt_15m_rules_339: RED
    (
        false,
        &[
            Cond::Ge(16, 1.595452540363_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 340 btcusdt_15m_rules_340: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.004499095898_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 341 btcusdt_15m_rules_341: RED
    (
        false,
        &[
            Cond::Ge(41, 0.660005226124_f64),
            Cond::Ge(2, 0.014808079873_f64),
            Cond::Eq(42, 15.0_f64),
        ],
    ),
    // 342 btcusdt_15m_rules_342: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000905802239_f64),
            Cond::Ge(79, 0.005828474127_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 343 btcusdt_15m_rules_343: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.220922553819_f64),
            Cond::Ge(45, 0.000404314765_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 344 btcusdt_15m_rules_344: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000186502931_f64),
            Cond::Le(77, -1.385238691491_f64),
            Cond::Eq(42, 20.0_f64),
        ],
    ),
    // 345 btcusdt_15m_rules_345: GREEN
    (
        true,
        &[
            Cond::Le(12, -169.388968825084_f64),
            Cond::Le(74, 0.000010977951_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 346 btcusdt_15m_rules_346: RED
    (
        false,
        &[
            Cond::Ge(47, 89.80044543946_f64),
            Cond::Between(27, 0.008375974151_f64, 0.013233652106_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 347 btcusdt_15m_rules_347: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.156166218685_f64),
            Cond::Le(1, -42.545668662262_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 348 btcusdt_15m_rules_348: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(46, 0.004216605253_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 349 btcusdt_15m_rules_349: GREEN
    (
        true,
        &[
            Cond::Le(12, -196.455697922622_f64),
            Cond::Ge(17, -0.804112577713_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 350 btcusdt_15m_rules_350: RED
    (
        false,
        &[
            Cond::Ge(16, 1.714252214364_f64),
            Cond::Le(52, -1.129440954302_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 351 btcusdt_15m_rules_351: RED
    (
        false,
        &[
            Cond::Ge(49, 90.540588550667_f64),
            Cond::Le(46, -0.002801477945_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 352 btcusdt_15m_rules_352: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Le(72, 8.42947706900000e-8_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 353 btcusdt_15m_rules_353: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.898892702_f64),
            Cond::Le(45, -0.002344176743_f64),
            Cond::Ge(51, 1.403339542_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 354 btcusdt_15m_rules_354: GREEN
    (
        true,
        &[
            Cond::Le(60, 25.790818624552_f64),
            Cond::Ge(24, -0.007460989272_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 355 btcusdt_15m_rules_355: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(77, -0.917339193708_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 356 btcusdt_15m_rules_356: RED
    (
        false,
        &[
            Cond::Ge(70, 98.026144238666_f64),
            Cond::Le(76, 0.485298641061_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 357 btcusdt_15m_rules_357: RED
    (
        false,
        &[
            Cond::Ge(63, 88.325740640992_f64),
            Cond::Le(45, 0.001174029944_f64),
            Cond::Eq(42, 10.0_f64),
        ],
    ),
    // 358 btcusdt_15m_rules_358: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.55850139458_f64),
            Cond::In(42, &[7.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 359 btcusdt_15m_rules_359: GREEN
    (
        true,
        &[
            Cond::Le(71, 8.683865767064_f64),
            Cond::Ge(60, 47.660407199097_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 360 btcusdt_15m_rules_360: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::In(42, &[17.0_f64]),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 361 btcusdt_15m_rules_361: RED
    (
        false,
        &[
            Cond::Ge(16, 1.962352079221_f64),
            Cond::Le(49, 37.017750359136_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 362 btcusdt_15m_rules_362: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(19, 1.995743208991_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 363 btcusdt_15m_rules_363: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 364 btcusdt_15m_rules_364: RED
    (
        false,
        &[
            Cond::Ge(70, 98.026144238666_f64),
            Cond::Le(76, 0.485298641061_f64),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 365 btcusdt_15m_rules_365: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.002650174002_f64),
            Cond::Ge(51, 3.036678151198_f64),
            Cond::Eq(42, 16.0_f64),
        ],
    ),
    // 366 btcusdt_15m_rules_366: GREEN
    (
        true,
        &[
            Cond::Le(60, 24.975510474295_f64),
            Cond::Le(43, 0.000193642335_f64),
            Cond::Eq(42, 15.0_f64),
        ],
    ),
    // 367 btcusdt_15m_rules_367: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.005348262374_f64),
            Cond::Ge(60, 69.060618393279_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 368 btcusdt_15m_rules_368: GREEN
    (
        true,
        &[
            Cond::Le(62, 18.00845307_f64),
            Cond::Ge(24, -0.01631766978_f64),
            Cond::Le(30, 0.0015569182_f64),
            Cond::Eq(42, 5.0_f64),
        ],
    ),
    // 369 btcusdt_15m_rules_369: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.469218562694_f64),
            Cond::Ge(45, 0.000140769183_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 370 btcusdt_15m_rules_370: RED
    (
        false,
        &[
            Cond::Ge(70, 99.185382275768_f64),
            Cond::Le(80, -0.009431026424_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 371 btcusdt_15m_rules_371: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000091348551_f64),
            Cond::Ge(75, 0.007973138013_f64),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 372 btcusdt_15m_rules_372: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.55850139458_f64),
            Cond::In(42, &[20.0_f64]),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 373 btcusdt_15m_rules_373: GREEN
    (
        true,
        &[
            Cond::Le(70, 5.418823304347_f64),
            Cond::Le(19, 0.517302629848_f64),
            Cond::Eq(42, 0.0_f64),
        ],
    ),
    // 374 btcusdt_15m_rules_374: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202478662_f64),
            Cond::Le(76, 1.758262671_f64),
            Cond::Le(78, 0.7249823529_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 375 btcusdt_15m_rules_375: RED
    (
        false,
        &[
            Cond::Ge(49, 92.869319882182_f64),
            Cond::Le(72, 0.000272339529_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 376 btcusdt_15m_rules_376: RED
    (
        false,
        &[
            Cond::Ge(41, 0.644678507722_f64),
            Cond::Ge(6, 0.010655328013_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 377 btcusdt_15m_rules_377: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.1302923821_f64),
            Cond::Le(2, 0.002139583407_f64),
            Cond::Eq(81, 6.0_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 378 btcusdt_15m_rules_378: RED
    (
        false,
        &[Cond::Ge(38, 6.0_f64), Cond::Le(49, 36.682496680971_f64)],
    ),
    // 379 btcusdt_15m_rules_379: GREEN
    (
        true,
        &[
            Cond::Le(63, 12.722439334531_f64),
            Cond::Ge(79, -0.004763212286_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 380 btcusdt_15m_rules_380: GREEN
    (
        true,
        &[
            Cond::Le(52, -1.536658910705_f64),
            Cond::Le(71, 24.168754528768_f64),
            Cond::Eq(42, 19.0_f64),
        ],
    ),
    // 381 btcusdt_15m_rules_381: RED
    (
        false,
        &[
            Cond::Ge(38, 6.0_f64),
            Cond::Le(2, 0.003315567219_f64),
            Cond::Eq(42, 16.0_f64),
        ],
    ),
    // 382 btcusdt_15m_rules_382: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.273882464262_f64),
            Cond::Ge(29, 0.030155709967_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 383 btcusdt_15m_rules_383: GREEN
    (
        true,
        &[
            Cond::Le(70, 2.062588143616_f64),
            Cond::In(81, &[6.0_f64]),
            Cond::Eq(42, 14.0_f64),
        ],
    ),
    // 384 btcusdt_15m_rules_384: GREEN
    (
        true,
        &[
            Cond::Le(12, -188.547426560093_f64),
            Cond::Le(52, -0.638402427824_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 385 btcusdt_15m_rules_385: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000886273409_f64),
            Cond::Le(45, -0.001638248858_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 386 btcusdt_15m_rules_386: GREEN
    (
        true,
        &[
            Cond::Le(70, 5.418823304347_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(42, 6.0_f64),
        ],
    ),
    // 387 btcusdt_15m_rules_387: GREEN
    (
        true,
        &[
            Cond::Ge(76, 4.86378068509_f64),
            Cond::In(81, &[0.0_f64]),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 388 btcusdt_15m_rules_388: RED
    (
        false,
        &[
            Cond::Ge(41, 0.80253046606_f64),
            Cond::Ge(73, 5.937114574536_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 389 btcusdt_15m_rules_389: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.397952097938_f64),
            Cond::Ge(37, 5.0_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 390 btcusdt_15m_rules_390: GREEN
    (
        true,
        &[
            Cond::Le(12, -239.1833565_f64),
            Cond::Ge(17, -2.058232069_f64),
            Cond::Le(43, 0.001092316795_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 391 btcusdt_15m_rules_391: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000201199042_f64),
            Cond::Le(52, -1.563227737634_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 392 btcusdt_15m_rules_392: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::Le(11, -0.436252860345_f64),
            Cond::Eq(42, 10.0_f64),
        ],
    ),
    // 393 btcusdt_15m_rules_393: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000239825686_f64),
            Cond::Ge(56, 0.01939351557_f64),
            Cond::Le(43, 0.001638794532_f64),
            Cond::Eq(42, 14.0_f64),
        ],
    ),
    // 394 btcusdt_15m_rules_394: GREEN
    (
        true,
        &[
            Cond::Le(49, 5.127057896226_f64),
            Cond::Le(64, 0.5_f64),
            Cond::Eq(42, 1.0_f64),
        ],
    ),
    // 395 btcusdt_15m_rules_395: GREEN
    (
        true,
        &[
            Cond::Ge(44, 99.803555555201_f64),
            Cond::Ge(15, 0.944724032971_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 396 btcusdt_15m_rules_396: GREEN
    (
        true,
        &[
            Cond::Le(9, -0.005375380189_f64),
            Cond::Ge(21, 0.029677055093_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 397 btcusdt_15m_rules_397: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(40, 0.255739517915_f64),
            Cond::Eq(42, 13.0_f64),
        ],
    ),
    // 398 btcusdt_15m_rules_398: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.359680944_f64),
            Cond::Le(2, 0.001795740443_f64),
            Cond::Le(4, -0.07487622772_f64),
            Cond::Eq(42, 6.0_f64),
        ],
    ),
    // 399 btcusdt_15m_rules_399: GREEN
    (
        true,
        &[
            Cond::Le(71, 0.937967556179_f64),
            Cond::Ge(9, -0.000631568573_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 400 btcusdt_15m_rules_400: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.880226526547_f64),
            Cond::Ge(56, -0.002710522059_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 401 btcusdt_15m_rules_401: GREEN
    (
        true,
        &[
            Cond::Le(12, -145.0194062_f64),
            Cond::Le(44, 0.01349188119_f64),
            Cond::Le(72, 0.00008848352749_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 402 btcusdt_15m_rules_402: GREEN
    (
        true,
        &[
            Cond::Le(62, 18.00845307_f64),
            Cond::Ge(24, -0.01631766978_f64),
            Cond::Le(30, 0.0015569182_f64),
            Cond::Eq(42, 11.0_f64),
        ],
    ),
    // 403 btcusdt_15m_rules_403: GREEN
    (
        true,
        &[
            Cond::Le(63, 31.93496681_f64),
            Cond::Ge(30, 0.03468189691_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(42, 20.0_f64),
        ],
    ),
    // 404 btcusdt_15m_rules_404: RED
    (
        false,
        &[
            Cond::Ge(17, 2.048626058358_f64),
            Cond::Le(80, -0.015077248431_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 405 btcusdt_15m_rules_405: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.170746597814_f64),
            Cond::Ge(73, 3.979483531844_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 406 btcusdt_15m_rules_406: RED
    (
        false,
        &[
            Cond::Ge(71, 97.799245309998_f64),
            Cond::Ge(23, 0.079980230055_f64),
        ],
    ),
    // 407 btcusdt_15m_rules_407: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Le(51, 0.952041479441_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 408 btcusdt_15m_rules_408: GREEN
    (
        true,
        &[
            Cond::Le(49, 0.0_f64),
            Cond::Ge(62, 32.766297807582_f64),
            Cond::Eq(42, 3.0_f64),
        ],
    ),
    // 409 btcusdt_15m_rules_409: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Ge(24, -0.002580027013_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 410 btcusdt_15m_rules_410: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Between(6, 0.002236733024_f64, 0.003968588912_f64),
            Cond::Eq(42, 0.0_f64),
        ],
    ),
    // 411 btcusdt_15m_rules_411: RED
    (
        false,
        &[
            Cond::Ge(12, 169.847668321743_f64),
            Cond::Le(77, -0.873606765831_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 412 btcusdt_15m_rules_412: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.273882464262_f64),
            Cond::Le(19, 0.368391267019_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 413 btcusdt_15m_rules_413: GREEN
    (
        true,
        &[
            Cond::Le(71, 8.683865767064_f64),
            Cond::Ge(21, -0.000859095584_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 414 btcusdt_15m_rules_414: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(69, 1.323958574628_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 415 btcusdt_15m_rules_415: RED
    (
        false,
        &[
            Cond::Ge(71, 83.706222662312_f64),
            Cond::Ge(0, 28.212125507464_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 416 btcusdt_15m_rules_416: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(25, -0.003833684053_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 417 btcusdt_15m_rules_417: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.970296088074_f64),
            Cond::Le(40, 0.118286862474_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 418 btcusdt_15m_rules_418: RED
    (
        false,
        &[
            Cond::Ge(17, 2.359739080046_f64),
            Cond::Le(8, 0.001347532606_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 419 btcusdt_15m_rules_419: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 420 btcusdt_15m_rules_420: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.592157413_f64),
            Cond::Eq(81, 5.0_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 421 btcusdt_15m_rules_421: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.293693136323_f64),
            Cond::Between(45, -0.00032682283_f64, 0.000294242144_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 422 btcusdt_15m_rules_422: RED
    (
        false,
        &[
            Cond::Ge(41, 0.660005226124_f64),
            Cond::Ge(2, 0.014808079873_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 423 btcusdt_15m_rules_423: GREEN
    (
        true,
        &[
            Cond::Le(12, -214.884127324173_f64),
            Cond::Le(11, -0.476299647738_f64),
        ],
    ),
    // 424 btcusdt_15m_rules_424: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.174372887633_f64),
            Cond::Between(51, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 425 btcusdt_15m_rules_425: RED
    (
        false,
        &[
            Cond::Ge(63, 80.15965415611_f64),
            Cond::Le(52, -0.979734943474_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 426 btcusdt_15m_rules_426: GREEN
    (
        true,
        &[
            Cond::Le(47, 12.370381786246_f64),
            Cond::Ge(45, -0.00093425444_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 427 btcusdt_15m_rules_427: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.561260986784_f64),
            Cond::Ge(73, 0.392199523066_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 428 btcusdt_15m_rules_428: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.581548268_f64),
            Cond::Le(2, 0.002521277008_f64),
            Cond::Le(28, 0.006105932389_f64),
            Cond::Eq(42, 4.0_f64),
        ],
    ),
    // 429 btcusdt_15m_rules_429: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000643444193_f64),
            Cond::Ge(0, 0.941851345832_f64),
            Cond::Eq(42, 22.0_f64),
        ],
    ),
    // 430 btcusdt_15m_rules_430: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.006258679323_f64),
            Cond::Ge(49, 88.271737232917_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 431 btcusdt_15m_rules_431: RED
    (
        false,
        &[
            Cond::Ge(71, 89.597440746859_f64),
            Cond::Ge(72, 0.005420124579_f64),
            Cond::Eq(42, 0.0_f64),
        ],
    ),
    // 432 btcusdt_15m_rules_432: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.218185817798_f64),
            Cond::Le(73, 0.035918324407_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 433 btcusdt_15m_rules_433: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.371097431342_f64),
            Cond::Le(47, 21.227386631273_f64),
            Cond::Eq(42, 15.0_f64),
        ],
    ),
    // 434 btcusdt_15m_rules_434: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(79, 0.014461038157_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 435 btcusdt_15m_rules_435: RED
    (
        false,
        &[
            Cond::Ge(71, 96.0335550175_f64),
            Cond::Le(1, -1.081483750837_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 436 btcusdt_15m_rules_436: GREEN
    (
        true,
        &[
            Cond::Le(71, 1.644587669_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(44, 0.01797752809_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 437 btcusdt_15m_rules_437: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000544949204_f64),
            Cond::Ge(23, 0.018572337802_f64),
            Cond::Eq(81, 1.0_f64),
        ],
    ),
    // 438 btcusdt_15m_rules_438: GREEN
    (
        true,
        &[
            Cond::Le(71, 15.545991535082_f64),
            Cond::Ge(13, -21.140333967991_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 439 btcusdt_15m_rules_439: GREEN
    (
        true,
        &[
            Cond::Le(63, 12.039352357177_f64),
            Cond::Ge(45, -0.002122380767_f64),
            Cond::Eq(42, 12.0_f64),
        ],
    ),
    // 440 btcusdt_15m_rules_440: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.148849848163_f64),
            Cond::Ge(50, 1425.0_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 441 btcusdt_15m_rules_441: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.058231615_f64),
            Cond::Ge(30, 0.02558085724_f64),
            Cond::Ge(26, -0.02252162313_f64),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 442 btcusdt_15m_rules_442: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::In(81, &[2.0_f64]),
            Cond::In(42, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 443 btcusdt_15m_rules_443: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(75, 0.011732866886_f64),
            Cond::Eq(42, 14.0_f64),
        ],
    ),
    // 444 btcusdt_15m_rules_444: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Eq(42, 11.0_f64),
            Cond::Le(28, 0.004305280217_f64),
            Cond::Eq(81, 0.0_f64),
        ],
    ),
    // 445 btcusdt_15m_rules_445: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Ge(20, 0.010119255562_f64),
            Cond::Eq(68, 1.0_f64),
        ],
    ),
    // 446 btcusdt_15m_rules_446: GREEN
    (
        true,
        &[
            Cond::Le(12, -188.547426560093_f64),
            Cond::Ge(39, -0.000805259118_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 447 btcusdt_15m_rules_447: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000123157637_f64),
            Cond::Ge(33, 1.0_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 448 btcusdt_15m_rules_448: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.242308839024_f64),
            Cond::Ge(70, 61.27763335905_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 449 btcusdt_15m_rules_449: GREEN
    (
        true,
        &[
            Cond::Le(70, 1.679463493_f64),
            Cond::Ge(6, 0.008140445126_f64),
            Cond::Le(46, -0.00345019142_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 450 btcusdt_15m_rules_450: RED
    (
        false,
        &[
            Cond::Ge(49, 92.538838851052_f64),
            Cond::In(42, &[16.0_f64]),
            Cond::Eq(81, 6.0_f64),
        ],
    ),
    // 451 btcusdt_15m_rules_451: GREEN
    (
        true,
        &[
            Cond::Le(71, 2.417455072988_f64),
            Cond::Between(52, -0.614786474045_f64, 0.207539773281_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
    // 452 btcusdt_15m_rules_452: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Le(43, 0.00002645309807_f64),
            Cond::Ge(8, -0.007253030737_f64),
            Cond::Eq(67, 1.0_f64),
        ],
    ),
    // 453 btcusdt_15m_rules_453: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(15, 0.000045742666_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 454 btcusdt_15m_rules_454: GREEN
    (
        true,
        &[
            Cond::Le(41, 0.218185817798_f64),
            Cond::Ge(50, 1320.0_f64),
            Cond::Eq(81, 4.0_f64),
        ],
    ),
    // 455 btcusdt_15m_rules_455: RED
    (
        false,
        &[
            Cond::Ge(70, 89.541313522713_f64),
            Cond::Ge(0, 30.625040783455_f64),
            Cond::Eq(81, 2.0_f64),
        ],
    ),
    // 456 btcusdt_15m_rules_456: RED
    (
        false,
        &[
            Cond::Ge(4, 1.075503944993_f64),
            Cond::Le(52, -0.822178298076_f64),
            Cond::In(
                42,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 457 btcusdt_15m_rules_457: RED
    (
        false,
        &[
            Cond::Ge(49, 90.21492658429_f64),
            Cond::Le(11, -0.479441712554_f64),
            Cond::Eq(42, 10.0_f64),
        ],
    ),
    // 458 btcusdt_15m_rules_458: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(79, -0.003994395698_f64),
            Cond::Eq(81, 3.0_f64),
        ],
    ),
    // 459 btcusdt_15m_rules_459: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Between(49, 37.154692990356_f64, 62.524777166172_f64),
            Cond::Eq(81, 5.0_f64),
        ],
    ),
    // 460 btcusdt_15m_rules_460: GREEN
    (
        true,
        &[
            Cond::Le(70, 3.503757061587_f64),
            Cond::Between(44, 0.276646203011_f64, 1.3696048831_f64),
            Cond::Eq(42, 23.0_f64),
        ],
    ),
    // 461 btcusdt_15m_rules_461: RED
    (
        false,
        &[
            Cond::Ge(70, 94.73247534402_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(42, 9.0_f64),
        ],
    ),
];

pub struct BtcM15Rules461 {
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

impl BtcM15Rules461 {
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

impl Strategy for BtcM15Rules461 {
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
