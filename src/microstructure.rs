use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use crate::binance::Candle;

const EPS: f64 = 1e-12;
const COIN_M_API: &str = "https://dapi.binance.com";
const USD_M_API: &str = "https://fapi.binance.com";

/// Features referenced by the frozen mixed_13 strategy.
///
/// Values are stored after a float32 round-trip because the reference forward
/// pipeline converts every decision feature to float32 before applying rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    FutBtcusdtM1CloseLocation,
    FutBtcusdtM1GreenRatio,
    FutBtcusdtM1MinuteReturnLag2,
    FutBtcusdtM1MinuteReturnLag3,
    FutBtcusdtM1MinuteTakerLag2,
    FutBtcusdtM1SegmentReturn2,
    FutBtcusdtM1SegmentReturnAcceleration,
    FutBtcusdtM1SegmentTakerImbalance2,
    FutEthusdtM1BlockReturn,
    FutEthusdtM1CloseLocation,
    FutEthusdtM1MinuteReturnLag1,
    FutEthusdtM1MinuteReturnLag3,
    FutEthusdtM1SegmentReturn2,
    FutEthusdtM1SegmentReturnAcceleration,
    FutEthusdtM1SegmentTakerAcceleration,
    FutEthusdtM1SegmentTakerImbalance2,
    IndexEthusdtReturn1,
    MarkBtcusdtCloseLocation,
    MarkEthusdtReturn1,
    MarkEthusdtReturn3,
    OiEthusdtValueChange12,
    OiEthusdtValueChange6,
    SignalBreakoutHigh20,
    SignalCloseEma8Atr,
    SignalEma8Ema21Atr,
    SignalGreenRatio12,
    SignalGreenRatio3,
    SignalGreenRatio6,
    SignalReturn12,
    SignalReturn6,
    SignalStoch14,
    SignalTransitionRatio12,
    SignalTransitionRatio3,
    TargetEthusdPerpM1BlockReturn,
    TargetEthusdPerpM1GreenCount,
    TargetEthusdPerpM1MinuteReturnLag2,
    TargetEthusdPerpM1SegmentReturn2,
    TargetEthusdPerpM1SegmentTakerImbalance1,
    TargetEthusdPerpM1SegmentTakerImbalance2,
}

impl Feature {
    pub const ALL: &'static [Self] = &[
        Self::FutBtcusdtM1CloseLocation,
        Self::FutBtcusdtM1GreenRatio,
        Self::FutBtcusdtM1MinuteReturnLag2,
        Self::FutBtcusdtM1MinuteReturnLag3,
        Self::FutBtcusdtM1MinuteTakerLag2,
        Self::FutBtcusdtM1SegmentReturn2,
        Self::FutBtcusdtM1SegmentReturnAcceleration,
        Self::FutBtcusdtM1SegmentTakerImbalance2,
        Self::FutEthusdtM1BlockReturn,
        Self::FutEthusdtM1CloseLocation,
        Self::FutEthusdtM1MinuteReturnLag1,
        Self::FutEthusdtM1MinuteReturnLag3,
        Self::FutEthusdtM1SegmentReturn2,
        Self::FutEthusdtM1SegmentReturnAcceleration,
        Self::FutEthusdtM1SegmentTakerAcceleration,
        Self::FutEthusdtM1SegmentTakerImbalance2,
        Self::IndexEthusdtReturn1,
        Self::MarkBtcusdtCloseLocation,
        Self::MarkEthusdtReturn1,
        Self::MarkEthusdtReturn3,
        Self::OiEthusdtValueChange12,
        Self::OiEthusdtValueChange6,
        Self::SignalBreakoutHigh20,
        Self::SignalCloseEma8Atr,
        Self::SignalEma8Ema21Atr,
        Self::SignalGreenRatio12,
        Self::SignalGreenRatio3,
        Self::SignalGreenRatio6,
        Self::SignalReturn12,
        Self::SignalReturn6,
        Self::SignalStoch14,
        Self::SignalTransitionRatio12,
        Self::SignalTransitionRatio3,
        Self::TargetEthusdPerpM1BlockReturn,
        Self::TargetEthusdPerpM1GreenCount,
        Self::TargetEthusdPerpM1MinuteReturnLag2,
        Self::TargetEthusdPerpM1SegmentReturn2,
        Self::TargetEthusdPerpM1SegmentTakerImbalance1,
        Self::TargetEthusdPerpM1SegmentTakerImbalance2,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FutBtcusdtM1CloseLocation => "fut_btcusdt_m1_close_location",
            Self::FutBtcusdtM1GreenRatio => "fut_btcusdt_m1_green_ratio",
            Self::FutBtcusdtM1MinuteReturnLag2 => "fut_btcusdt_m1_minute_return_lag_2",
            Self::FutBtcusdtM1MinuteReturnLag3 => "fut_btcusdt_m1_minute_return_lag_3",
            Self::FutBtcusdtM1MinuteTakerLag2 => "fut_btcusdt_m1_minute_taker_lag_2",
            Self::FutBtcusdtM1SegmentReturn2 => "fut_btcusdt_m1_segment_return_2",
            Self::FutBtcusdtM1SegmentReturnAcceleration => {
                "fut_btcusdt_m1_segment_return_acceleration"
            }
            Self::FutBtcusdtM1SegmentTakerImbalance2 => "fut_btcusdt_m1_segment_taker_imbalance_2",
            Self::FutEthusdtM1BlockReturn => "fut_ethusdt_m1_block_return",
            Self::FutEthusdtM1CloseLocation => "fut_ethusdt_m1_close_location",
            Self::FutEthusdtM1MinuteReturnLag1 => "fut_ethusdt_m1_minute_return_lag_1",
            Self::FutEthusdtM1MinuteReturnLag3 => "fut_ethusdt_m1_minute_return_lag_3",
            Self::FutEthusdtM1SegmentReturn2 => "fut_ethusdt_m1_segment_return_2",
            Self::FutEthusdtM1SegmentReturnAcceleration => {
                "fut_ethusdt_m1_segment_return_acceleration"
            }
            Self::FutEthusdtM1SegmentTakerAcceleration => {
                "fut_ethusdt_m1_segment_taker_acceleration"
            }
            Self::FutEthusdtM1SegmentTakerImbalance2 => "fut_ethusdt_m1_segment_taker_imbalance_2",
            Self::IndexEthusdtReturn1 => "index_ethusdt_return_1",
            Self::MarkBtcusdtCloseLocation => "mark_btcusdt_close_location",
            Self::MarkEthusdtReturn1 => "mark_ethusdt_return_1",
            Self::MarkEthusdtReturn3 => "mark_ethusdt_return_3",
            Self::OiEthusdtValueChange12 => "oi_ethusdt_value_change_12",
            Self::OiEthusdtValueChange6 => "oi_ethusdt_value_change_6",
            Self::SignalBreakoutHigh20 => "signal_breakout_high_20",
            Self::SignalCloseEma8Atr => "signal_close_ema8_atr",
            Self::SignalEma8Ema21Atr => "signal_ema8_ema21_atr",
            Self::SignalGreenRatio12 => "signal_green_ratio_12",
            Self::SignalGreenRatio3 => "signal_green_ratio_3",
            Self::SignalGreenRatio6 => "signal_green_ratio_6",
            Self::SignalReturn12 => "signal_return_12",
            Self::SignalReturn6 => "signal_return_6",
            Self::SignalStoch14 => "signal_stoch_14",
            Self::SignalTransitionRatio12 => "signal_transition_ratio_12",
            Self::SignalTransitionRatio3 => "signal_transition_ratio_3",
            Self::TargetEthusdPerpM1BlockReturn => "target_ethusd_perp_m1_block_return",
            Self::TargetEthusdPerpM1GreenCount => "target_ethusd_perp_m1_green_count",
            Self::TargetEthusdPerpM1MinuteReturnLag2 => "target_ethusd_perp_m1_minute_return_lag_2",
            Self::TargetEthusdPerpM1SegmentReturn2 => "target_ethusd_perp_m1_segment_return_2",
            Self::TargetEthusdPerpM1SegmentTakerImbalance1 => {
                "target_ethusd_perp_m1_segment_taker_imbalance_1"
            }
            Self::TargetEthusdPerpM1SegmentTakerImbalance2 => {
                "target_ethusd_perp_m1_segment_taker_imbalance_2"
            }
        }
    }
}

impl FromStr for Feature {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|feature| feature.as_str() == value)
            .ok_or_else(|| format!("feature microstructure inconnue: {value}"))
    }
}

#[derive(Debug, Clone)]
pub struct MicrostructureSnapshot {
    candle: Candle,
    values: BTreeMap<Feature, f64>,
    observed_at: DateTime<Utc>,
    feature_source_times: BTreeMap<Feature, DateTime<Utc>>,
}

impl MicrostructureSnapshot {
    pub fn new(candle: Candle, values: BTreeMap<Feature, f64>) -> Self {
        let observed_at = candle.close_time;
        let feature_source_times = values
            .keys()
            .copied()
            .map(|feature| (feature, observed_at))
            .collect();
        Self {
            candle,
            values,
            observed_at,
            feature_source_times,
        }
    }

    pub fn with_metadata(
        candle: Candle,
        values: BTreeMap<Feature, f64>,
        observed_at: DateTime<Utc>,
        feature_source_times: BTreeMap<Feature, DateTime<Utc>>,
    ) -> Self {
        Self {
            candle,
            values,
            observed_at,
            feature_source_times,
        }
    }

    pub fn candle(&self) -> &Candle {
        &self.candle
    }

    pub fn value(&self, feature: Feature) -> Option<f64> {
        self.values.get(&feature).copied()
    }

    pub fn values(&self) -> &BTreeMap<Feature, f64> {
        &self.values
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub fn feature_source_times(&self) -> &BTreeMap<Feature, DateTime<Utc>> {
        &self.feature_source_times
    }

    pub fn is_complete(&self) -> bool {
        Feature::ALL
            .iter()
            .all(|feature| self.values.contains_key(feature))
    }

    pub fn ensure_complete(&self) -> Result<()> {
        let missing: Vec<_> = Feature::ALL
            .iter()
            .filter(|feature| !self.values.contains_key(feature))
            .map(|feature| feature.as_str())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            bail!("snapshot microstructure incomplet: {}", missing.join(", "))
        }
    }

    pub fn ensure_audit_complete(&self) -> Result<()> {
        self.ensure_complete()?;
        for feature in Feature::ALL {
            let source_time = self
                .feature_source_times
                .get(feature)
                .ok_or_else(|| anyhow!("horodatage source absent pour {}", feature.as_str()))?;
            if *source_time > self.candle.close_time {
                bail!(
                    "horodatage source futur pour {}: {} > {}",
                    feature.as_str(),
                    source_time,
                    self.candle.close_time
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct RichCandle {
    candle: Candle,
    quote_volume: f64,
    taker_buy_quote_volume: f64,
}

#[derive(Debug, Clone)]
struct OpenInterest {
    timestamp: DateTime<Utc>,
    value: f64,
}

#[derive(Debug, Deserialize)]
struct ServerTime {
    #[serde(rename = "serverTime")]
    server_time: i64,
}

#[derive(Debug, Deserialize)]
struct OpenInterestRow {
    timestamp: i64,
    #[serde(rename = "sumOpenInterestValue")]
    value: String,
}

/// Fetches the same public Binance series and uses the same causal cut-off as
/// the frozen Python forward validator.
pub struct EthUsdPerpMicrostructureCollector {
    client: Client,
}

impl EthUsdPerpMicrostructureCollector {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("creation client HTTP microstructure Binance")?;
        Ok(Self { client })
    }

    pub async fn fetch_snapshot(&self) -> Result<MicrostructureSnapshot> {
        let (coin_m_time, usd_m_time) = tokio::try_join!(
            self.fetch_server_time(COIN_M_API, "/dapi/v1/time"),
            self.fetch_server_time(USD_M_API, "/fapi/v1/time"),
        )?;
        let observed_at = coin_m_time.min(usd_m_time);

        let (
            target_m1,
            target_m15,
            btc_m1,
            _btc_m15,
            eth_m1,
            _eth_m15,
            mark_btc_m15,
            mark_eth_m15,
            index_eth_m15,
            open_interest,
        ) = tokio::try_join!(
            self.fetch_klines(
                COIN_M_API,
                "/dapi/v1/klines",
                "ETHUSD_PERP",
                "1m",
                1500,
                observed_at
            ),
            self.fetch_klines(
                COIN_M_API,
                "/dapi/v1/klines",
                "ETHUSD_PERP",
                "15m",
                500,
                observed_at
            ),
            self.fetch_klines(
                USD_M_API,
                "/fapi/v1/klines",
                "BTCUSDT",
                "1m",
                1500,
                observed_at
            ),
            self.fetch_klines(
                USD_M_API,
                "/fapi/v1/klines",
                "BTCUSDT",
                "15m",
                500,
                observed_at
            ),
            self.fetch_klines(
                USD_M_API,
                "/fapi/v1/klines",
                "ETHUSDT",
                "1m",
                1500,
                observed_at
            ),
            self.fetch_klines(
                USD_M_API,
                "/fapi/v1/klines",
                "ETHUSDT",
                "15m",
                500,
                observed_at
            ),
            self.fetch_klines(
                USD_M_API,
                "/fapi/v1/markPriceKlines",
                "BTCUSDT",
                "15m",
                500,
                observed_at
            ),
            self.fetch_klines(
                USD_M_API,
                "/fapi/v1/markPriceKlines",
                "ETHUSDT",
                "15m",
                500,
                observed_at
            ),
            self.fetch_index_klines("ETHUSDT", observed_at),
            self.fetch_open_interest(observed_at),
        )?;

        let target_index = target_m15
            .iter()
            .rposition(|candle| candle.candle.close_time <= observed_at)
            .ok_or_else(|| anyhow!("aucune bougie COIN-M ETHUSD_PERP 15m fermee"))?;
        let target = &target_m15[target_index];
        let decision_time = target.candle.close_time;
        let signal_open = target.candle.open_time;
        let mut feature_source_times = Feature::ALL
            .iter()
            .copied()
            .map(|feature| (feature, decision_time))
            .collect::<BTreeMap<_, _>>();

        let mut values = technical_values(&target_m15, target_index)?;
        let target_m1 = aggregate_m1(&target_m1, signal_open)?;
        let btc_m1 = aggregate_m1(&btc_m1, signal_open)?;
        let eth_m1 = aggregate_m1(&eth_m1, signal_open)?;

        insert(
            &mut values,
            Feature::TargetEthusdPerpM1BlockReturn,
            target_m1.block_return,
        );
        insert(
            &mut values,
            Feature::TargetEthusdPerpM1GreenCount,
            target_m1.green_count as f64,
        );
        insert(
            &mut values,
            Feature::TargetEthusdPerpM1MinuteReturnLag2,
            target_m1.minute_return_lag(2)?,
        );
        insert(
            &mut values,
            Feature::TargetEthusdPerpM1SegmentReturn2,
            target_m1.segment_return(2)?,
        );
        insert(
            &mut values,
            Feature::TargetEthusdPerpM1SegmentTakerImbalance1,
            target_m1.segment_taker_imbalance(1)?,
        );
        insert(
            &mut values,
            Feature::TargetEthusdPerpM1SegmentTakerImbalance2,
            target_m1.segment_taker_imbalance(2)?,
        );

        insert(
            &mut values,
            Feature::FutBtcusdtM1CloseLocation,
            btc_m1.close_location,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1GreenRatio,
            btc_m1.green_ratio,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1MinuteReturnLag2,
            btc_m1.minute_return_lag(2)?,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1MinuteReturnLag3,
            btc_m1.minute_return_lag(3)?,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1MinuteTakerLag2,
            btc_m1.minute_taker_lag(2)?,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1SegmentReturn2,
            btc_m1.segment_return(2)?,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1SegmentReturnAcceleration,
            btc_m1.segment_return_acceleration()?,
        );
        insert(
            &mut values,
            Feature::FutBtcusdtM1SegmentTakerImbalance2,
            btc_m1.segment_taker_imbalance(2)?,
        );

        insert(
            &mut values,
            Feature::FutEthusdtM1BlockReturn,
            eth_m1.block_return,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1CloseLocation,
            eth_m1.close_location,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1MinuteReturnLag1,
            eth_m1.minute_return_lag(1)?,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1MinuteReturnLag3,
            eth_m1.minute_return_lag(3)?,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1SegmentReturn2,
            eth_m1.segment_return(2)?,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1SegmentReturnAcceleration,
            eth_m1.segment_return_acceleration()?,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1SegmentTakerAcceleration,
            eth_m1.segment_taker_acceleration()?,
        );
        insert(
            &mut values,
            Feature::FutEthusdtM1SegmentTakerImbalance2,
            eth_m1.segment_taker_imbalance(2)?,
        );

        let mark_btc_index = index_at_open(&mark_btc_m15, signal_open, "mark BTCUSDT")?;
        insert(
            &mut values,
            Feature::MarkBtcusdtCloseLocation,
            close_location(&mark_btc_m15[mark_btc_index]),
        );

        let mark_eth_index = index_at_open(&mark_eth_m15, signal_open, "mark ETHUSDT")?;
        insert(
            &mut values,
            Feature::MarkEthusdtReturn1,
            return_n(&mark_eth_m15, mark_eth_index, 1)?,
        );
        insert(
            &mut values,
            Feature::MarkEthusdtReturn3,
            return_n(&mark_eth_m15, mark_eth_index, 3)?,
        );

        let index_eth_index = index_at_open(&index_eth_m15, signal_open, "index ETHUSDT")?;
        insert(
            &mut values,
            Feature::IndexEthusdtReturn1,
            return_n(&index_eth_m15, index_eth_index, 1)?,
        );

        let (oi_change_6, oi_source_time) =
            open_interest_value_change_with_timestamp(&open_interest, decision_time, 6)?;
        insert(&mut values, Feature::OiEthusdtValueChange6, oi_change_6);
        feature_source_times.insert(Feature::OiEthusdtValueChange6, oi_source_time);

        let (oi_change_12, oi_source_time) =
            open_interest_value_change_with_timestamp(&open_interest, decision_time, 12)?;
        insert(&mut values, Feature::OiEthusdtValueChange12, oi_change_12);
        feature_source_times.insert(Feature::OiEthusdtValueChange12, oi_source_time);

        let snapshot = MicrostructureSnapshot::with_metadata(
            target.candle.clone(),
            values,
            observed_at,
            feature_source_times,
        );
        snapshot.ensure_audit_complete()?;
        Ok(snapshot)
    }

    async fn fetch_server_time(&self, base: &str, path: &str) -> Result<DateTime<Utc>> {
        let response = self
            .client
            .get(format!("{base}{path}"))
            .send()
            .await
            .with_context(|| format!("requete heure serveur Binance {base}{path}"))?
            .error_for_status()
            .with_context(|| format!("statut heure serveur Binance {base}{path}"))?
            .json::<ServerTime>()
            .await
            .context("parsing heure serveur Binance")?;
        DateTime::from_timestamp_millis(response.server_time)
            .ok_or_else(|| anyhow!("horodatage Binance invalide {}", response.server_time))
    }

    async fn fetch_klines(
        &self,
        base: &str,
        path: &str,
        symbol: &str,
        interval: &str,
        limit: u32,
        observed_at: DateTime<Utc>,
    ) -> Result<Vec<RichCandle>> {
        let limit = limit.to_string();
        let payload = self
            .client
            .get(format!("{base}{path}"))
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("limit", limit.as_str()),
            ])
            .send()
            .await
            .with_context(|| format!("requete klines {symbol} {interval}"))?
            .error_for_status()
            .with_context(|| format!("statut klines {symbol} {interval}"))?
            .json::<Vec<Value>>()
            .await
            .with_context(|| format!("parsing klines {symbol} {interval}"))?;
        parse_klines(payload, observed_at)
    }

    async fn fetch_index_klines(
        &self,
        pair: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<Vec<RichCandle>> {
        let payload = self
            .client
            .get(format!("{USD_M_API}/fapi/v1/indexPriceKlines"))
            .query(&[("pair", pair), ("interval", "15m"), ("limit", "500")])
            .send()
            .await
            .with_context(|| format!("requete index klines {pair}"))?
            .error_for_status()
            .with_context(|| format!("statut index klines {pair}"))?
            .json::<Vec<Value>>()
            .await
            .with_context(|| format!("parsing index klines {pair}"))?;
        parse_klines(payload, observed_at)
    }

    async fn fetch_open_interest(&self, observed_at: DateTime<Utc>) -> Result<Vec<OpenInterest>> {
        let payload = self
            .client
            .get(format!("{USD_M_API}/futures/data/openInterestHist"))
            .query(&[("symbol", "ETHUSDT"), ("period", "5m"), ("limit", "500")])
            .send()
            .await
            .context("requete open interest ETHUSDT")?
            .error_for_status()
            .context("statut open interest ETHUSDT")?
            .json::<Vec<OpenInterestRow>>()
            .await
            .context("parsing open interest ETHUSDT")?;

        let mut rows = Vec::with_capacity(payload.len());
        for row in payload {
            let timestamp = DateTime::from_timestamp_millis(row.timestamp)
                .ok_or_else(|| anyhow!("horodatage OI invalide {}", row.timestamp))?;
            let value = row
                .value
                .parse::<f64>()
                .with_context(|| format!("valeur OI invalide {}", row.value))?;
            if !value.is_finite() || value <= 0.0 {
                bail!("valeur OI invalide {value}");
            }
            if timestamp <= observed_at {
                rows.push(OpenInterest { timestamp, value });
            }
        }
        rows.sort_by_key(|row| row.timestamp);
        rows.dedup_by_key(|row| row.timestamp);
        if rows.is_empty() {
            bail!("aucune observation d'open interest causale");
        }
        Ok(rows)
    }
}

fn parse_klines(payload: Vec<Value>, observed_at: DateTime<Utc>) -> Result<Vec<RichCandle>> {
    let mut candles = Vec::with_capacity(payload.len());
    for row in payload {
        let fields = row
            .as_array()
            .ok_or_else(|| anyhow!("ligne kline Binance non-tableau"))?;
        if fields.len() < 11 {
            bail!("ligne kline Binance incomplete: {} champs", fields.len());
        }
        let open_time_ms = value_i64(&fields[0], "open_time")?;
        let close_time_ms = value_i64(&fields[6], "close_time")?;
        let open_time = DateTime::from_timestamp_millis(open_time_ms)
            .ok_or_else(|| anyhow!("open_time invalide {open_time_ms}"))?;
        let close_time = DateTime::from_timestamp_millis(close_time_ms)
            .ok_or_else(|| anyhow!("close_time invalide {close_time_ms}"))?;
        let open = value_f64(&fields[1], "open")?;
        let high = value_f64(&fields[2], "high")?;
        let low = value_f64(&fields[3], "low")?;
        let close = value_f64(&fields[4], "close")?;
        let volume = value_f64(&fields[5], "volume")?;
        let quote_volume = value_f64(&fields[7], "quote_volume")?;
        let taker_buy_quote_volume = value_f64(&fields[10], "taker_buy_quote_volume")?;

        if open <= 0.0 || high <= 0.0 || low <= 0.0 || close <= 0.0 {
            bail!("prix kline invalide pour {}", open_time);
        }
        if !volume.is_finite()
            || !quote_volume.is_finite()
            || !taker_buy_quote_volume.is_finite()
            || volume < 0.0
            || quote_volume < 0.0
            || taker_buy_quote_volume < 0.0
        {
            bail!("volume kline invalide pour {}", open_time);
        }
        if close_time <= observed_at {
            candles.push(RichCandle {
                candle: Candle {
                    open_time,
                    close_time,
                    open,
                    high,
                    low,
                    close,
                    volume,
                    is_closed: true,
                },
                quote_volume,
                taker_buy_quote_volume,
            });
        }
    }
    candles.sort_by_key(|row| row.candle.open_time);
    candles.dedup_by_key(|row| row.candle.open_time);
    if candles.is_empty() {
        bail!("aucune kline fermee avant {}", observed_at);
    }
    Ok(candles)
}

fn value_i64(value: &Value, field: &str) -> Result<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| anyhow!("{field} invalide: {value}"))
}

fn value_f64(value: &Value, field: &str) -> Result<f64> {
    let parsed = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| anyhow!("{field} invalide: {value}"))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        bail!("{field} non fini")
    }
}

fn frozen(value: f64) -> f64 {
    (value as f32) as f64
}

fn insert(values: &mut BTreeMap<Feature, f64>, feature: Feature, value: f64) {
    values.insert(feature, frozen(value));
}

fn technical_values(candles: &[RichCandle], index: usize) -> Result<BTreeMap<Feature, f64>> {
    if index < 20 {
        bail!("historique 15m insuffisant pour les variables signal");
    }
    let current = &candles[index].candle;
    let atr14 = (index + 1 - 14..=index)
        .map(|position| {
            let candle = &candles[position].candle;
            let previous = if position == 0 {
                candle.close
            } else {
                candles[position - 1].candle.close
            };
            (candle.high - candle.low)
                .max((candle.high - previous).abs())
                .max((candle.low - previous).abs())
        })
        .sum::<f64>()
        / 14.0;
    let ema8 = ema(candles, index, 8);
    let ema21 = ema(candles, index, 21);
    let mut values = BTreeMap::new();

    insert(
        &mut values,
        Feature::SignalCloseEma8Atr,
        (current.close - ema8) / (atr14 + EPS),
    );
    insert(
        &mut values,
        Feature::SignalEma8Ema21Atr,
        (ema8 - ema21) / (atr14 + EPS),
    );
    insert(
        &mut values,
        Feature::SignalGreenRatio3,
        green_ratio(candles, index, 3)?,
    );
    insert(
        &mut values,
        Feature::SignalGreenRatio6,
        green_ratio(candles, index, 6)?,
    );
    insert(
        &mut values,
        Feature::SignalGreenRatio12,
        green_ratio(candles, index, 12)?,
    );
    insert(
        &mut values,
        Feature::SignalTransitionRatio3,
        transition_ratio(candles, index, 3)?,
    );
    insert(
        &mut values,
        Feature::SignalTransitionRatio12,
        transition_ratio(candles, index, 12)?,
    );
    insert(
        &mut values,
        Feature::SignalReturn6,
        return_n(candles, index, 6)?,
    );
    insert(
        &mut values,
        Feature::SignalReturn12,
        return_n(candles, index, 12)?,
    );
    insert(
        &mut values,
        Feature::SignalStoch14,
        stoch14(candles, index)?,
    );
    let prior_high = candles[index - 20..index]
        .iter()
        .map(|row| row.candle.high)
        .fold(f64::NEG_INFINITY, f64::max);
    insert(
        &mut values,
        Feature::SignalBreakoutHigh20,
        f64::from((current.close > prior_high) as u8),
    );
    Ok(values)
}

fn ema(candles: &[RichCandle], index: usize, span: usize) -> f64 {
    let alpha = 2.0 / (span as f64 + 1.0);
    let mut value = candles[0].candle.close;
    for candle in candles.iter().take(index + 1).skip(1) {
        value += alpha * (candle.candle.close - value);
    }
    value
}

fn candle_color(candle: &RichCandle) -> i8 {
    if candle.candle.close > candle.candle.open {
        1
    } else if candle.candle.close < candle.candle.open {
        -1
    } else {
        0
    }
}

fn green_ratio(candles: &[RichCandle], index: usize, window: usize) -> Result<f64> {
    if index + 1 < window {
        bail!("historique insuffisant pour signal_green_ratio_{window}");
    }
    let start = index + 1 - window;
    let green = candles[start..=index]
        .iter()
        .filter(|candle| candle_color(candle) > 0)
        .count();
    Ok(green as f64 / window as f64)
}

fn transition_ratio(candles: &[RichCandle], index: usize, window: usize) -> Result<f64> {
    if index < window {
        bail!("historique insuffisant pour signal_transition_ratio_{window}");
    }
    let start = index + 1 - window;
    let transitions = (start..=index)
        .filter(|position| {
            candle_color(&candles[*position]) != candle_color(&candles[*position - 1])
        })
        .count();
    Ok(transitions as f64 / window as f64)
}

fn return_n(candles: &[RichCandle], index: usize, lag: usize) -> Result<f64> {
    let previous = index
        .checked_sub(lag)
        .ok_or_else(|| anyhow!("historique insuffisant pour retour {lag}"))?;
    let previous_close = candles[previous].candle.close;
    if previous_close <= 0.0 {
        bail!("close historique invalide");
    }
    Ok(candles[index].candle.close / previous_close - 1.0)
}

fn stoch14(candles: &[RichCandle], index: usize) -> Result<f64> {
    if index + 1 < 14 {
        bail!("historique insuffisant pour stochastique 14");
    }
    let start = index + 1 - 14;
    let low = candles[start..=index]
        .iter()
        .map(|row| row.candle.low)
        .fold(f64::INFINITY, f64::min);
    let high = candles[start..=index]
        .iter()
        .map(|row| row.candle.high)
        .fold(f64::NEG_INFINITY, f64::max);
    Ok(100.0 * (candles[index].candle.close - low) / (high - low + EPS))
}

#[derive(Debug)]
struct M1Aggregate {
    block_return: f64,
    close_location: f64,
    green_count: usize,
    green_ratio: f64,
    minute_returns: Vec<f64>,
    minute_takers: Vec<f64>,
    segment_returns: [f64; 3],
    segment_taker_imbalances: [f64; 3],
}

impl M1Aggregate {
    fn minute_return_lag(&self, lag: usize) -> Result<f64> {
        let index = self
            .minute_returns
            .len()
            .checked_sub(lag)
            .ok_or_else(|| anyhow!("retard minute_return_{lag} indisponible"))?;
        Ok(self.minute_returns[index])
    }

    fn minute_taker_lag(&self, lag: usize) -> Result<f64> {
        let index = self
            .minute_takers
            .len()
            .checked_sub(lag)
            .ok_or_else(|| anyhow!("retard minute_taker_{lag} indisponible"))?;
        Ok(self.minute_takers[index])
    }

    fn segment_return(&self, segment: usize) -> Result<f64> {
        self.segment_returns
            .get(segment)
            .copied()
            .ok_or_else(|| anyhow!("segment_return_{segment} indisponible"))
    }

    fn segment_taker_imbalance(&self, segment: usize) -> Result<f64> {
        self.segment_taker_imbalances
            .get(segment)
            .copied()
            .ok_or_else(|| anyhow!("segment_taker_imbalance_{segment} indisponible"))
    }

    fn segment_return_acceleration(&self) -> Result<f64> {
        Ok(self.segment_return(2)? - self.segment_return(0)?)
    }

    fn segment_taker_acceleration(&self) -> Result<f64> {
        Ok(self.segment_taker_imbalance(2)? - self.segment_taker_imbalance(0)?)
    }
}

fn aggregate_m1(candles: &[RichCandle], signal_open: DateTime<Utc>) -> Result<M1Aggregate> {
    let end = signal_open + ChronoDuration::minutes(15);
    let window: Vec<_> = candles
        .iter()
        .filter(|candle| candle.candle.open_time >= signal_open && candle.candle.open_time < end)
        .collect();

    if window.len() != 15 {
        bail!(
            "m1 incompletes pour {}: {} bougies au lieu de 15",
            signal_open,
            window.len()
        );
    }
    for (index, candle) in window.iter().enumerate() {
        let expected = signal_open + ChronoDuration::minutes(index as i64);
        if candle.candle.open_time != expected || candle.candle.close_time > end {
            bail!(
                "m1 hors alignement pour {} a la position {}",
                signal_open,
                index
            );
        }
    }

    let minute_returns: Vec<_> = window
        .iter()
        .map(|candle| candle.candle.close / (candle.candle.open + EPS) - 1.0)
        .collect();
    let minute_takers: Vec<_> = window
        .iter()
        .map(|candle| 2.0 * candle.taker_buy_quote_volume / (candle.quote_volume + EPS) - 1.0)
        .collect();
    let block_open = window[0].candle.open;
    let block_close = window[14].candle.close;
    let block_low = window
        .iter()
        .map(|candle| candle.candle.low)
        .fold(f64::INFINITY, f64::min);
    let block_high = window
        .iter()
        .map(|candle| candle.candle.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let green_count = window
        .iter()
        .filter(|candle| candle_color(candle) > 0)
        .count();

    let segment_returns = std::array::from_fn(|segment| {
        let start = segment * 5;
        let end = start + 5;
        window[end - 1].candle.close / (window[start].candle.open + EPS) - 1.0
    });
    let segment_taker_imbalances = std::array::from_fn(|segment| {
        let start = segment * 5;
        let end = start + 5;
        let quote = window[start..end]
            .iter()
            .map(|candle| candle.quote_volume)
            .sum::<f64>();
        let taker = window[start..end]
            .iter()
            .map(|candle| candle.taker_buy_quote_volume)
            .sum::<f64>();
        2.0 * taker / (quote + EPS) - 1.0
    });

    Ok(M1Aggregate {
        block_return: block_close / (block_open + EPS) - 1.0,
        close_location: (block_close - block_low) / (block_high - block_low + EPS),
        green_count,
        green_ratio: green_count as f64 / 15.0,
        minute_returns,
        minute_takers,
        segment_returns,
        segment_taker_imbalances,
    })
}

fn index_at_open(candles: &[RichCandle], open_time: DateTime<Utc>, source: &str) -> Result<usize> {
    let index = candles
        .iter()
        .position(|candle| candle.candle.open_time == open_time)
        .ok_or_else(|| anyhow!("bougie {source} absente pour {open_time}"))?;
    if candles[index].candle.close_time > open_time + ChronoDuration::minutes(15) {
        bail!("bougie {source} future pour {open_time}");
    }
    Ok(index)
}

fn close_location(candle: &RichCandle) -> f64 {
    (candle.candle.close - candle.candle.low) / (candle.candle.high - candle.candle.low + EPS)
}

fn open_interest_value_change_with_timestamp(
    rows: &[OpenInterest],
    decision_time: DateTime<Utc>,
    lag: usize,
) -> Result<(f64, DateTime<Utc>)> {
    let index = rows
        .iter()
        .rposition(|row| row.timestamp <= decision_time)
        .ok_or_else(|| anyhow!("aucun open interest causal pour {decision_time}"))?;
    let current = &rows[index];
    if decision_time - current.timestamp > ChronoDuration::minutes(15) {
        bail!(
            "open interest trop ancien pour {}: {}",
            decision_time,
            current.timestamp
        );
    }
    let previous = rows
        .get(
            index
                .checked_sub(lag)
                .ok_or_else(|| anyhow!("historique OI insuffisant pour lag {lag}"))?,
        )
        .ok_or_else(|| anyhow!("historique OI indisponible pour lag {lag}"))?;
    if previous.value <= 0.0 {
        bail!("valeur OI historique invalide");
    }
    Ok((current.value / previous.value - 1.0, current.timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rich_candle(open_time: DateTime<Utc>, open: f64, close: f64) -> RichCandle {
        RichCandle {
            candle: Candle {
                open_time,
                close_time: open_time + ChronoDuration::minutes(1)
                    - ChronoDuration::milliseconds(1),
                open,
                high: open.max(close) + 1.0,
                low: open.min(close) - 1.0,
                close,
                volume: 1.0,
                is_closed: true,
            },
            quote_volume: 100.0,
            taker_buy_quote_volume: 50.0,
        }
    }

    #[test]
    fn aggregate_m1_uses_closed_fifteen_minute_bucket() {
        let start = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let candles: Vec<_> = (0..15)
            .map(|index| rich_candle(start + ChronoDuration::minutes(index), 100.0, 101.0))
            .collect();

        let aggregate = aggregate_m1(&candles, start).unwrap();

        assert_eq!(aggregate.green_count, 15);
        assert!((aggregate.minute_return_lag(1).unwrap() - 0.01).abs() < 1e-10);
        assert!((aggregate.segment_return(2).unwrap() - 0.01).abs() < 1e-10);
    }

    #[test]
    fn snapshot_requires_all_frozen_features() {
        let candle = rich_candle(
            DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            100.0,
            101.0,
        )
        .candle;
        let snapshot = MicrostructureSnapshot::new(candle, BTreeMap::new());

        assert!(!snapshot.is_complete());
        assert!(snapshot.ensure_complete().is_err());
    }
}
