use tracing::debug;

use crate::binance::Candle;
use crate::microstructure::{Feature, MicrostructureSnapshot};
use crate::strategy::{MicrostructureDecisionSummary, Prediction, Signal, Strategy};

const STRATEGY_NAME: &str = "ethusd_perp_coinm_15m_microstructure_mixed_13";
const MIN_VOTES: u32 = 1;

#[derive(Clone, Copy)]
enum Vote {
    Green,
    Red,
}

#[derive(Clone, Copy)]
enum Operator {
    GreaterOrEqual,
    LessOrEqual,
    Equal,
}

#[derive(Clone, Copy)]
struct Condition {
    feature: Feature,
    operator: Operator,
    threshold: f64,
}

impl Condition {
    const fn ge(feature: Feature, threshold: f64) -> Self {
        Self {
            feature,
            operator: Operator::GreaterOrEqual,
            threshold,
        }
    }

    const fn le(feature: Feature, threshold: f64) -> Self {
        Self {
            feature,
            operator: Operator::LessOrEqual,
            threshold,
        }
    }

    const fn eq(feature: Feature, threshold: f64) -> Self {
        Self {
            feature,
            operator: Operator::Equal,
            threshold,
        }
    }

    fn matches(self, snapshot: &MicrostructureSnapshot) -> bool {
        let Some(value) = snapshot.value(self.feature) else {
            return false;
        };
        match self.operator {
            Operator::GreaterOrEqual => value >= self.threshold,
            Operator::LessOrEqual => value <= self.threshold,
            Operator::Equal => value == self.threshold,
        }
    }
}

struct Rule {
    name: &'static str,
    vote: Vote,
    conditions: &'static [Condition],
}

static RULES: &[Rule] = &[
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_1",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalEma8Ema21Atr, -0.9558222889900208),
            Condition::le(Feature::FutBtcusdtM1GreenRatio, 0.3333333432674408),
            Condition::le(Feature::FutEthusdtM1BlockReturn, -0.0016062239883467555),
            Condition::le(Feature::FutEthusdtM1CloseLocation, 0.09973753243684769),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_2",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalCloseEma8Atr, -0.5925500869750976),
            Condition::le(
                Feature::TargetEthusdPerpM1SegmentTakerImbalance2,
                -0.14666366577148438,
            ),
            Condition::le(
                Feature::FutBtcusdtM1SegmentReturnAcceleration,
                -0.0017797600012272596,
            ),
            Condition::le(
                Feature::FutEthusdtM1SegmentTakerImbalance2,
                -0.28041884303092957,
            ),
            Condition::le(
                Feature::FutEthusdtM1SegmentTakerAcceleration,
                -0.4976053386926651,
            ),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_3",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalGreenRatio3, 0.0),
            Condition::le(
                Feature::FutEthusdtM1MinuteReturnLag1,
                -0.0015310485614463687,
            ),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_4",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::FutBtcusdtM1MinuteReturnLag2, -0.001328605052549392),
            Condition::le(
                Feature::FutBtcusdtM1SegmentTakerImbalance2,
                -0.3259851783514023,
            ),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_5",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::MarkBtcusdtCloseLocation, 0.07163261622190475),
            Condition::le(Feature::OiEthusdtValueChange6, -0.013785077957436442),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_6",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalTransitionRatio12, 0.3333333432674408),
            Condition::le(Feature::FutBtcusdtM1SegmentReturn2, -0.00302238785661757),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_7",
        vote: Vote::Green,
        conditions: &[
            Condition::le(
                Feature::TargetEthusdPerpM1BlockReturn,
                -0.001649390789680183,
            ),
            Condition::le(Feature::FutBtcusdtM1MinuteTakerLag2, -0.5129081308841705),
            Condition::le(Feature::FutEthusdtM1SegmentReturn2, -0.0025398905854672194),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_8",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalEma8Ema21Atr, -0.9558222889900208),
            Condition::le(Feature::FutBtcusdtM1SegmentReturn2, -0.00302238785661757),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_9",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalGreenRatio12, 0.25),
            Condition::le(
                Feature::TargetEthusdPerpM1MinuteReturnLag2,
                -0.00168620451586321,
            ),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_10",
        vote: Vote::Green,
        conditions: &[
            Condition::le(Feature::SignalStoch14, 13.061938285827637),
            Condition::le(
                Feature::FutEthusdtM1MinuteReturnLag3,
                -0.0016388711519539356,
            ),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_11",
        vote: Vote::Green,
        conditions: &[
            Condition::eq(Feature::TargetEthusdPerpM1GreenCount, 5.0),
            Condition::le(Feature::OiEthusdtValueChange6, -0.013785077957436442),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_12",
        vote: Vote::Red,
        conditions: &[
            Condition::le(Feature::SignalTransitionRatio3, 0.0),
            Condition::ge(Feature::SignalGreenRatio6, 0.8333333134651184),
            Condition::ge(Feature::SignalCloseEma8Atr, 0.5562092304229737),
            Condition::ge(
                Feature::TargetEthusdPerpM1SegmentReturn2,
                0.0008606369374319911,
            ),
            Condition::ge(Feature::FutEthusdtM1SegmentReturn2, 0.0008581436122767627),
            Condition::ge(
                Feature::FutEthusdtM1SegmentTakerAcceleration,
                0.40127360820770264,
            ),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_13",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalGreenRatio6, 0.8333333134651184),
            Condition::ge(Feature::FutBtcusdtM1CloseLocation, 0.8853974342346191),
            Condition::ge(Feature::OiEthusdtValueChange6, 0.008817994501441733),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_14",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalGreenRatio6, 0.8333333134651184),
            Condition::ge(Feature::FutBtcusdtM1CloseLocation, 0.8853974342346191),
            Condition::ge(
                Feature::FutBtcusdtM1SegmentReturnAcceleration,
                0.0011242945911362767,
            ),
            Condition::ge(Feature::FutEthusdtM1SegmentReturn2, 0.0008581436122767627),
            Condition::ge(Feature::OiEthusdtValueChange12, 0.0074382608756422995),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_15",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(
                Feature::FutEthusdtM1SegmentTakerImbalance2,
                0.2779318690299988,
            ),
            Condition::ge(Feature::MarkEthusdtReturn3, 0.007906512822955845),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_16",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(
                Feature::FutEthusdtM1SegmentReturnAcceleration,
                0.004034372977912426,
            ),
            Condition::ge(Feature::MarkEthusdtReturn1, 0.006600463879294692),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_17",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalReturn6, 0.011234910413622857),
            Condition::ge(Feature::SignalGreenRatio3, 1.0),
            Condition::ge(Feature::FutBtcusdtM1CloseLocation, 0.8853974342346191),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_18",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalReturn6, 0.011234910413622857),
            Condition::ge(Feature::FutEthusdtM1SegmentReturn2, 0.0036720656789839268),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_19",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalReturn6, 0.011234910413622857),
            Condition::ge(Feature::FutBtcusdtM1CloseLocation, 0.938892275094986),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_20",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalReturn6, 0.011234910413622857),
            Condition::ge(
                Feature::TargetEthusdPerpM1SegmentTakerImbalance1,
                0.2385961264371872,
            ),
            Condition::ge(Feature::FutEthusdtM1SegmentReturn2, 0.0008581436122767627),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_21",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalCloseEma8Atr, 0.5562092304229737),
            Condition::ge(
                Feature::TargetEthusdPerpM1SegmentTakerImbalance1,
                0.2385961264371872,
            ),
            Condition::ge(Feature::IndexEthusdtReturn1, 0.006568198488093911),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_22",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalStoch14, 78.11871032714843),
            Condition::ge(
                Feature::TargetEthusdPerpM1SegmentTakerImbalance1,
                0.2385961264371872,
            ),
            Condition::ge(
                Feature::TargetEthusdPerpM1SegmentReturn2,
                0.0008606369374319911,
            ),
            Condition::ge(Feature::MarkEthusdtReturn3, 0.007906512822955845),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_23",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalReturn12, 0.01619062013924122),
            Condition::ge(Feature::FutBtcusdtM1MinuteReturnLag3, 0.0013222055276855826),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_24",
        vote: Vote::Red,
        conditions: &[
            Condition::ge(Feature::SignalStoch14, 78.11871032714843),
            Condition::eq(Feature::SignalBreakoutHigh20, 1.0),
            Condition::ge(
                Feature::TargetEthusdPerpM1SegmentReturn2,
                0.0037076674634590745,
            ),
            Condition::ge(Feature::FutBtcusdtM1MinuteReturnLag3, 0.0013222055276855826),
        ],
    },
    Rule {
        name: "ethusd_perp_coinm_15m_microstructure_mixed_13_rule_25",
        vote: Vote::Red,
        conditions: &[
            Condition::eq(Feature::SignalBreakoutHigh20, 1.0),
            Condition::ge(Feature::FutBtcusdtM1MinuteReturnLag3, 0.0013222055276855826),
            Condition::ge(Feature::FutEthusdtM1SegmentReturn2, 0.0036720656789839268),
        ],
    },
];

pub struct EthUsdPerpMicrostructureMixed13 {
    last_votes: (u32, u32),
    last_active_rules: String,
    last_decision: Option<MicrostructureDecisionSummary>,
}

impl EthUsdPerpMicrostructureMixed13 {
    pub fn new() -> Self {
        Self {
            last_votes: (0, 0),
            last_active_rules: String::new(),
            last_decision: None,
        }
    }

    fn evaluate(&mut self, snapshot: &MicrostructureSnapshot) -> Option<Signal> {
        if let Err(error) = snapshot.ensure_complete() {
            self.last_votes = (0, 0);
            self.last_active_rules = format!("snapshot_incomplet: {error}");
            self.last_decision = Some(MicrostructureDecisionSummary {
                prediction: None,
                green_votes: 0,
                red_votes: 0,
                active_rules: vec![self.last_active_rules.clone()],
            });
            return None;
        }

        let (mut green_votes, mut red_votes) = (0u32, 0u32);
        let mut active_rules = Vec::new();
        for rule in RULES {
            if rule
                .conditions
                .iter()
                .all(|condition| condition.matches(snapshot))
            {
                active_rules.push(rule.name);
                match rule.vote {
                    Vote::Green => green_votes += 1,
                    Vote::Red => red_votes += 1,
                }
            }
        }

        self.last_votes = (green_votes, red_votes);
        self.last_active_rules = active_rules.join(",");
        debug!(
            "[MICROSTRUCTURE] green_votes={} red_votes={} active_rules={}",
            green_votes, red_votes, self.last_active_rules
        );

        let prediction = match (green_votes > 0, red_votes > 0) {
            (true, false) => Some(Prediction::Up),
            (false, true) => Some(Prediction::Down),
            _ => None,
        };
        self.last_decision = Some(MicrostructureDecisionSummary {
            prediction: prediction.clone(),
            green_votes,
            red_votes,
            active_rules: active_rules
                .iter()
                .map(|rule| (*rule).to_string())
                .collect(),
        });

        let prediction = prediction?;
        let total = green_votes + red_votes;
        let vote_pct = green_votes.max(red_votes) as f64 / total as f64 * 100.0;

        Some(Signal {
            prediction,
            signal_candle_close_time: snapshot.candle().close_time,
            rsi: vote_pct,
            strategy_name: self.name().to_string(),
        })
    }
}

impl Default for EthUsdPerpMicrostructureMixed13 {
    fn default() -> Self {
        Self::new()
    }
}

impl Strategy for EthUsdPerpMicrostructureMixed13 {
    fn name(&self) -> &str {
        STRATEGY_NAME
    }

    fn warmup(&mut self, _candle: &Candle) {}

    fn on_closed_candle(&mut self, _candle: &Candle) -> Option<Signal> {
        None
    }

    fn requires_microstructure(&self) -> bool {
        true
    }

    fn on_microstructure_snapshot(&mut self, snapshot: &MicrostructureSnapshot) -> Option<Signal> {
        self.evaluate(snapshot)
    }

    fn last_microstructure_decision_summary(&self) -> Option<MicrostructureDecisionSummary> {
        self.last_decision.clone()
    }

    fn current_rsi(&self) -> Option<f64> {
        None
    }

    fn current_series(&self) -> Option<bool> {
        None
    }

    fn current_atr(&self) -> Option<f64> {
        None
    }

    fn candle_log_extras(&self) -> String {
        let (green_votes, red_votes) = self.last_votes;
        let total = green_votes + red_votes;
        let votes = if total == 0 {
            format!("green=0 | red=0 | total=0 | min_votes={MIN_VOTES}")
        } else {
            format!(
                "green={} | red={} | total={} | pct={:.1}% | min_votes={}",
                green_votes,
                red_votes,
                total,
                green_votes.max(red_votes) as f64 / total as f64 * 100.0,
                MIN_VOTES
            )
        };
        if self.last_active_rules.is_empty() {
            votes
        } else {
            format!("{votes} | active={}", self.last_active_rules)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    struct ForwardFixtureRow {
        prediction: String,
        green_votes: u32,
        red_votes: u32,
        values: Vec<f64>,
    }

    fn snapshot(values: BTreeMap<Feature, f64>) -> MicrostructureSnapshot {
        MicrostructureSnapshot::new(
            Candle {
                open_time: Utc::now(),
                close_time: Utc::now() + Duration::minutes(15),
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 1.0,
                is_closed: true,
            },
            values,
        )
    }

    fn complete_values() -> BTreeMap<Feature, f64> {
        Feature::ALL
            .iter()
            .copied()
            .map(|feature| (feature, 0.0))
            .collect()
    }

    #[test]
    fn emits_up_when_only_green_rules_are_active() {
        let mut values = complete_values();
        values.insert(Feature::SignalGreenRatio3, 0.0);
        values.insert(Feature::FutEthusdtM1MinuteReturnLag1, -0.002);

        let mut strategy = EthUsdPerpMicrostructureMixed13::new();
        let signal = strategy
            .on_microstructure_snapshot(&snapshot(values))
            .unwrap();

        assert_eq!(signal.prediction, Prediction::Up);
        assert!(strategy
            .candle_log_extras()
            .contains("ethusd_perp_coinm_15m_microstructure_mixed_13_rule_3"));
    }

    #[test]
    fn skips_when_green_and_red_rules_are_both_active() {
        let mut values = complete_values();
        values.insert(Feature::SignalGreenRatio3, 0.0);
        values.insert(Feature::FutEthusdtM1MinuteReturnLag1, -0.002);
        values.insert(Feature::SignalTransitionRatio3, 0.0);
        values.insert(Feature::SignalGreenRatio6, 0.9);
        values.insert(Feature::SignalCloseEma8Atr, 0.6);
        values.insert(Feature::TargetEthusdPerpM1SegmentReturn2, 0.001);
        values.insert(Feature::FutEthusdtM1SegmentReturn2, 0.001);
        values.insert(Feature::FutEthusdtM1SegmentTakerAcceleration, 0.5);

        let mut strategy = EthUsdPerpMicrostructureMixed13::new();

        assert!(strategy
            .on_microstructure_snapshot(&snapshot(values))
            .is_none());
        assert!(strategy.candle_log_extras().contains("green=1 | red=1"));
    }

    #[test]
    fn matches_frozen_forward_snapshot_fixture() {
        let rows: Vec<ForwardFixtureRow> = serde_json::from_str(include_str!(
            "../../tests/fixtures/ethusd_perp_mixed_13_forward.json"
        ))
        .unwrap();

        for row in rows {
            assert_eq!(row.values.len(), Feature::ALL.len());
            let values = Feature::ALL
                .iter()
                .copied()
                .zip(row.values)
                .collect::<BTreeMap<_, _>>();
            let mut strategy = EthUsdPerpMicrostructureMixed13::new();
            let signal = strategy.on_microstructure_snapshot(&snapshot(values));

            match row.prediction.as_str() {
                "SKIP" => assert!(signal.is_none()),
                "GREEN" => assert_eq!(signal.unwrap().prediction, Prediction::Up),
                "RED" => assert_eq!(signal.unwrap().prediction, Prediction::Down),
                other => panic!("prediction fixture inconnue: {other}"),
            }
            assert!(strategy
                .candle_log_extras()
                .contains(&format!("green={}", row.green_votes)));
            assert!(strategy
                .candle_log_extras()
                .contains(&format!("red={}", row.red_votes)));
        }
    }
}
