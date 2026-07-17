use crate::binance::Candle;
use crate::microstructure::MicrostructureSnapshot;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Prediction {
    Up,
    Down,
}

/// Résultat déterministe de la dernière évaluation microstructure.
///
/// Il est séparé du signal afin qu'un `SKIP` reste traçable dans le journal
/// d'audit sans jamais être interprété comme un ordre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrostructureDecisionSummary {
    pub prediction: Option<Prediction>,
    pub green_votes: u32,
    pub red_votes: u32,
    pub active_rules: Vec<String>,
}

impl std::fmt::Display for Prediction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Prediction::Up => write!(f, "UP"),
            Prediction::Down => write!(f, "DOWN"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub prediction: Prediction,
    pub signal_candle_close_time: DateTime<Utc>,
    pub rsi: f64,
    pub strategy_name: String,
}

/// Abstraction permettant de brancher plusieurs strategies.
/// Chaque strategie recoit les bougies fermees une par une
/// et retourne un signal optionnel.
pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    fn on_closed_candle(&mut self, candle: &Candle) -> Option<Signal>;
    /// Indique que la strategie requiert le collecteur multi-sources Binance.
    fn requires_microstructure(&self) -> bool {
        false
    }
    /// Evalue un snapshot microstructure causal. Les strategies historiques
    /// utilisent l'implementation par defaut et restent alimentees par Candle.
    fn on_microstructure_snapshot(&mut self, _snapshot: &MicrostructureSnapshot) -> Option<Signal> {
        None
    }
    /// Retourne le résultat de la dernière évaluation microstructure lorsqu'il
    /// est disponible pour l'audit. Les stratégies historiques retournent `None`.
    fn last_microstructure_decision_summary(&self) -> Option<MicrostructureDecisionSummary> {
        None
    }
    /// Alimente l'historique sans logger ni retourner de signal (préchargement).
    fn warmup(&mut self, candle: &Candle);
    /// RSI courant (None si pas assez de bougies).
    fn current_rsi(&self) -> Option<f64>;
    /// Série des 3 dernières bougies : Some(true)=3xVERT, Some(false)=3xROUGE, None=mixte.
    fn current_series(&self) -> Option<bool>;
    /// ATR14 courant (None si pas assez de bougies).
    fn current_atr(&self) -> Option<f64>;
    /// Infos contextuelles à afficher dans le log de bougie fermée.
    /// Chaque stratégie retourne sa propre représentation.
    fn candle_log_extras(&self) -> String;
}
