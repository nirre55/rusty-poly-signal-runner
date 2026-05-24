use anyhow::Result;

use crate::config::Config;
use crate::strategies::btc_15m_rules_18_min_votes_1::BtcRules18;
use crate::strategies::btc_1h_rules_15_min_votes_1::BtcH1Rules15;
use crate::strategies::btc_5m_rules_23_min_votes_1::BtcRules23;
use crate::strategies::btc_5m_rules_90_min_votes_1::BtcRules90;
use crate::strategies::eth_15m_rules_24_min_votes_1::EthRules24;
use crate::strategies::eth_1h_rules_17_min_votes_1::EthH1Rules17;
use crate::strategies::eth_5m_rules_25_min_votes_1::EthRules25;
use crate::strategies::three_candle_rsi7_reversal::ThreeCandleRsi7Reversal;
use crate::strategy::Strategy;

pub fn create_strategy(config: &Config) -> Result<Box<dyn Strategy>> {
    match config.strategy.as_str() {
        "three_candle_rsi7_reversal" => Ok(Box::new(ThreeCandleRsi7Reversal::new(
            config.rsi_overbought,
            config.rsi_oversold,
        ))),
        "btc_5m_rules_90_min_votes_1" => Ok(Box::new(BtcRules90::new(config.ensemble_min_votes))),
        "btc_5m_rules_23_min_votes_1" => Ok(Box::new(BtcRules23::new(
            config.ensemble_min_votes,
        ))),
        "btc_15m_rules_18_min_votes_1" => Ok(Box::new(BtcRules18::new(config.ensemble_min_votes))),
        "btc_1h_rules_15_min_votes_1" => {
            Ok(Box::new(BtcH1Rules15::new(config.ensemble_min_votes)))
        },
        "eth_5m_rules_25_min_votes_1" => Ok(Box::new(EthRules25::new(config.ensemble_min_votes))),
        "eth_15m_rules_24_min_votes_1" => {
            Ok(Box::new(EthRules24::new(config.ensemble_min_votes)))
        }
        "eth_1h_rules_17_min_votes_1" => {
            Ok(Box::new(EthH1Rules17::new(config.ensemble_min_votes)))
        },
        other => anyhow::bail!(
            "Stratégie '{}' inconnue. Stratégies disponibles: three_candle_rsi7_reversal, btc_5m_rules_90_min_votes_1, btc_5m_rules_23_min_votes_1, btc_15m_rules_18_min_votes_1, eth_5m_rules_25_min_votes_1, eth_15m_rules_24_min_votes_1",
            other
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::create_strategy;
    use crate::config::{
        Config, ExecutionMode, LimitPriceHighGuard, LimitPriceReference, MarketOrderType,
        PolymarketSlugFormat,
    };

    fn config_with_strategy(strategy: &str) -> Config {
        Config {
            binance_ws_url: "wss://stream.binance.com:9443/ws".to_string(),
            symbol: "btcusdt".to_string(),
            interval: "5m".to_string(),
            execution_mode: ExecutionMode::DryRun,
            trade_amount_usdc: 10.0,
            polymarket_api_key: String::new(),
            polymarket_api_secret: String::new(),
            polymarket_api_url: "https://clob.polymarket.com".to_string(),
            logs_dir: "logs".to_string(),
            evm_private_key: None,
            polymarket_funder: None,
            polymarket_signature_type: None,
            strategy: strategy.to_string(),
            rsi_overbought: 65.0,
            rsi_oversold: 35.0,
            polymarket_slug_prefix: "btc-updown-5m".to_string(),
            polymarket_slug_format: PolymarketSlugFormat::Timestamp,
            polymarket_slug_asset: "bitcoin".to_string(),
            martingale_multiplier: 1.0,
            martingale_max_amount: 0.0,
            trade_amount_pct: 0.0,
            excluded_days: Vec::new(),
            excluded_hours: Vec::new(),
            ensemble_min_votes: 1,
            limit_price_reference: LimitPriceReference::BestAsk,
            limit_price_offset: 0.01,
            limit_price_high_guard: LimitPriceHighGuard {
                enabled: false,
                threshold: 0.60,
                price: 0.55,
            },
            market_order_type: MarketOrderType::Fok,
        }
    }

    #[test]
    fn creates_all_known_strategies() {
        for strategy_name in [
            "three_candle_rsi7_reversal",
            "btc_5m_rules_90_min_votes_1",
            "btc_5m_rules_23_min_votes_1",
            "btc_15m_rules_18_min_votes_1",
            "btc_1h_rules_15_min_votes_1",
            "eth_5m_rules_25_min_votes_1",
            "eth_15m_rules_24_min_votes_1",
            "eth_1h_rules_17_min_votes_1",
        ] {
            let strategy = create_strategy(&config_with_strategy(strategy_name))
                .expect("strategy should be created");
            assert_eq!(strategy.name(), strategy_name);
        }
    }

    #[test]
    fn rejects_unknown_strategy() {
        assert!(create_strategy(&config_with_strategy("missing")).is_err());
    }
}
