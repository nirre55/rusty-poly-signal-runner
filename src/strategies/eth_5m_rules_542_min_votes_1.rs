use chrono::{Datelike, Timelike};
use std::collections::VecDeque;
use tracing::debug;

use crate::binance::Candle;
use crate::strategy::{Prediction, Signal, Strategy};

const MAX_WINDOW: usize = 160;
const STRATEGY_NAME: &str = "eth_5m_rules_542_min_votes_1";
const FEATURE_COUNT: usize = 81;

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
// 64=session_london
// 65=session_overlap_london_us
// 66=session_us
// 67=signed_volume_ratio20
// 68=stoch_k12
// 69=stoch_k24
// 70=stoch_k72
// 71=upper_wick
// 72=upper_wick_body
// 73=volume_body_efficiency
// 74=volume_range_efficiency
// 75=volume_ratio20
// 76=volume_z24
// 77=volume_z96
// 78=vwap_slope24
// 79=vwap_slope72
// 80=weekday
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
    f[64] = Some(session_london(minute_of_day));
    f[65] = Some(session_overlap_london_us(minute_of_day));
    f[66] = Some(session_us(minute_of_day));
    f[67] = signed_vol_ratio(buf, 20);
    f[68] = stoch_k(buf, 12, close);
    f[69] = stoch_k(buf, 24, close);
    f[70] = stoch_k(buf, 72, close);
    f[71] = upper_wick;
    f[72] = upper_wick_body;
    f[73] = vol_body_eff(buf);
    f[74] = vol_range_eff(buf);
    f[75] = volume_ratio(buf, 20);
    f[76] = vol_z(buf, 24);
    f[77] = vol_z(buf, 96);
    f[78] = vwap_slope(buf, 24);
    f[79] = vwap_slope(buf, 72);
    f[80] = Some(weekday);
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
    // 1 eth_m5_rules_1: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.013135821846_f64),
            Cond::Ge(19, 2.080444997831_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 2 eth_m5_rules_2: RED
    (
        false,
        &[
            Cond::Ge(55, 0.006173028957_f64),
            Cond::Le(62, 27.867709092007_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 3 eth_m5_rules_3: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.013135821846_f64),
            Cond::Ge(19, 2.080444997831_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 4 eth_m5_rules_4: GREEN
    (
        true,
        &[Cond::Le(24, -0.103167040398_f64), Cond::Ge(65, 1.0_f64)],
    ),
    // 5 eth_m5_rules_5: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.705007578106_f64),
            Cond::Ge(67, 3.241967620561_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 6 eth_m5_rules_6: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.005185093586_f64),
            Cond::Ge(12, 197.29361706083_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 7 eth_m5_rules_7: RED
    (
        false,
        &[
            Cond::Ge(40, 0.644678507722_f64),
            Cond::Ge(6, 0.010655328013_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 8 eth_m5_rules_8: RED
    (
        false,
        &[
            Cond::Ge(27, 0.095760974066_f64),
            Cond::Le(72, 0.018666518665_f64),
        ],
    ),
    // 9 eth_m5_rules_9: RED
    (
        false,
        &[
            Cond::Ge(27, 0.113311886667_f64),
            Cond::Le(50, 0.545495059332_f64),
        ],
    ),
    // 10 eth_m5_rules_10: RED
    (
        false,
        &[
            Cond::Ge(40, 0.644678507722_f64),
            Cond::Ge(6, 0.010655328013_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 11 eth_m5_rules_11: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::In(80, &[5.0_f64]),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 12 eth_m5_rules_12: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(78, -0.000430553703_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 13 eth_m5_rules_13: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404171868_f64),
            Cond::Le(38, -0.007640254912_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 14 eth_m5_rules_14: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.5443385043_f64),
            Cond::Eq(41, 5.0_f64),
            Cond::Le(61, 39.11398072_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 15 eth_m5_rules_15: GREEN
    (
        true,
        &[
            Cond::Le(69, 15.180011010737_f64),
            Cond::Ge(72, 102.022412698827_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 16 eth_m5_rules_16: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Ge(24, -0.007190350995_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 17 eth_m5_rules_17: RED
    (
        false,
        &[
            Cond::Ge(17, 3.082851148_f64),
            Cond::Eq(80, 3.0_f64),
            Cond::Le(47, 66.52204642_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 18 eth_m5_rules_18: GREEN
    (
        true,
        &[
            Cond::Le(68, 1.371097431342_f64),
            Cond::Ge(79, 0.010852199486_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 19 eth_m5_rules_19: GREEN
    (
        true,
        &[
            Cond::Le(69, 5.132606156_f64),
            Cond::Ge(59, 0.02599541236_f64),
            Cond::Le(62, 24.46140344_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 20 eth_m5_rules_20: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Ge(72, 5.117647059_f64),
            Cond::Ge(42, 0.00001612549971_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 21 eth_m5_rules_21: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.263095943275_f64),
            Cond::Le(52, 0.0_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 22 eth_m5_rules_22: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.089556532241_f64),
            Cond::Le(79, -0.015077248431_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 23 eth_m5_rules_23: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(10, -0.004965357046_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 24 eth_m5_rules_24: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(1, -1.127098909018_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 25 eth_m5_rules_25: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.220922553819_f64),
            Cond::Ge(44, 0.000404314765_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 26 eth_m5_rules_26: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.110392815456_f64),
            Cond::Ge(1, 5.602555707271_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 27 eth_m5_rules_27: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.04489019383_f64),
            Cond::Ge(7, 0.9987638412_f64),
            Cond::Ge(13, -201.1674785_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 28 eth_m5_rules_28: RED
    (
        false,
        &[
            Cond::Ge(68, 89.541313522713_f64),
            Cond::Ge(0, 30.625040783455_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 29 eth_m5_rules_29: GREEN
    (
        true,
        &[
            Cond::Le(69, 9.711755951452_f64),
            Cond::Ge(1, 13.438071751687_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 30 eth_m5_rules_30: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.787062464583_f64),
            Cond::Ge(45, 0.000533890036_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 31 eth_m5_rules_31: GREEN
    (
        true,
        &[
            Cond::Le(68, 5.418823304347_f64),
            Cond::Ge(19, 1.731253361596_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 32 eth_m5_rules_32: RED
    (
        false,
        &[
            Cond::Ge(25, -0.00135922631_f64),
            Cond::Ge(45, 0.013873233781_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 33 eth_m5_rules_33: GREEN
    (
        true,
        &[
            Cond::Le(13, -296.373970792508_f64),
            Cond::Ge(38, -0.003927732991_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 34 eth_m5_rules_34: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.293693136323_f64),
            Cond::Ge(72, 0.389314900769_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 35 eth_m5_rules_35: GREEN
    (
        true,
        &[
            Cond::Le(62, 19.291134799525_f64),
            Cond::Le(11, -0.313680587227_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 36 eth_m5_rules_36: GREEN
    (
        true,
        &[
            Cond::Le(68, 15.251559689203_f64),
            Cond::Ge(72, 102.022412698827_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 37 eth_m5_rules_37: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.970296088074_f64),
            Cond::Le(39, 0.118286862474_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 38 eth_m5_rules_38: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(19, 0.590766072643_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 39 eth_m5_rules_39: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404171868_f64),
            Cond::Le(38, -0.007640254912_f64),
            Cond::Ge(6, 0.007312429007_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 40 eth_m5_rules_40: GREEN
    (
        true,
        &[
            Cond::Le(69, 2.898892702_f64),
            Cond::Le(44, -0.002344176743_f64),
            Cond::Ge(50, 1.403339542_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 41 eth_m5_rules_41: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(61, 31.37459303_f64),
            Cond::Le(30, 0.0001481980796_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 42 eth_m5_rules_42: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Ge(18, 3.429563387_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 43 eth_m5_rules_43: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 44 eth_m5_rules_44: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.273882464262_f64),
            Cond::Ge(29, 0.030155709967_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 45 eth_m5_rules_45: GREEN
    (
        true,
        &[
            Cond::Le(69, 5.132606156_f64),
            Cond::Ge(59, 0.02599541236_f64),
            Cond::Le(62, 24.46140344_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 46 eth_m5_rules_46: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Ge(72, 1.27376961958_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 47 eth_m5_rules_47: GREEN
    (
        true,
        &[
            Cond::Ge(53, 5.0_f64),
            Cond::Ge(40, 0.713503577816_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 48 eth_m5_rules_48: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000362649771_f64),
            Cond::Ge(2, 0.0076658772_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 49 eth_m5_rules_49: RED
    (
        false,
        &[
            Cond::Ge(12, 211.785069809589_f64),
            Cond::Ge(7, 1.0_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 50 eth_m5_rules_50: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.452207778726_f64),
            Cond::Ge(27, 0.000134239291_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 51 eth_m5_rules_51: RED
    (
        false,
        &[
            Cond::Ge(48, 92.538838851052_f64),
            Cond::Le(11, -0.227167506527_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 52 eth_m5_rules_52: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.156166218685_f64),
            Cond::Le(1, -42.545668662262_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 53 eth_m5_rules_53: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.083867390324_f64),
            Cond::Le(76, -1.100475105203_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 54 eth_m5_rules_54: RED
    (
        false,
        &[
            Cond::Ge(68, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 55 eth_m5_rules_55: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006404179203_f64),
            Cond::Le(24, -0.02387268949_f64),
            Cond::Le(36, 1.0_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 56 eth_m5_rules_56: RED
    (
        false,
        &[
            Cond::Ge(10, 0.01364096683_f64),
            Cond::Ge(24, -0.000509590257_f64),
            Cond::Le(57, 0.0173612306_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 57 eth_m5_rules_57: GREEN
    (
        true,
        &[
            Cond::Le(69, 7.662631226_f64),
            Cond::Le(24, -0.04360964035_f64),
            Cond::Le(2, 0.01278625059_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 58 eth_m5_rules_58: GREEN
    (
        true,
        &[
            Cond::Le(12, -209.954494058912_f64),
            Cond::Le(79, -0.011145677221_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 59 eth_m5_rules_59: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Ge(72, 5.117647059_f64),
            Cond::Ge(42, 0.00001612549971_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 60 eth_m5_rules_60: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.025147324264_f64),
            Cond::Ge(12, -90.37920447042_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 61 eth_m5_rules_61: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Le(50, 0.470794215282_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 62 eth_m5_rules_62: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Ge(13, -43.945068697837_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 63 eth_m5_rules_63: RED
    (
        false,
        &[
            Cond::Ge(4, 1.011928971326_f64),
            Cond::Le(51, -1.203389651906_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 64 eth_m5_rules_64: GREEN
    (
        true,
        &[
            Cond::Le(12, -225.990615221245_f64),
            Cond::Ge(38, -0.00191371853_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 65 eth_m5_rules_65: GREEN
    (
        true,
        &[
            Cond::Le(62, 20.069277361163_f64),
            Cond::Between(46, 44.867293806931_f64, 55.028092949405_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 66 eth_m5_rules_66: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000362649771_f64),
            Cond::Ge(29, 0.047966000661_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 67 eth_m5_rules_67: RED
    (
        false,
        &[
            Cond::Ge(17, 2.801397412222_f64),
            Cond::Le(79, -0.007352033255_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 68 eth_m5_rules_68: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(78, -0.011503037214_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 69 eth_m5_rules_69: RED
    (
        false,
        &[
            Cond::Ge(17, 3.093219443607_f64),
            Cond::Le(2, 0.000515254142_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 70 eth_m5_rules_70: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Le(50, 1.586225659_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 71 eth_m5_rules_71: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.005317545063_f64),
            Cond::Le(6, 0.005359445082_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 72 eth_m5_rules_72: GREEN
    (
        true,
        &[
            Cond::Le(68, 24.430142129257_f64),
            Cond::Ge(4, 0.615571013053_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 73 eth_m5_rules_73: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.970296088074_f64),
            Cond::Ge(67, 1.841596579591_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 74 eth_m5_rules_74: GREEN
    (
        true,
        &[
            Cond::Le(69, 9.711755951452_f64),
            Cond::Ge(9, 0.001375151652_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 75 eth_m5_rules_75: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.001555592433_f64),
            Cond::Ge(63, 72.617548923107_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 76 eth_m5_rules_76: GREEN
    (
        true,
        &[
            Cond::Le(18, -2.447691672_f64),
            Cond::Le(3, 0.0006406963614_f64),
            Cond::Le(8, -0.004124824725_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 77 eth_m5_rules_77: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.673958918983_f64),
            Cond::Ge(44, 0.000552259724_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 78 eth_m5_rules_78: RED
    (
        false,
        &[
            Cond::Ge(4, 1.075503944993_f64),
            Cond::Le(51, -0.822178298076_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 79 eth_m5_rules_79: RED
    (
        false,
        &[
            Cond::Ge(24, -2.58479000000000e-7_f64),
            Cond::Ge(56, 0.019271469794_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 80 eth_m5_rules_80: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(51, 1.413382543016_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 81 eth_m5_rules_81: GREEN
    (
        true,
        &[
            Cond::Le(62, 20.625140058973_f64),
            Cond::Ge(79, 0.009956278533_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 82 eth_m5_rules_82: RED
    (
        false,
        &[
            Cond::Ge(68, 97.683323069458_f64),
            Cond::Le(46, 31.262587312934_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 83 eth_m5_rules_83: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(74, 0.011732866886_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 84 eth_m5_rules_84: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Ge(73, 0.005051760632_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 85 eth_m5_rules_85: RED
    (
        false,
        &[
            Cond::Ge(4, 1.060875802045_f64),
            Cond::Le(39, 0.140781768746_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 86 eth_m5_rules_86: GREEN
    (
        true,
        &[
            Cond::Le(57, -0.05666612446_f64),
            Cond::Le(61, 28.5686663_f64),
            Cond::Le(13, -264.6708728_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 87 eth_m5_rules_87: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(12, -213.8206725_f64),
            Cond::Ge(70, 2.887028121_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 88 eth_m5_rules_88: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.644587669_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(43, 0.01797752809_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 89 eth_m5_rules_89: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.644587669_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(43, 0.01797752809_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 90 eth_m5_rules_90: GREEN
    (
        true,
        &[
            Cond::Le(68, 2.062588143616_f64),
            Cond::Le(46, 9.622580642198_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 91 eth_m5_rules_91: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Ge(72, 10.072336134454_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 92 eth_m5_rules_92: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.00254012653_f64),
            Cond::Ge(42, 0.002459555014_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 93 eth_m5_rules_93: GREEN
    (
        true,
        &[
            Cond::Le(48, 5.127057896226_f64),
            Cond::Le(1, -1.893164059008_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 94 eth_m5_rules_94: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Ge(12, -124.741159730216_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 95 eth_m5_rules_95: GREEN
    (
        true,
        &[
            Cond::Ge(53, 5.0_f64),
            Cond::Ge(40, 0.713503577816_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 96 eth_m5_rules_96: GREEN
    (
        true,
        &[
            Cond::Le(69, 4.960424772543_f64),
            Cond::Ge(1, 23.119074919152_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 97 eth_m5_rules_97: GREEN
    (
        true,
        &[
            Cond::Le(69, 4.960424772543_f64),
            Cond::Ge(1, 23.119074919152_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 98 eth_m5_rules_98: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.089556532241_f64),
            Cond::Le(74, 0.000473607612_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 99 eth_m5_rules_99: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.089556532241_f64),
            Cond::Ge(38, -0.001496400714_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 100 eth_m5_rules_100: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.005731108736_f64),
            Cond::Ge(30, 0.02923394723_f64),
            Cond::Ge(28, 0.04226528076_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 101 eth_m5_rules_101: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 21.0_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 102 eth_m5_rules_102: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006709289818_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Le(77, 2.912906413_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 103 eth_m5_rules_103: RED
    (
        false,
        &[
            Cond::Ge(4, 1.244413952997_f64),
            Cond::Ge(33, 10.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 104 eth_m5_rules_104: GREEN
    (
        true,
        &[
            Cond::Le(13, -198.155728544659_f64),
            Cond::Ge(12, -90.330450456706_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 105 eth_m5_rules_105: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::In(80, &[5.0_f64]),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 106 eth_m5_rules_106: RED
    (
        false,
        &[
            Cond::Ge(68, 96.35554424836_f64),
            Cond::Le(44, -0.000547643196_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 107 eth_m5_rules_107: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.65525868_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(70, 10.166951_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 108 eth_m5_rules_108: GREEN
    (
        true,
        &[
            Cond::Le(24, -0.03797357864_f64),
            Cond::Le(47, 13.8331558_f64),
            Cond::Le(18, -3.063615586_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 109 eth_m5_rules_109: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.083545302651_f64),
            Cond::Le(75, 0.342897905517_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 110 eth_m5_rules_110: RED
    (
        false,
        &[
            Cond::Ge(17, 3.093219443607_f64),
            Cond::Le(2, 0.000515254142_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 111 eth_m5_rules_111: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Ge(1, 4.325244011802_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 112 eth_m5_rules_112: GREEN
    (
        true,
        &[
            Cond::Le(69, 7.662631226_f64),
            Cond::Le(24, -0.04360964035_f64),
            Cond::Le(2, 0.01278625059_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 113 eth_m5_rules_113: GREEN
    (
        true,
        &[
            Cond::Le(13, -183.059643654916_f64),
            Cond::Le(15, 0.0_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 114 eth_m5_rules_114: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::In(41, &[9.0_f64]),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 115 eth_m5_rules_115: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.089556532241_f64),
            Cond::Le(79, -0.015077248431_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 116 eth_m5_rules_116: GREEN
    (
        true,
        &[
            Cond::Le(69, 9.711755951452_f64),
            Cond::Ge(72, 24.524084565759_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 117 eth_m5_rules_117: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.107140425_f64),
            Cond::Le(3, 0.0004654084234_f64),
            Cond::Ge(2, 0.0002988690878_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 118 eth_m5_rules_118: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(78, -0.000430553703_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 119 eth_m5_rules_119: RED
    (
        false,
        &[
            Cond::Ge(62, 79.187266351359_f64),
            Cond::Ge(73, 0.007704921362_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 120 eth_m5_rules_120: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Ge(62, 65.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.6_f64),
            Cond::Eq(80, 6.0_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 121 eth_m5_rules_121: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::Ge(7, 0.841316476733_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 122 eth_m5_rules_122: RED
    (
        false,
        &[Cond::Le(53, 0.0_f64), Cond::Ge(38, 0.026410385548_f64)],
    ),
    // 123 eth_m5_rules_123: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(12, 262.3666355551_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 124 eth_m5_rules_124: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.65908861946_f64),
            Cond::In(41, &[22.0_f64]),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 125 eth_m5_rules_125: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::In(80, &[5.0_f64]),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 126 eth_m5_rules_126: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.112495107_f64),
            Cond::Le(24, -0.09779320525_f64),
            Cond::Ge(12, -175.5808578_f64),
        ],
    ),
    // 127 eth_m5_rules_127: GREEN
    (
        true,
        &[
            Cond::Le(13, -153.526550112837_f64),
            Cond::Le(76, -0.992604162908_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 128 eth_m5_rules_128: RED
    (
        false,
        &[
            Cond::Ge(68, 95.010861843374_f64),
            Cond::Le(46, 30.621116950057_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 129 eth_m5_rules_129: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.334336613744_f64),
            Cond::Ge(46, 75.524132937806_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 130 eth_m5_rules_130: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 22.0_f64),
            Cond::Le(71, 9.22394520900000e-8_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 131 eth_m5_rules_131: RED
    (
        false,
        &[
            Cond::Ge(63, 73.82299439_f64),
            Cond::Le(59, -0.01475596055_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 132 eth_m5_rules_132: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.004596587699_f64),
            Cond::Le(2, 0.001646300512_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 133 eth_m5_rules_133: RED
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
    // 134 eth_m5_rules_134: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(62, 35.0_f64),
            Cond::Ge(50, 1.2_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 12.0_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 135 eth_m5_rules_135: RED
    (
        false,
        &[
            Cond::Ge(63, 80.23066448_f64),
            Cond::Le(18, 1.456773235_f64),
            Cond::Le(46, 73.70646328_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 136 eth_m5_rules_136: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.65525868_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(70, 10.166951_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 137 eth_m5_rules_137: RED
    (
        false,
        &[
            Cond::Ge(63, 85.600813068941_f64),
            Cond::Between(78, -0.003184483329_f64, 0.003367010176_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 138 eth_m5_rules_138: GREEN
    (
        true,
        &[
            Cond::Le(68, 5.767007525744_f64),
            Cond::Le(51, -1.397975506363_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 139 eth_m5_rules_139: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.240426681222_f64),
            Cond::Ge(23, 0.013975919138_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 140 eth_m5_rules_140: RED
    (
        false,
        &[
            Cond::Ge(69, 92.398919564528_f64),
            Cond::Le(40, 0.426292967876_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 141 eth_m5_rules_141: GREEN
    (
        true,
        &[
            Cond::Le(60, 21.149731261368_f64),
            Cond::Ge(13, -88.21972132586_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 142 eth_m5_rules_142: GREEN
    (
        true,
        &[
            Cond::Le(68, 0.6862995766_f64),
            Cond::Le(17, -2.808854839_f64),
            Cond::Le(6, 0.003473998504_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 143 eth_m5_rules_143: GREEN
    (
        true,
        &[
            Cond::Le(69, 2.417455072988_f64),
            Cond::Ge(0, 0.228653572966_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 144 eth_m5_rules_144: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.452207778726_f64),
            Cond::Ge(27, 0.000134239291_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 145 eth_m5_rules_145: RED
    (
        false,
        &[
            Cond::Ge(68, 97.251019949653_f64),
            Cond::Ge(71, 0.001007899334_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 146 eth_m5_rules_146: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(68, 95.36043284_f64),
            Cond::Ge(47, 73.95404425_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 147 eth_m5_rules_147: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Le(26, -0.001004677022_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 148 eth_m5_rules_148: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(28, 0.001091384768_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 149 eth_m5_rules_149: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(78, -0.000430553703_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 150 eth_m5_rules_150: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.174372887633_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 151 eth_m5_rules_151: GREEN
    (
        true,
        &[
            Cond::Le(8, -0.07163992181_f64),
            Cond::Le(68, 8.601601931_f64),
            Cond::Le(48, 20.08710485_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 152 eth_m5_rules_152: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(46, 39.31642972534_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 153 eth_m5_rules_153: GREEN
    (
        true,
        &[
            Cond::Le(62, 18.659987150027_f64),
            Cond::Le(19, 0.612626728912_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 154 eth_m5_rules_154: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 155 eth_m5_rules_155: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(6, 0.000012687105_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 156 eth_m5_rules_156: GREEN
    (
        true,
        &[
            Cond::Le(13, -206.043561926848_f64),
            Cond::Le(1, -0.610353276712_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 157 eth_m5_rules_157: RED
    (
        false,
        &[
            Cond::Ge(68, 93.154663125084_f64),
            Cond::Le(76, -1.38757033289_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 158 eth_m5_rules_158: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(23, -0.017233546983_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 159 eth_m5_rules_159: GREEN
    (
        true,
        &[
            Cond::Le(12, -185.239722022272_f64),
            Cond::Ge(73, 0.0067308277_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 160 eth_m5_rules_160: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(3, 0.001411663091_f64),
            Cond::Ge(6, 0.003473998504_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 161 eth_m5_rules_161: GREEN
    (
        true,
        &[
            Cond::Le(57, -0.05666612446_f64),
            Cond::Le(61, 28.5686663_f64),
            Cond::Le(13, -264.6708728_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 162 eth_m5_rules_162: GREEN
    (
        true,
        &[
            Cond::Le(13, -296.373970792508_f64),
            Cond::Ge(38, -0.003927732991_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 163 eth_m5_rules_163: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.282482101409_f64),
            Cond::Ge(21, 0.009380892054_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 164 eth_m5_rules_164: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(60, 44.439233481289_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 165 eth_m5_rules_165: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(0, 4.657215509588_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 166 eth_m5_rules_166: GREEN
    (
        true,
        &[
            Cond::Le(68, 6.452676568325_f64),
            Cond::Le(0, -1.915883231776_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 167 eth_m5_rules_167: RED
    (
        false,
        &[
            Cond::Ge(16, 1.722377357787_f64),
            Cond::Ge(1, 25.309101221933_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 168 eth_m5_rules_168: RED
    (
        false,
        &[
            Cond::Ge(62, 78.148726107272_f64),
            Cond::Le(48, 55.944196476296_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 169 eth_m5_rules_169: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.878329763064_f64),
            Cond::Le(76, -0.765737264252_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 170 eth_m5_rules_170: RED
    (
        false,
        &[
            Cond::Ge(69, 98.04361321_f64),
            Cond::Ge(8, 0.02432418065_f64),
            Cond::Ge(37, 3.0_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 171 eth_m5_rules_171: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Le(3, 0.0006406964493_f64),
            Cond::Ge(69, 10.74126157_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 172 eth_m5_rules_172: RED
    (
        false,
        &[
            Cond::Ge(48, 94.13387702_f64),
            Cond::Ge(24, -0.0001214530509_f64),
            Cond::Le(69, 99.45945562_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 173 eth_m5_rules_173: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(46, 45.114842308581_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 174 eth_m5_rules_174: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(15, 0.000045742666_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 175 eth_m5_rules_175: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.210641246093_f64),
            Cond::Ge(29, 0.01508702537_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 176 eth_m5_rules_176: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000917948329_f64),
            Cond::Le(13, -19.900866452413_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 177 eth_m5_rules_177: RED
    (
        false,
        &[
            Cond::Ge(69, 98.13255444133_f64),
            Cond::Ge(19, 2.511911306736_f64),
        ],
    ),
    // 178 eth_m5_rules_178: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.235606694343_f64),
            Cond::Ge(78, 0.001053177884_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 179 eth_m5_rules_179: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(6, 0.007864217465_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 180 eth_m5_rules_180: GREEN
    (
        true,
        &[
            Cond::Le(69, 5.132606156_f64),
            Cond::Ge(59, 0.02599541236_f64),
            Cond::Le(62, 24.46140344_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 181 eth_m5_rules_181: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.0006709289818_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Le(77, 2.912906413_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 182 eth_m5_rules_182: RED
    (
        false,
        &[
            Cond::Ge(18, 3.429940156_f64),
            Cond::Le(8, 0.005753340807_f64),
            Cond::Ge(61, 67.95174536_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 183 eth_m5_rules_183: GREEN
    (
        true,
        &[
            Cond::Le(56, -0.03224343338_f64),
            Cond::Le(68, 3.779328959_f64),
            Cond::Le(13, -175.7548746_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 184 eth_m5_rules_184: GREEN
    (
        true,
        &[
            Cond::Le(62, 28.488182057264_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 185 eth_m5_rules_185: GREEN
    (
        true,
        &[
            Cond::Le(62, 28.488182057264_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 186 eth_m5_rules_186: GREEN
    (
        true,
        &[
            Cond::Le(63, 27.937723759111_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 187 eth_m5_rules_187: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Between(50, 0.754270965462_f64, 0.952041479441_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 188 eth_m5_rules_188: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Le(75, 0.604709102916_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 189 eth_m5_rules_189: RED
    (
        false,
        &[
            Cond::Ge(69, 98.632249177409_f64),
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 190 eth_m5_rules_190: RED
    (
        false,
        &[
            Cond::Ge(69, 94.755383566354_f64),
            Cond::Le(67, -0.942445884865_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 191 eth_m5_rules_191: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(6, 0.000012687105_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 192 eth_m5_rules_192: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(51, -1.397975506363_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 193 eth_m5_rules_193: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(51, -0.052398857241_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 194 eth_m5_rules_194: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(51, -0.052398857241_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 195 eth_m5_rules_195: GREEN
    (
        true,
        &[
            Cond::Le(69, 6.753473519311_f64),
            Cond::Ge(43, 5.286416861829_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 196 eth_m5_rules_196: GREEN
    (
        true,
        &[
            Cond::Le(63, 19.860943343755_f64),
            Cond::Ge(78, 0.00080334357_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 197 eth_m5_rules_197: RED
    (
        false,
        &[
            Cond::Ge(4, 1.244413952997_f64),
            Cond::Le(38, 0.001148336911_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 198 eth_m5_rules_198: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.01125914436_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Le(71, 8.42947706900000e-8_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 199 eth_m5_rules_199: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000544949204_f64),
            Cond::Ge(23, 0.018572337802_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 200 eth_m5_rules_200: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(10, -0.004965357046_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 201 eth_m5_rules_201: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(74, 0.007974367829_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 202 eth_m5_rules_202: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Ge(7, 0.9656401664_f64),
            Cond::Ge(57, -0.01042349892_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 203 eth_m5_rules_203: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::In(80, &[5.0_f64]),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 204 eth_m5_rules_204: GREEN
    (
        true,
        &[
            Cond::Le(20, -0.027370425915_f64),
            Cond::Ge(49, 1320.0_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 205 eth_m5_rules_205: RED
    (
        false,
        &[
            Cond::Ge(68, 99.566819537935_f64),
            Cond::In(41, &[15.0_f64]),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 206 eth_m5_rules_206: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.110392815456_f64),
            Cond::Le(74, 0.00027449778_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 207 eth_m5_rules_207: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000544949204_f64),
            Cond::Le(48, 0.0_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 208 eth_m5_rules_208: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.65525868_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(70, 10.166951_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 209 eth_m5_rules_209: GREEN
    (
        true,
        &[
            Cond::Le(68, 3.592157413_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Le(12, -145.0194062_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 210 eth_m5_rules_210: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(10, -0.004965357046_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 211 eth_m5_rules_211: RED
    (
        false,
        &[
            Cond::Ge(68, 96.3831114696_f64),
            Cond::Le(46, 33.955352912075_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 212 eth_m5_rules_212: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(61, 31.37459303_f64),
            Cond::Le(30, 0.0001481980796_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 213 eth_m5_rules_213: RED
    (
        false,
        &[
            Cond::Ge(63, 80.23066448_f64),
            Cond::Le(18, 1.456773235_f64),
            Cond::Le(46, 73.70646328_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 214 eth_m5_rules_214: GREEN
    (
        true,
        &[
            Cond::Le(69, 2.417455072988_f64),
            Cond::Ge(0, 0.228653572966_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 215 eth_m5_rules_215: GREEN
    (
        true,
        &[
            Cond::Le(63, 12.722439334531_f64),
            Cond::Ge(23, -0.012496232218_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 216 eth_m5_rules_216: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000184374788_f64),
            Cond::Le(11, -0.436252860345_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 217 eth_m5_rules_217: GREEN
    (
        true,
        &[
            Cond::Le(68, 1.679463493_f64),
            Cond::Ge(6, 0.008140445126_f64),
            Cond::Le(45, -0.00345019142_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 218 eth_m5_rules_218: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(8, -0.005758468918_f64),
            Cond::Le(28, 0.001091384768_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 219 eth_m5_rules_219: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.235606694343_f64),
            Cond::Ge(78, 0.001053177884_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 220 eth_m5_rules_220: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(78, -0.003994395698_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 221 eth_m5_rules_221: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.583474519884_f64),
            Cond::Ge(40, 0.603485537608_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 222 eth_m5_rules_222: GREEN
    (
        true,
        &[
            Cond::Le(68, 0.6862995766_f64),
            Cond::Le(17, -2.808854839_f64),
            Cond::Le(6, 0.003473998504_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 223 eth_m5_rules_223: GREEN
    (
        true,
        &[
            Cond::Le(69, 20.19209860754_f64),
            Cond::Le(75, 0.216880790466_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 224 eth_m5_rules_224: RED
    (
        false,
        &[
            Cond::Le(52, 0.0_f64),
            Cond::Le(76, -1.457487435051_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 225 eth_m5_rules_225: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.133228681061_f64),
            Cond::Between(76, -0.470588798961_f64, -0.128994916769_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 226 eth_m5_rules_226: GREEN
    (
        true,
        &[
            Cond::Le(69, 6.753473519311_f64),
            Cond::Ge(43, 5.286416861829_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 227 eth_m5_rules_227: RED
    (
        false,
        &[
            Cond::Ge(46, 89.80044543946_f64),
            Cond::Le(60, 60.198393960611_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 228 eth_m5_rules_228: GREEN
    (
        true,
        &[
            Cond::Le(68, 15.251559689203_f64),
            Cond::Ge(72, 102.022412698827_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 229 eth_m5_rules_229: GREEN
    (
        true,
        &[
            Cond::Le(69, 13.103119286977_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 230 eth_m5_rules_230: RED
    (
        false,
        &[
            Cond::Ge(69, 89.373499384241_f64),
            Cond::Le(68, 70.210545155072_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 231 eth_m5_rules_231: RED
    (
        false,
        &[
            Cond::Ge(69, 92.398919564528_f64),
            Cond::Le(68, 78.724397723401_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 232 eth_m5_rules_232: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.878329763064_f64),
            Cond::Le(76, -0.765737264252_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 233 eth_m5_rules_233: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001091384106_f64),
            Cond::Le(10, -0.01817603669_f64),
            Cond::Ge(77, 2.919388313_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 234 eth_m5_rules_234: RED
    (
        false,
        &[
            Cond::Ge(26, -0.000212151614_f64),
            Cond::Eq(41, 12.0_f64),
            Cond::Le(4, 1.061948757_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 235 eth_m5_rules_235: RED
    (
        false,
        &[
            Cond::Ge(17, 3.068414947_f64),
            Cond::Le(18, 1.903776951_f64),
            Cond::Le(28, 0.03033879208_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 236 eth_m5_rules_236: RED
    (
        false,
        &[
            Cond::Ge(60, 77.03278368_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Le(77, 2.919374564_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 237 eth_m5_rules_237: GREEN
    (
        true,
        &[
            Cond::Le(12, -188.547426560093_f64),
            Cond::Le(51, -0.822178298076_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 238 eth_m5_rules_238: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(78, -0.000430553703_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 239 eth_m5_rules_239: RED
    (
        false,
        &[
            Cond::Ge(68, 99.185382275768_f64),
            Cond::Le(79, -0.009431026424_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 240 eth_m5_rules_240: GREEN
    (
        true,
        &[
            Cond::Le(9, -0.007111471533_f64),
            Cond::Ge(31, 1.0_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 241 eth_m5_rules_241: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(45, 0.004216605253_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 242 eth_m5_rules_242: RED
    (
        false,
        &[
            Cond::Ge(69, 94.302371417231_f64),
            Cond::Le(58, -0.000384754229_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 243 eth_m5_rules_243: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(62, 70.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(41, &[11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64, 15.0_f64]),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 244 eth_m5_rules_244: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.26951785_f64),
            Cond::Le(42, 0.0001510480269_f64),
            Cond::Ge(14, -111.0492288_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 245 eth_m5_rules_245: GREEN
    (
        true,
        &[
            Cond::Le(69, 0.5443385043_f64),
            Cond::Eq(41, 5.0_f64),
            Cond::Le(61, 39.11398072_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 246 eth_m5_rules_246: RED
    (
        false,
        &[
            Cond::Ge(18, 3.429940156_f64),
            Cond::Le(8, 0.005753340807_f64),
            Cond::Ge(61, 67.95174536_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 247 eth_m5_rules_247: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(2, 0.001277921698_f64),
            Cond::Le(47, 33.22794653_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 248 eth_m5_rules_248: GREEN
    (
        true,
        &[
            Cond::Le(68, 2.062588143616_f64),
            Cond::Le(46, 9.622580642198_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 249 eth_m5_rules_249: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.742641598641_f64),
            Cond::Ge(46, 55.226633333302_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 250 eth_m5_rules_250: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(72, 0.151279003962_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 251 eth_m5_rules_251: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(72, 0.151279003962_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 252 eth_m5_rules_252: RED
    (
        false,
        &[
            Cond::Ge(22, 0.041798655327_f64),
            Cond::Ge(0, 6.734015313961_f64),
        ],
    ),
    // 253 eth_m5_rules_253: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Le(48, 81.594378791001_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 254 eth_m5_rules_254: RED
    (
        false,
        &[
            Cond::Ge(69, 98.632249177409_f64),
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 255 eth_m5_rules_255: GREEN
    (
        true,
        &[
            Cond::Le(68, 5.418823304347_f64),
            Cond::Ge(19, 1.731253361596_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 256 eth_m5_rules_256: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.126060052744_f64),
            Cond::Le(51, -0.975669526392_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 257 eth_m5_rules_257: RED
    (
        false,
        &[
            Cond::Ge(62, 87.21365662096_f64),
            Cond::In(80, &[6.0_f64]),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 258 eth_m5_rules_258: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(51, -1.397975506363_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 259 eth_m5_rules_259: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000673519805_f64),
            Cond::Le(78, -0.011317995852_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 260 eth_m5_rules_260: RED
    (
        false,
        &[
            Cond::Ge(16, 1.584018061175_f64),
            Cond::Le(25, -0.037571861183_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 261 eth_m5_rules_261: RED
    (
        false,
        &[
            Cond::Ge(12, 211.785069809589_f64),
            Cond::Ge(7, 1.0_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 262 eth_m5_rules_262: RED
    (
        false,
        &[
            Cond::Ge(62, 79.381448231167_f64),
            Cond::Le(78, -0.001855155415_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 263 eth_m5_rules_263: RED
    (
        false,
        &[
            Cond::Ge(62, 79.381448231167_f64),
            Cond::Le(78, -0.001855155415_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 264 eth_m5_rules_264: RED
    (
        false,
        &[
            Cond::Ge(17, 2.801397412222_f64),
            Cond::Le(79, -0.007352033255_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 265 eth_m5_rules_265: GREEN
    (
        true,
        &[
            Cond::Le(63, 22.349039721054_f64),
            Cond::Le(51, -1.243294430995_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 266 eth_m5_rules_266: RED
    (
        false,
        &[
            Cond::Ge(4, 0.944008017013_f64),
            Cond::Le(44, -0.000836071568_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 267 eth_m5_rules_267: RED
    (
        false,
        &[
            Cond::Ge(69, 90.414132730189_f64),
            Cond::Le(35, 0.0_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 268 eth_m5_rules_268: RED
    (
        false,
        &[
            Cond::Ge(69, 90.414132730189_f64),
            Cond::Le(68, 61.448545250073_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 269 eth_m5_rules_269: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.644587669_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(43, 0.01797752809_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 270 eth_m5_rules_270: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(12, -213.8206725_f64),
            Cond::Ge(70, 2.887028121_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 271 eth_m5_rules_271: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.346311048_f64),
            Cond::Le(42, 0.00002645309807_f64),
            Cond::Ge(8, -0.007253030737_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 272 eth_m5_rules_272: RED
    (
        false,
        &[
            Cond::Ge(68, 95.36043284_f64),
            Cond::Ge(44, 0.002366750995_f64),
            Cond::Ge(17, 2.046146229_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 273 eth_m5_rules_273: RED
    (
        false,
        &[
            Cond::Ge(68, 99.566819537935_f64),
            Cond::Le(51, -1.087983177347_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 274 eth_m5_rules_274: RED
    (
        false,
        &[
            Cond::Ge(60, 82.623793495996_f64),
            Cond::Between(2, 0.006117730378_f64, 0.007861395294_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 275 eth_m5_rules_275: RED
    (
        false,
        &[
            Cond::Ge(68, 92.58675389879_f64),
            Cond::Le(15, 0.306261093449_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 276 eth_m5_rules_276: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.586231489434_f64),
            Cond::Le(0, -7.107013931768_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 277 eth_m5_rules_277: RED
    (
        false,
        &[
            Cond::Ge(48, 88.271737232917_f64),
            Cond::Le(39, 0.039537406427_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 278 eth_m5_rules_278: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(61, 31.37459303_f64),
            Cond::Le(30, 0.0001481980796_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 279 eth_m5_rules_279: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Le(18, 1.450800747_f64),
            Cond::Ge(56, 0.004280476703_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 280 eth_m5_rules_280: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001549259691_f64),
            Cond::Ge(78, 0.003599409445_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 281 eth_m5_rules_281: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.782486369008_f64),
            Cond::Between(50, 0.780090159838_f64, 0.978325233895_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 282 eth_m5_rules_282: GREEN
    (
        true,
        &[
            Cond::Le(48, 13.48098558_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(3, 0.00646353357_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 283 eth_m5_rules_283: GREEN
    (
        true,
        &[
            Cond::Le(70, 6.986747793_f64),
            Cond::Le(7, 0.03305785124_f64),
            Cond::Ge(12, -89.88475125_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 284 eth_m5_rules_284: GREEN
    (
        true,
        &[
            Cond::Le(62, 22.509371455114_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 285 eth_m5_rules_285: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 286 eth_m5_rules_286: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.387301319167_f64),
            Cond::Ge(13, -43.945068697837_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 287 eth_m5_rules_287: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.921859236625_f64),
            Cond::Le(2, 0.001519053115_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 288 eth_m5_rules_288: RED
    (
        false,
        &[
            Cond::Ge(62, 81.929769320515_f64),
            Cond::Le(78, -0.000748194933_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 289 eth_m5_rules_289: GREEN
    (
        true,
        &[
            Cond::Le(63, 37.670808563448_f64),
            Cond::Ge(17, -0.382943541103_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 290 eth_m5_rules_290: RED
    (
        false,
        &[
            Cond::Ge(68, 94.73247534402_f64),
            Cond::Ge(1, 0.834297868579_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 291 eth_m5_rules_291: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 292 eth_m5_rules_292: RED
    (
        false,
        &[
            Cond::Ge(4, 1.242274122_f64),
            Cond::Eq(80, 3.0_f64),
            Cond::Le(77, 2.098463339_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 293 eth_m5_rules_293: GREEN
    (
        true,
        &[
            Cond::Le(63, 18.570891908685_f64),
            Cond::In(41, &[2.0_f64]),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 294 eth_m5_rules_294: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.878329763064_f64),
            Cond::Le(11, -0.141889659972_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 295 eth_m5_rules_295: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Ge(40, 0.588083928324_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 296 eth_m5_rules_296: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 13.0_f64),
            Cond::Ge(48, 25.84167143_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 297 eth_m5_rules_297: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(62, 70.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::In(41, &[11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64, 15.0_f64]),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 298 eth_m5_rules_298: RED
    (
        false,
        &[
            Cond::Ge(17, 3.082851148_f64),
            Cond::Eq(80, 3.0_f64),
            Cond::Le(47, 66.52204642_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 299 eth_m5_rules_299: GREEN
    (
        true,
        &[
            Cond::Le(62, 22.509371455114_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 300 eth_m5_rules_300: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.056247011082_f64),
            Cond::Ge(32, 1.0_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 301 eth_m5_rules_301: RED
    (
        false,
        &[
            Cond::Ge(40, 0.80253046606_f64),
            Cond::Ge(72, 15.903107859896_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 302 eth_m5_rules_302: GREEN
    (
        true,
        &[
            Cond::Le(51, -1.536658910705_f64),
            Cond::Le(75, 0.226931525872_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 303 eth_m5_rules_303: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.218185817798_f64),
            Cond::Le(1, -6.984363537212_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 304 eth_m5_rules_304: GREEN
    (
        true,
        &[
            Cond::Le(60, 19.168638833873_f64),
            Cond::In(80, &[6.0_f64]),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 305 eth_m5_rules_305: GREEN
    (
        true,
        &[
            Cond::Le(60, 19.168638833873_f64),
            Cond::In(80, &[6.0_f64]),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 306 eth_m5_rules_306: GREEN
    (
        true,
        &[
            Cond::Le(68, 1.371097431342_f64),
            Cond::Ge(79, 0.010852199486_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 307 eth_m5_rules_307: RED
    (
        false,
        &[
            Cond::Ge(63, 76.610575342271_f64),
            Cond::Le(73, 0.000024786183_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 308 eth_m5_rules_308: RED
    (
        false,
        &[
            Cond::Ge(63, 80.15965415611_f64),
            Cond::Le(51, -0.979734943474_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 309 eth_m5_rules_309: RED
    (
        false,
        &[
            Cond::Ge(69, 92.398919564528_f64),
            Cond::Le(40, 0.426292967876_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 310 eth_m5_rules_310: RED
    (
        false,
        &[
            Cond::Ge(63, 70.510977130054_f64),
            Cond::Le(17, 0.838389255232_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 311 eth_m5_rules_311: RED
    (
        false,
        &[
            Cond::Ge(60, 79.821411093328_f64),
            Cond::Ge(0, 4.657215509588_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 312 eth_m5_rules_312: RED
    (
        false,
        &[
            Cond::Ge(16, 1.722377357787_f64),
            Cond::Ge(1, 25.309101221933_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 313 eth_m5_rules_313: RED
    (
        false,
        &[
            Cond::Ge(63, 70.510977130054_f64),
            Cond::Le(1, -20.780423400716_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 314 eth_m5_rules_314: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(46, 27.45320025_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 315 eth_m5_rules_315: RED
    (
        false,
        &[
            Cond::Ge(68, 95.36043284_f64),
            Cond::Ge(8, 0.01911639022_f64),
            Cond::Ge(14, 301.1917591_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 316 eth_m5_rules_316: RED
    (
        false,
        &[
            Cond::Ge(68, 95.36043284_f64),
            Cond::Ge(8, 0.01911639022_f64),
            Cond::Ge(14, 301.1917591_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 317 eth_m5_rules_317: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001457840018_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Le(21, -0.0156322462_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 318 eth_m5_rules_318: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Le(26, -0.001004677022_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 319 eth_m5_rules_319: GREEN
    (
        true,
        &[
            Cond::Le(69, 5.812368993449_f64),
            Cond::Ge(20, -0.000598615933_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 320 eth_m5_rules_320: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.005348262374_f64),
            Cond::Ge(60, 69.060618393279_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 321 eth_m5_rules_321: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(51, 0.245066079634_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 322 eth_m5_rules_322: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.210641246093_f64),
            Cond::Ge(29, 0.01508702537_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 323 eth_m5_rules_323: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(25, -0.003833684053_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 324 eth_m5_rules_324: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.001086987122_f64),
            Cond::Ge(45, 0.001725415755_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 325 eth_m5_rules_325: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.000268702787_f64),
            Cond::Ge(19, 1.89989821024_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 326 eth_m5_rules_326: RED
    (
        false,
        &[
            Cond::Ge(12, 169.847668321743_f64),
            Cond::Ge(79, 0.021524632953_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 327 eth_m5_rules_327: RED
    (
        false,
        &[
            Cond::Ge(63, 73.931490170639_f64),
            Cond::Le(51, -1.61225812853_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 328 eth_m5_rules_328: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Le(1, -5.344432465957_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 329 eth_m5_rules_329: RED
    (
        false,
        &[
            Cond::Ge(17, 3.082851148_f64),
            Cond::Eq(80, 3.0_f64),
            Cond::Le(47, 66.52204642_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 330 eth_m5_rules_330: RED
    (
        false,
        &[
            Cond::Ge(63, 80.23066448_f64),
            Cond::Le(2, 0.000953452966_f64),
            Cond::Ge(47, 78.27879154_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 331 eth_m5_rules_331: GREEN
    (
        true,
        &[
            Cond::Le(68, 0.6862995766_f64),
            Cond::Le(17, -2.808854839_f64),
            Cond::Le(6, 0.003473998504_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 332 eth_m5_rules_332: RED
    (
        false,
        &[
            Cond::Ge(68, 97.75223967825_f64),
            Cond::Le(44, -0.000433596303_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 333 eth_m5_rules_333: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.126060052744_f64),
            Cond::Ge(63, 37.066174537088_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 334 eth_m5_rules_334: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 335 eth_m5_rules_335: GREEN
    (
        true,
        &[
            Cond::Le(62, 16.441111837773_f64),
            Cond::Ge(60, 32.751338756074_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 336 eth_m5_rules_336: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.160811012751_f64),
            Cond::Le(1, -0.75450257826_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 337 eth_m5_rules_337: GREEN
    (
        true,
        &[
            Cond::Le(63, 27.937723759111_f64),
            Cond::Le(11, -0.60399701237_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 338 eth_m5_rules_338: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::Le(39, 0.31888152245_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 339 eth_m5_rules_339: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::In(41, &[9.0_f64]),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 340 eth_m5_rules_340: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(76, -1.457487435051_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 341 eth_m5_rules_341: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.187038250278_f64),
            Cond::Between(50, 0.754270965462_f64, 0.952041479441_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 342 eth_m5_rules_342: GREEN
    (
        true,
        &[
            Cond::Le(36, 1.0_f64),
            Cond::Le(76, -1.457487435051_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 343 eth_m5_rules_343: GREEN
    (
        true,
        &[
            Cond::Le(69, 13.599250472473_f64),
            Cond::Le(1, -21.606633337189_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 344 eth_m5_rules_344: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(72, 50.208999999976_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 345 eth_m5_rules_345: RED
    (
        false,
        &[
            Cond::Ge(69, 92.499098560538_f64),
            Cond::Le(46, 39.31642972534_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 346 eth_m5_rules_346: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 347 eth_m5_rules_347: RED
    (
        false,
        &[
            Cond::Ge(62, 84.181207583347_f64),
            Cond::Le(15, 0.17007379085_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 348 eth_m5_rules_348: GREEN
    (
        true,
        &[
            Cond::Le(4, 0.048300974027_f64),
            Cond::Ge(27, 0.038486437398_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 349 eth_m5_rules_349: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Le(76, -0.992604162908_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 350 eth_m5_rules_350: RED
    (
        false,
        &[
            Cond::Ge(16, 2.120054690474_f64),
            Cond::Le(46, 30.058809486348_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 351 eth_m5_rules_351: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Le(51, 1.402973124746_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 352 eth_m5_rules_352: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(73, 0.007814538794_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 353 eth_m5_rules_353: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.289885513984_f64),
            Cond::Ge(34, 6.0_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 354 eth_m5_rules_354: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.65908861946_f64),
            Cond::Le(19, 0.607016051968_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 355 eth_m5_rules_355: GREEN
    (
        true,
        &[
            Cond::Le(69, 9.590121893836_f64),
            Cond::Le(11, -0.887448173015_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 356 eth_m5_rules_356: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(6, 0.000012687105_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 357 eth_m5_rules_357: RED
    (
        false,
        &[
            Cond::Ge(69, 94.958449012797_f64),
            Cond::Le(15, 0.396308918755_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 358 eth_m5_rules_358: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.596399053377_f64),
            Cond::Le(51, -0.052398857241_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 359 eth_m5_rules_359: RED
    (
        false,
        &[
            Cond::Ge(12, 211.785069809589_f64),
            Cond::Ge(7, 1.0_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 360 eth_m5_rules_360: RED
    (
        false,
        &[
            Cond::Ge(63, 70.644314127054_f64),
            Cond::Le(13, 24.856599717988_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 361 eth_m5_rules_361: GREEN
    (
        true,
        &[
            Cond::Le(69, 4.960424772543_f64),
            Cond::Ge(1, 23.119074919152_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 362 eth_m5_rules_362: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.563149366712_f64),
            Cond::Ge(78, 0.001902549548_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 363 eth_m5_rules_363: RED
    (
        false,
        &[
            Cond::Ge(62, 81.929769320515_f64),
            Cond::Le(11, -0.885183402308_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 364 eth_m5_rules_364: GREEN
    (
        true,
        &[
            Cond::Le(63, 19.860943343755_f64),
            Cond::Le(7, 0.004232647518_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 365 eth_m5_rules_365: GREEN
    (
        true,
        &[
            Cond::Le(68, 0.5066458518_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Ge(42, 9.29093662300000e-8_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 366 eth_m5_rules_366: RED
    (
        false,
        &[
            Cond::Ge(68, 95.36043284_f64),
            Cond::Ge(56, 0.01939351557_f64),
            Cond::Eq(80, 4.0_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 367 eth_m5_rules_367: RED
    (
        false,
        &[
            Cond::Ge(62, 79.187266351359_f64),
            Cond::Ge(73, 0.007704921362_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 368 eth_m5_rules_368: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 369 eth_m5_rules_369: GREEN
    (
        true,
        &[
            Cond::Le(12, -168.9532813_f64),
            Cond::Le(43, 0.01349188119_f64),
            Cond::Ge(21, -0.005575910157_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 370 eth_m5_rules_370: RED
    (
        false,
        &[
            Cond::Ge(25, -0.000091348551_f64),
            Cond::Ge(74, 0.007973138013_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 371 eth_m5_rules_371: GREEN
    (
        true,
        &[
            Cond::Le(68, 0.5066458518_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Ge(42, 9.29093662300000e-8_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 372 eth_m5_rules_372: RED
    (
        false,
        &[
            Cond::Ge(69, 92.398919564528_f64),
            Cond::Le(67, -1.54535539948_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 373 eth_m5_rules_373: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000544949204_f64),
            Cond::Ge(71, 0.004468097235_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 374 eth_m5_rules_374: GREEN
    (
        true,
        &[
            Cond::Le(63, 20.878329763064_f64),
            Cond::Le(11, -0.141889659972_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 375 eth_m5_rules_375: GREEN
    (
        true,
        &[
            Cond::Le(69, 13.103119286977_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 376 eth_m5_rules_376: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.013135821846_f64),
            Cond::Ge(19, 2.080444997831_f64),
            Cond::Eq(41, 14.0_f64),
        ],
    ),
    // 377 eth_m5_rules_377: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(62, 75.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 378 eth_m5_rules_378: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(62, 75.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 379 eth_m5_rules_379: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 1.5_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 21.0_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 380 eth_m5_rules_380: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000940217931_f64),
            Cond::Ge(19, 2.222113870768_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 381 eth_m5_rules_381: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Le(15, 0.000045742666_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 382 eth_m5_rules_382: RED
    (
        false,
        &[
            Cond::Ge(62, 72.019291926381_f64),
            Cond::Le(13, 24.33220298786_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 383 eth_m5_rules_383: RED
    (
        false,
        &[
            Cond::Ge(4, 1.202805912201_f64),
            Cond::Le(78, -0.000993286689_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 384 eth_m5_rules_384: GREEN
    (
        true,
        &[
            Cond::Le(12, -163.848507305154_f64),
            Cond::Ge(73, 0.007704921362_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 385 eth_m5_rules_385: RED
    (
        false,
        &[
            Cond::Ge(12, 210.227488831543_f64),
            Cond::Le(23, -0.004814800565_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 386 eth_m5_rules_386: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.005733286026_f64),
            Cond::Ge(6, 0.01649142991_f64),
            Cond::Le(45, -0.006858402107_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 387 eth_m5_rules_387: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.336010955575_f64),
            Cond::Le(0, -22.999073893274_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 388 eth_m5_rules_388: RED
    (
        false,
        &[
            Cond::Le(53, 0.0_f64),
            Cond::Le(11, -0.422866719649_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 389 eth_m5_rules_389: GREEN
    (
        true,
        &[
            Cond::Le(69, 3.695232436063_f64),
            Cond::Ge(1, 3.927244001484_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 390 eth_m5_rules_390: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000053521742_f64),
            Cond::Ge(72, 0.056497175141_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 391 eth_m5_rules_391: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(51, -1.315533522184_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 392 eth_m5_rules_392: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000813794422_f64),
            Cond::Le(15, 0.0_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 393 eth_m5_rules_393: GREEN
    (
        true,
        &[
            Cond::Le(48, 17.929017212354_f64),
            Cond::Ge(46, 69.85818583021_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 394 eth_m5_rules_394: RED
    (
        false,
        &[
            Cond::Ge(62, 79.381448231167_f64),
            Cond::Ge(49, 1430.0_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 395 eth_m5_rules_395: RED
    (
        false,
        &[
            Cond::Ge(63, 70.644314127054_f64),
            Cond::Le(13, 24.856599717988_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 396 eth_m5_rules_396: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(78, -0.011503037214_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 397 eth_m5_rules_397: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.921859236625_f64),
            Cond::Le(2, 0.001519053115_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 398 eth_m5_rules_398: GREEN
    (
        true,
        &[
            Cond::Le(48, 4.701970326377_f64),
            Cond::Ge(73, 0.005051760632_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 399 eth_m5_rules_399: GREEN
    (
        true,
        &[
            Cond::Le(69, 1.644587669_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(43, 0.01797752809_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 400 eth_m5_rules_400: GREEN
    (
        true,
        &[
            Cond::Le(48, 0.0_f64),
            Cond::Ge(33, 9.0_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 401 eth_m5_rules_401: RED
    (
        false,
        &[
            Cond::Ge(16, 1.584018061175_f64),
            Cond::Le(25, -0.037571861183_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 402 eth_m5_rules_402: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Between(7, 0.348371370028_f64, 0.50546791832_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 403 eth_m5_rules_403: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.487372513_f64),
            Cond::Eq(41, 11.0_f64),
            Cond::Ge(10, -0.004965357046_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 404 eth_m5_rules_404: GREEN
    (
        true,
        &[
            Cond::Le(69, 7.366825932134_f64),
            Cond::Ge(48, 56.429411536562_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 405 eth_m5_rules_405: RED
    (
        false,
        &[
            Cond::Ge(4, 1.17405034696_f64),
            Cond::Ge(74, 0.007974367829_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 406 eth_m5_rules_406: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.583474519884_f64),
            Cond::Le(45, -0.005856406184_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 407 eth_m5_rules_407: RED
    (
        false,
        &[Cond::Ge(37, 4.0_f64), Cond::Le(15, 0.029402232243_f64)],
    ),
    // 408 eth_m5_rules_408: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.270218913246_f64),
            Cond::Le(51, 1.402973124746_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 409 eth_m5_rules_409: RED
    (
        false,
        &[
            Cond::Ge(24, -0.002077937651_f64),
            Cond::Ge(23, 0.069654406809_f64),
        ],
    ),
    // 410 eth_m5_rules_410: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.001091384106_f64),
            Cond::Le(10, -0.01817603669_f64),
            Cond::Ge(77, 2.919388313_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 411 eth_m5_rules_411: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(51, -1.12961546566_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 412 eth_m5_rules_412: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.110392815456_f64),
            Cond::Ge(1, 5.602555707271_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 413 eth_m5_rules_413: RED
    (
        false,
        &[
            Cond::Ge(16, 1.694065028932_f64),
            Cond::Le(24, -0.014727084111_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 414 eth_m5_rules_414: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.2340435963_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(28, 0.001091384768_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 415 eth_m5_rules_415: GREEN
    (
        true,
        &[
            Cond::Ge(54, 3.0_f64),
            Cond::Le(62, 30.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 416 eth_m5_rules_416: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Ge(51, 2.14921727869_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 417 eth_m5_rules_417: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.110392815456_f64),
            Cond::Ge(1, 5.602555707271_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 418 eth_m5_rules_418: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.547097817_f64),
            Cond::Le(2, 0.001795740443_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 419 eth_m5_rules_419: GREEN
    (
        true,
        &[
            Cond::Le(13, -296.373970792508_f64),
            Cond::Ge(38, -0.003927732991_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 420 eth_m5_rules_420: GREEN
    (
        true,
        &[
            Cond::Le(18, -2.447691672_f64),
            Cond::Le(3, 0.0006406963614_f64),
            Cond::Le(8, -0.004124824725_f64),
        ],
    ),
    // 421 eth_m5_rules_421: GREEN
    (
        true,
        &[
            Cond::Le(63, 23.24758078_f64),
            Cond::Ge(28, 0.03033879208_f64),
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 422 eth_m5_rules_422: RED
    (
        false,
        &[
            Cond::Ge(62, 79.187266351359_f64),
            Cond::Ge(73, 0.007704921362_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 423 eth_m5_rules_423: GREEN
    (
        true,
        &[
            Cond::Le(69, 3.994763574046_f64),
            Cond::Ge(78, 0.002288868958_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 424 eth_m5_rules_424: RED
    (
        false,
        &[
            Cond::Ge(62, 78.115236912005_f64),
            Cond::Le(13, 47.198310116171_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 425 eth_m5_rules_425: GREEN
    (
        true,
        &[
            Cond::Le(62, 17.596607453968_f64),
            Cond::Le(19, 0.607016051968_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 426 eth_m5_rules_426: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.353463333651_f64),
            Cond::Between(51, -0.464821755383_f64, -0.073200228374_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 427 eth_m5_rules_427: GREEN
    (
        true,
        &[
            Cond::Le(17, -3.275620005277_f64),
            Cond::Between(7, 0.348371370028_f64, 0.50546791832_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 428 eth_m5_rules_428: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.000186502931_f64),
            Cond::Ge(73, 0.012698614211_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 429 eth_m5_rules_429: RED
    (
        false,
        &[
            Cond::Ge(69, 83.360348764515_f64),
            Cond::Le(76, -1.509021822217_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 430 eth_m5_rules_430: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.008267982551_f64),
            Cond::Ge(25, -0.010784453702_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 431 eth_m5_rules_431: RED
    (
        false,
        &[
            Cond::Ge(62, 78.148726107272_f64),
            Cond::Le(46, 44.954532800507_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 432 eth_m5_rules_432: RED
    (
        false,
        &[
            Cond::Ge(68, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 433 eth_m5_rules_433: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Ge(62, 75.0_f64),
            Cond::Ge(50, 1.0_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 1.0_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 434 eth_m5_rules_434: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Ge(62, 70.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 435 eth_m5_rules_435: RED
    (
        false,
        &[
            Cond::Ge(44, 0.001865948161_f64),
            Cond::Ge(68, 98.87542722_f64),
            Cond::Le(59, 0.03681466689_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 436 eth_m5_rules_436: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.012972255848_f64),
            Cond::Ge(25, -0.00215355613_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 437 eth_m5_rules_437: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000317708634_f64),
            Cond::Le(15, 0.124796612801_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 438 eth_m5_rules_438: RED
    (
        false,
        &[
            Cond::Ge(68, 96.35554424836_f64),
            Cond::Le(44, -0.000547643196_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 439 eth_m5_rules_439: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.267047060819_f64),
            Cond::Le(19, 0.656844883173_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 440 eth_m5_rules_440: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(74, 0.011732866886_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 441 eth_m5_rules_441: RED
    (
        false,
        &[
            Cond::Ge(16, 2.218106766884_f64),
            Cond::Le(78, -0.003994395698_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 442 eth_m5_rules_442: GREEN
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
    // 443 eth_m5_rules_443: RED
    (
        false,
        &[
            Cond::Ge(16, 2.082088491926_f64),
            Cond::Le(45, -0.004456144174_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 444 eth_m5_rules_444: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.092705762683_f64),
            Cond::Ge(45, 0.004216605253_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 445 eth_m5_rules_445: GREEN
    (
        true,
        &[
            Cond::Le(30, 0.0005704541149_f64),
            Cond::Le(4, -0.24090522_f64),
            Cond::Ge(18, -2.696757558_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 446 eth_m5_rules_446: GREEN
    (
        true,
        &[
            Cond::Le(12, -243.4867158_f64),
            Cond::Le(3, 0.001411663091_f64),
            Cond::Ge(6, 0.003473998504_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 447 eth_m5_rules_447: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.344526637064_f64),
            Cond::Ge(46, 60.696816665125_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 448 eth_m5_rules_448: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(76, -1.341363982825_f64),
            Cond::Eq(41, 17.0_f64),
        ],
    ),
    // 449 eth_m5_rules_449: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.561260986784_f64),
            Cond::Ge(4, 0.302583639175_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 450 eth_m5_rules_450: GREEN
    (
        true,
        &[
            Cond::Le(36, 1.0_f64),
            Cond::Le(76, -1.457487435051_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 451 eth_m5_rules_451: GREEN
    (
        true,
        &[
            Cond::Le(29, 0.00254012653_f64),
            Cond::Ge(42, 0.002459555014_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 452 eth_m5_rules_452: RED
    (
        false,
        &[
            Cond::Ge(68, 89.433032932232_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(41, 3.0_f64),
        ],
    ),
    // 453 eth_m5_rules_453: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000258173049_f64),
            Cond::Le(51, -1.315533522184_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 454 eth_m5_rules_454: RED
    (
        false,
        &[
            Cond::Ge(69, 96.167908771068_f64),
            Cond::Ge(49, 1430.0_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 455 eth_m5_rules_455: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(78, -0.011503037214_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 456 eth_m5_rules_456: GREEN
    (
        true,
        &[
            Cond::Le(63, 19.860943343755_f64),
            Cond::Le(7, 0.004232647518_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 457 eth_m5_rules_457: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.824148755859_f64),
            Cond::Ge(74, 0.015517215474_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 458 eth_m5_rules_458: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.116199896442_f64),
            Cond::Ge(29, 0.017910613907_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 459 eth_m5_rules_459: GREEN
    (
        true,
        &[
            Cond::Le(62, 13.443623461596_f64),
            Cond::Le(19, 0.712660732397_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 460 eth_m5_rules_460: RED
    (
        false,
        &[
            Cond::Ge(48, 92.538838851052_f64),
            Cond::Ge(43, 16.649945784544_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 461 eth_m5_rules_461: GREEN
    (
        true,
        &[
            Cond::Le(62, 16.815653800848_f64),
            Cond::Le(51, -0.781972702142_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 462 eth_m5_rules_462: RED
    (
        false,
        &[
            Cond::Ge(63, 88.325740640992_f64),
            Cond::Le(13, 127.802445543786_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 463 eth_m5_rules_463: GREEN
    (
        true,
        &[
            Cond::Le(12, -264.440276366037_f64),
            Cond::Between(46, 44.954532800507_f64, 54.831328521965_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 464 eth_m5_rules_464: GREEN
    (
        true,
        &[
            Cond::Le(60, 24.975510474295_f64),
            Cond::Ge(46, 39.788932118881_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 465 eth_m5_rules_465: RED
    (
        false,
        &[
            Cond::Ge(27, 0.089607310064_f64),
            Cond::Le(2, 0.013586365309_f64),
        ],
    ),
    // 466 eth_m5_rules_466: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.160650711145_f64),
            Cond::Ge(0, 4.657215509588_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 467 eth_m5_rules_467: GREEN
    (
        true,
        &[
            Cond::Le(51, -1.536658910705_f64),
            Cond::Le(22, -0.004457586342_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 468 eth_m5_rules_468: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.353463333651_f64),
            Cond::Between(51, -0.464821755383_f64, -0.073200228374_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 469 eth_m5_rules_469: GREEN
    (
        true,
        &[
            Cond::Le(68, 15.251559689203_f64),
            Cond::Ge(72, 102.022412698827_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 470 eth_m5_rules_470: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(60, 44.439233481289_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
    // 471 eth_m5_rules_471: GREEN
    (
        true,
        &[
            Cond::Le(69, 15.180011010737_f64),
            Cond::Ge(72, 102.022412698827_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 472 eth_m5_rules_472: GREEN
    (
        true,
        &[
            Cond::Le(36, 0.0_f64),
            Cond::Ge(16, -0.73657496892_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 473 eth_m5_rules_473: RED
    (
        false,
        &[
            Cond::Ge(37, 4.0_f64),
            Cond::Le(76, -1.385238691491_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 474 eth_m5_rules_474: RED
    (
        false,
        &[
            Cond::Ge(48, 92.538838851052_f64),
            Cond::Le(40, 0.378420082698_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 475 eth_m5_rules_475: GREEN
    (
        true,
        &[
            Cond::Le(55, -0.008267982551_f64),
            Cond::Ge(19, 2.255084993412_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 476 eth_m5_rules_476: RED
    (
        false,
        &[
            Cond::Ge(69, 89.373499384241_f64),
            Cond::Le(58, -0.002741635191_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 477 eth_m5_rules_477: RED
    (
        false,
        &[
            Cond::Ge(16, 1.962352079221_f64),
            Cond::Le(6, 0.000391630717_f64),
            Cond::Eq(41, 1.0_f64),
        ],
    ),
    // 478 eth_m5_rules_478: GREEN
    (
        true,
        &[
            Cond::Le(38, -0.001555592433_f64),
            Cond::Ge(60, 74.332784822998_f64),
            Cond::In(
                41,
                &[
                    7.0_f64, 8.0_f64, 9.0_f64, 10.0_f64, 11.0_f64, 12.0_f64, 13.0_f64, 14.0_f64,
                ],
            ),
        ],
    ),
    // 479 eth_m5_rules_479: RED
    (
        false,
        &[
            Cond::Ge(68, 83.042281112572_f64),
            Cond::Ge(33, 11.0_f64),
            Cond::Eq(41, 15.0_f64),
        ],
    ),
    // 480 eth_m5_rules_480: RED
    (
        false,
        &[
            Cond::Ge(25, -0.003202986598_f64),
            Cond::Le(44, -0.000710524492_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 481 eth_m5_rules_481: RED
    (
        false,
        &[
            Cond::Ge(68, 98.87542775_f64),
            Cond::Ge(56, 0.02486548978_f64),
            Cond::Le(42, 0.001638796436_f64),
            Cond::Eq(41, 20.0_f64),
        ],
    ),
    // 482 eth_m5_rules_482: GREEN
    (
        true,
        &[
            Cond::Le(69, 2.898892702_f64),
            Cond::Le(24, -0.02994954907_f64),
            Cond::Le(48, 14.51065799_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 483 eth_m5_rules_483: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.774117242_f64),
            Cond::Ge(21, -0.004431553752_f64),
            Cond::Le(18, -3.385888687_f64),
            Cond::Eq(80, 4.0_f64),
        ],
    ),
    // 484 eth_m5_rules_484: RED
    (
        false,
        &[
            Cond::Ge(10, 0.008390907843_f64),
            Cond::Ge(24, -0.0003773777571_f64),
            Cond::Le(42, 0.0_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 485 eth_m5_rules_485: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.304024546_f64),
            Cond::Eq(41, 22.0_f64),
            Cond::Le(71, 9.22394520900000e-8_f64),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 486 eth_m5_rules_486: GREEN
    (
        true,
        &[
            Cond::Le(61, 33.28245704_f64),
            Cond::Ge(57, -0.005787322997_f64),
            Cond::Le(4, -0.173645982_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 487 eth_m5_rules_487: RED
    (
        false,
        &[
            Cond::Ge(63, 79.78754453_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Le(61, 64.95549342_f64),
            Cond::Eq(41, 9.0_f64),
        ],
    ),
    // 488 eth_m5_rules_488: GREEN
    (
        true,
        &[
            Cond::Le(44, -0.002650404386_f64),
            Cond::Le(68, 5.152344313_f64),
            Cond::Le(69, 3.478803314_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 489 eth_m5_rules_489: GREEN
    (
        true,
        &[
            Cond::Le(17, -2.354298009924_f64),
            Cond::Le(19, 0.457331193089_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 490 eth_m5_rules_490: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.210641246093_f64),
            Cond::Ge(23, 0.007795884974_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 491 eth_m5_rules_491: GREEN
    (
        true,
        &[
            Cond::Le(35, 0.0_f64),
            Cond::Ge(20, 0.010119255562_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 492 eth_m5_rules_492: GREEN
    (
        true,
        &[
            Cond::Le(58, -0.007057262978_f64),
            Cond::Ge(13, 151.60195878472_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 493 eth_m5_rules_493: RED
    (
        false,
        &[
            Cond::Ge(37, 5.0_f64),
            Cond::Le(46, 25.454349113993_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 494 eth_m5_rules_494: GREEN
    (
        true,
        &[
            Cond::Le(4, -0.124295441041_f64),
            Cond::Between(51, -0.464230165361_f64, -0.047403807875_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 495 eth_m5_rules_495: GREEN
    (
        true,
        &[
            Cond::Le(69, 4.056578989833_f64),
            Cond::Ge(34, 6.0_f64),
            Cond::Eq(41, 21.0_f64),
        ],
    ),
    // 496 eth_m5_rules_496: RED
    (
        false,
        &[
            Cond::Ge(24, -0.000201199042_f64),
            Cond::Le(51, -1.563227737634_f64),
            Cond::Eq(41, 7.0_f64),
        ],
    ),
    // 497 eth_m5_rules_497: GREEN
    (
        true,
        &[
            Cond::Le(12, -249.930452912772_f64),
            Cond::Between(48, 37.154692990356_f64, 62.524777166172_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 498 eth_m5_rules_498: GREEN
    (
        true,
        &[
            Cond::Le(27, 0.000120857876_f64),
            Cond::Ge(1, 2.301340395721_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 499 eth_m5_rules_499: GREEN
    (
        true,
        &[
            Cond::Le(16, -2.220922553819_f64),
            Cond::Ge(44, 0.000404314765_f64),
            Cond::In(41, &[20.0_f64, 21.0_f64, 22.0_f64, 23.0_f64]),
        ],
    ),
    // 500 eth_m5_rules_500: RED
    (
        false,
        &[
            Cond::Ge(37, 6.0_f64),
            Cond::Le(48, 36.682496680971_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 501 eth_m5_rules_501: RED
    (
        false,
        &[
            Cond::Ge(16, 2.082088491926_f64),
            Cond::Le(45, -0.004456144174_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 502 eth_m5_rules_502: GREEN
    (
        true,
        &[
            Cond::Ge(54, 5.0_f64),
            Cond::Le(62, 25.0_f64),
            Cond::Ge(50, 1.0_f64),
            Cond::Ge(7, 0.6_f64),
            Cond::Eq(80, 2.0_f64),
            Cond::Eq(41, 22.0_f64),
        ],
    ),
    // 503 eth_m5_rules_503: GREEN
    (
        true,
        &[
            Cond::Ge(54, 4.0_f64),
            Cond::Le(62, 25.0_f64),
            Cond::Ge(50, 0.8_f64),
            Cond::Ge(7, 0.75_f64),
            Cond::Eq(41, 13.0_f64),
            Cond::Eq(80, 2.0_f64),
        ],
    ),
    // 504 eth_m5_rules_504: RED
    (
        false,
        &[
            Cond::Ge(4, 1.242274122_f64),
            Cond::Eq(80, 3.0_f64),
            Cond::Le(77, 2.098463339_f64),
            Cond::Eq(41, 18.0_f64),
        ],
    ),
    // 505 eth_m5_rules_505: RED
    (
        false,
        &[
            Cond::Ge(63, 80.23066448_f64),
            Cond::Le(2, 0.000953452966_f64),
            Cond::Ge(47, 78.27879154_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 506 eth_m5_rules_506: GREEN
    (
        true,
        &[
            Cond::Le(68, 0.6862995766_f64),
            Cond::Le(17, -2.808854839_f64),
            Cond::Le(6, 0.003473998504_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 507 eth_m5_rules_507: GREEN
    (
        true,
        &[
            Cond::Le(28, 0.005548156292_f64),
            Cond::Le(8, -0.03657074613_f64),
            Cond::Le(46, 21.44107346_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 508 eth_m5_rules_508: RED
    (
        false,
        &[
            Cond::Ge(46, 90.605260301048_f64),
            Cond::Le(7, 0.045506715202_f64),
            Cond::Eq(41, 8.0_f64),
        ],
    ),
    // 509 eth_m5_rules_509: RED
    (
        false,
        &[
            Cond::Ge(62, 67.899626426361_f64),
            Cond::Le(46, 33.487434256627_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 510 eth_m5_rules_510: GREEN
    (
        true,
        &[
            Cond::Le(40, 0.336010955575_f64),
            Cond::Le(0, -22.999073893274_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 511 eth_m5_rules_511: GREEN
    (
        true,
        &[
            Cond::Le(63, 12.039352357177_f64),
            Cond::In(41, &[3.0_f64]),
            Cond::Eq(80, 1.0_f64),
        ],
    ),
    // 512 eth_m5_rules_512: GREEN
    (
        true,
        &[
            Cond::Le(68, 25.103448275861_f64),
            Cond::Ge(46, 79.69050284032_f64),
            Cond::Eq(41, 2.0_f64),
        ],
    ),
    // 513 eth_m5_rules_513: RED
    (
        false,
        &[
            Cond::Ge(48, 92.869319882182_f64),
            Cond::Le(40, 0.407243075194_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 514 eth_m5_rules_514: RED
    (
        false,
        &[
            Cond::Ge(36, 5.0_f64),
            Cond::Le(1, -39.147158960848_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 515 eth_m5_rules_515: GREEN
    (
        true,
        &[
            Cond::Le(17, -1.840110258471_f64),
            Cond::Le(1, -10.991762317629_f64),
            Cond::Eq(65, 1.0_f64),
        ],
    ),
    // 516 eth_m5_rules_516: GREEN
    (
        true,
        &[
            Cond::Le(60, 18.578805575288_f64),
            Cond::In(80, &[2.0_f64]),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 517 eth_m5_rules_517: GREEN
    (
        true,
        &[
            Cond::Le(5, -0.005185093586_f64),
            Cond::Ge(12, 197.29361706083_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 518 eth_m5_rules_518: GREEN
    (
        true,
        &[
            Cond::Le(69, 13.599250472473_f64),
            Cond::Le(1, -21.606633337189_f64),
            Cond::Eq(41, 12.0_f64),
        ],
    ),
    // 519 eth_m5_rules_519: RED
    (
        false,
        &[
            Cond::Ge(69, 83.706222662312_f64),
            Cond::Le(67, -2.782739943152_f64),
            Cond::Eq(41, 4.0_f64),
        ],
    ),
    // 520 eth_m5_rules_520: GREEN
    (
        true,
        &[
            Cond::Le(15, 0.120614853726_f64),
            Cond::Ge(8, 0.070114758373_f64),
        ],
    ),
    // 521 eth_m5_rules_521: RED
    (
        false,
        &[
            Cond::Ge(22, 0.037047849353_f64),
            Cond::In(41, &[10.0_f64]),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 522 eth_m5_rules_522: RED
    (
        false,
        &[
            Cond::Ge(62, 87.21365662096_f64),
            Cond::In(80, &[6.0_f64]),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 523 eth_m5_rules_523: RED
    (
        false,
        &[
            Cond::Ge(48, 90.971006267369_f64),
            Cond::Le(40, 0.365308166414_f64),
            Cond::Eq(41, 6.0_f64),
        ],
    ),
    // 524 eth_m5_rules_524: GREEN
    (
        true,
        &[
            Cond::Ge(76, 4.082365664208_f64),
            Cond::Between(46, 44.701987979408_f64, 55.226633333302_f64),
            Cond::Eq(41, 13.0_f64),
        ],
    ),
    // 525 eth_m5_rules_525: RED
    (
        false,
        &[
            Cond::Ge(15, 0.981752292899_f64),
            Cond::Ge(19, 1.995743208991_f64),
            Cond::Eq(41, 5.0_f64),
        ],
    ),
    // 526 eth_m5_rules_526: RED
    (
        false,
        &[
            Cond::Ge(69, 89.551972381464_f64),
            Cond::Le(67, -1.939490804131_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 527 eth_m5_rules_527: RED
    (
        false,
        &[
            Cond::Ge(62, 66.597829192963_f64),
            Cond::Le(46, 30.058809486348_f64),
            Cond::Eq(41, 23.0_f64),
        ],
    ),
    // 528 eth_m5_rules_528: GREEN
    (
        true,
        &[
            Cond::Le(16, -1.595966140347_f64),
            Cond::Le(51, -1.397975506363_f64),
            Cond::Eq(41, 11.0_f64),
        ],
    ),
    // 529 eth_m5_rules_529: GREEN
    (
        true,
        &[
            Cond::Le(8, -0.050353430971_f64),
            Cond::Ge(63, 38.043238023799_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 530 eth_m5_rules_530: RED
    (
        false,
        &[
            Cond::Ge(69, 94.958449012797_f64),
            Cond::Le(15, 0.396308918755_f64),
            Cond::Eq(41, 19.0_f64),
        ],
    ),
    // 531 eth_m5_rules_531: RED
    (
        false,
        &[
            Cond::Ge(17, 1.798879744053_f64),
            Cond::Le(38, -0.000282718339_f64),
            Cond::Eq(80, 5.0_f64),
        ],
    ),
    // 532 eth_m5_rules_532: RED
    (
        false,
        &[
            Cond::Ge(63, 77.591037873398_f64),
            Cond::Le(78, -0.001367816057_f64),
            Cond::Eq(80, 0.0_f64),
        ],
    ),
    // 533 eth_m5_rules_533: RED
    (
        false,
        &[
            Cond::Ge(4, 1.107991027781_f64),
            Cond::Ge(49, 1425.0_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 534 eth_m5_rules_534: RED
    (
        false,
        &[
            Cond::Ge(69, 93.669724770642_f64),
            Cond::Le(24, -0.004226946089_f64),
            Cond::Eq(41, 16.0_f64),
        ],
    ),
    // 535 eth_m5_rules_535: GREEN
    (
        true,
        &[
            Cond::Le(48, 7.263095943275_f64),
            Cond::Le(52, 0.0_f64),
            Cond::Eq(66, 1.0_f64),
        ],
    ),
    // 536 eth_m5_rules_536: GREEN
    (
        true,
        &[
            Cond::Le(13, -176.966019376678_f64),
            Cond::Le(1, -8.245498861958_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 537 eth_m5_rules_537: GREEN
    (
        true,
        &[
            Cond::Le(48, 13.48098558_f64),
            Cond::Ge(6, 0.01206610733_f64),
            Cond::Le(3, 0.00646353357_f64),
            Cond::Eq(80, 3.0_f64),
        ],
    ),
    // 538 eth_m5_rules_538: GREEN
    (
        true,
        &[
            Cond::Ge(54, 6.0_f64),
            Cond::Ge(1, 6.159740467929_f64),
            Cond::Eq(80, 6.0_f64),
        ],
    ),
    // 539 eth_m5_rules_539: RED
    (
        false,
        &[
            Cond::Ge(68, 97.251019949653_f64),
            Cond::Ge(71, 0.001007899334_f64),
            Cond::In(
                41,
                &[
                    0.0_f64, 1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64, 5.0_f64, 6.0_f64, 7.0_f64,
                ],
            ),
        ],
    ),
    // 540 eth_m5_rules_540: GREEN
    (
        true,
        &[
            Cond::Le(63, 21.766483332328_f64),
            Cond::Le(2, 0.00127885769_f64),
            Cond::Eq(41, 10.0_f64),
        ],
    ),
    // 541 eth_m5_rules_541: RED
    (
        false,
        &[
            Cond::Ge(62, 78.148726107272_f64),
            Cond::Le(46, 44.954532800507_f64),
            Cond::Eq(64, 1.0_f64),
        ],
    ),
    // 542 eth_m5_rules_542: RED
    (
        false,
        &[
            Cond::Ge(17, 2.783849801_f64),
            Cond::Eq(80, 5.0_f64),
            Cond::Le(26, -0.001004677022_f64),
            Cond::Eq(41, 0.0_f64),
        ],
    ),
];

pub struct EthRules542 {
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

impl EthRules542 {
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

impl Strategy for EthRules542 {
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
