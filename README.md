# rusty-poly-signal-runner

Bot de trading live en Rust pour les marchés Polymarket Up/Down, alimenté par les bougies Binance.

Le projet supporte plusieurs stratégies configurables, le dry-run, les ordres réels Polymarket via CLOB, le suivi des positions, les logs CSV et une gestion optionnelle de la taille de position.

## Fonctionnalités

- WebSocket Binance pour bougies fermées.
- Warmup historique Binance au démarrage.
- Stratégies RSI/reversal et ensembles de micro-règles.
- Modes `dry-run`, `market` et `limit`.
- Résolution des marchés Polymarket par slug.
- Cache et warmup du client Polymarket CLOB.
- Suivi des ordres ouverts via `pending_orders.json`.
- Validation du résultat avec la bougie Binance cible.
- Persistance du money management dans `money_state.json`.
- Logs de trades dans `trades.csv`.

## Stratégies disponibles

| Nom | Marché typique | Description |
|---|---|---|
| `boll_fade` | BTC/ETH 5m/15m | Reversal d'une cloture de capitulation hors des bandes de Bollinger 20. |
| `streak_rsi` | BTC/ETH 5m/15m | Fade apres 3 bougies de meme couleur, RSI7 extreme, ATR et body filtres. |
| `trio_vote2` | BTC/ETH 5m/15m | Composite : au moins 2 votes concordants parmi Bollinger, streak/RSI et Donchian/z-score. |
| `reversal_pro` | BTC/ETH 5m/15m | Fade selectif : streak, RSI7/z24 extremes, range ATR et corps plein. |
| `three_candle_rsi7_reversal` | BTC 5m | Reversal après 3 bougies de même couleur + RSI7 + filtres range/body. |
| `btc_5m_rules_90_min_votes_1` | BTC 5m | Ensemble de 90 micro-règles. |
| `btc_5m_rules_23_min_votes_1` | BTC 5m | Ensemble combine de 23 micro-strategies, `min_votes=1`. |
| `btc_5m_rules_626_min_votes_1` | BTC 5m | Ensemble de 626 micro-regles. |
| `btc_15m_rules_18_min_votes_1` | BTC 15m | Ensemble de 18 micro-règles. |
| `btc_15m_rules_461_min_votes_1` | BTC 15m | Ensemble de 461 micro-regles. |
| `btc_1h_rules_15_min_votes_1` | BTC 1h | Ensemble de 15 micro-règles H1. |
| `eth_5m_rules_25_min_votes_1` | ETH 5m | Ensemble de 25 micro-règles. |
| `eth_5m_rules_542_min_votes_1` | ETH 5m | Ensemble simplifie de 542 micro-regles. |
| `eth_15m_rules_24_min_votes_1` | ETH 15m | Ensemble de 24 micro-règles. |
| `eth_15m_rules_663_min_votes_1` | ETH 15m | Ensemble de 663 micro-regles. |
| `eth_1h_rules_17_min_votes_1` | ETH 1h | Ensemble de 17 micro-règles H1. |
| `eth_1h_rules_210_min_votes_1` | ETH 1h | Ensemble de 210 micro-regles H1. |
| `five_year_70pct_btc_5m_rules_71_min_votes_1` | BTC 5m | Ensemble FIVE_YEAR_70PCT de 71 micro-regles. |
| `five_year_70pct_btc_15m_rules_176_min_votes_1` | BTC 15m | Ensemble FIVE_YEAR_70PCT de 176 micro-regles. |
| `five_year_70pct_btc_1h_rules_586_min_votes_1` | BTC 1h | Ensemble FIVE_YEAR_70PCT de 586 micro-regles. |
| `five_year_70pct_eth_5m_rules_75_min_votes_1` | ETH 5m | Ensemble FIVE_YEAR_70PCT de 75 micro-regles. |
| `five_year_70pct_eth_15m_rules_181_min_votes_1` | ETH 15m | Ensemble FIVE_YEAR_70PCT de 181 micro-regles. |
| `five_year_70pct_eth_1h_rules_632_min_votes_1` | ETH 1h | Ensemble FIVE_YEAR_70PCT de 632 micro-regles. |

La stratégie active se choisit avec `STRATEGY` ou via `STRATEGY_CONFIG`.

## Installation

```bash
rustup update stable
cargo build --locked
```

Copier ensuite l'exemple d'environnement :

```bash
cp .env.example .env
```

## Configuration

Variables principales :

| Variable | Description | Défaut |
|---|---|---|
| `EXECUTION_MODE` | `dry-run`, `market` ou `limit`. Requis. | Aucun |
| `STRATEGY_CONFIG` | Fichier `.env` de stratégie à charger avant `.env`. | Aucun |
| `STRATEGY` | Nom de la stratégie. | `three_candle_rsi7_reversal` |
| `SYMBOL` | Symbole Binance, ex. `btcusdt`, `ethusdt`. | `btcusdt` |
| `INTERVAL` | Intervalle Binance, ex. `5m`, `15m`. | `5m` |
| `POLYMARKET_SLUG_PREFIX` | Préfixe du slug Polymarket pour le format `timestamp`. | `btc-updown-5m` |
| `POLYMARKET_SLUG_FORMAT` | Format du slug Polymarket: `timestamp` pour 5m/15m, `hourly_et` pour les marches 1h. | `timestamp` |
| `POLYMARKET_SLUG_ASSET` | Asset utilise avec `hourly_et`, ex. `bitcoin`, `solana`. Si absent, derive de `SYMBOL`. | Selon `SYMBOL` |
| `POLYMARKET_API_URL` | URL CLOB Polymarket. | `https://clob.polymarket.com` |
| `TRADE_AMOUNT_USDC` | Montant fixe par trade. | `10.0` |
| `TRADE_AMOUNT_PCT` | Pourcentage du solde USDC à utiliser. Mutuellement exclusif avec `TRADE_AMOUNT_USDC`. | `0.0` |
| `ENSEMBLE_MIN_VOTES` | Nombre minimal de votes pour les stratégies ensemble. | `1` |
| `LIMIT_PRICE_REFERENCE` | Prix de reference des ordres limite: `best_ask` ou `best_bid`. | `best_ask` |
| `LIMIT_PRICE_OFFSET` | Offset signe ajoute au prix de reference en mode `limit`, ex. `0.01`, `0`, `-0.01`. | `0.01` |
| `PORTFOLIO_WINDOW_BUDGET_PCT` | Budget total partage par fenetre du runner Meche. | `3.5` |
| `PORTFOLIO_SIGNAL_CAP_PCT` | Plafond de sizing par signal du runner Meche. | `1.2` |
| `PORTFOLIO_SYNC_GRACE_MS` | Delai maximal d'attente des flux Binance attendus pour une fenetre Meche. | `1250` |
| `PORTFOLIO_ENABLED_CONFIG` | Grille persistante des 16 sorties strategie/marche. | `configs/meche050_enabled.env` |
| `LIMIT_PRICE_FIXED` | Prix limite fixe optionnel, ex. `0.50`. Si defini, ignore reference, offset et high guard. | Aucun |
| `LIMIT_PRICE_HIGH_GUARD_ENABLED` | Active le garde-fou des prix limite eleves. Si actif et que le prix calcule depasse le seuil, le bot force le prix configure. | `false` |
| `LIMIT_PRICE_HIGH_GUARD_THRESHOLD` | Seuil du garde-fou, ex. `0.60`. Le garde-fou s'applique seulement si le prix calcule est strictement superieur au seuil. | `0.60` |
| `LIMIT_PRICE_HIGH_GUARD_PRICE` | Prix limite force par le garde-fou, ex. `0.55`. Doit etre inferieur au seuil. | `0.55` |
| `MARTINGALE_MULTIPLIER` | Multiplicateur après une perte. `1.0` désactive la martingale. | `1.0` |
| `MARTINGALE_MAX_AMOUNT` | Plafond martingale. `0.0` désactive le plafond. | `0.0` |
| `EXCLUDED_DAYS` | Jours exclus, ex. `sat,sun`. | Vide |
| `EXCLUDED_HOURS` | Plages UTC exclues, ex. `0-9,22-24`. | Vide |
| `LOGS_DIR` | Dossier des logs et états runtime. | `logs` |

Variables nécessaires aux modes réels :

Pour les marches 1h Polymarket dont les slugs ressemblent a
`bitcoin-up-or-down-may-21-2026-11am-et`, utilisez :

```env
INTERVAL=1h
POLYMARKET_SLUG_FORMAT=hourly_et
POLYMARKET_SLUG_ASSET=bitcoin
```

Pour Solana :

```env
SYMBOL=solusdt
INTERVAL=1h
POLYMARKET_SLUG_FORMAT=hourly_et
POLYMARKET_SLUG_ASSET=solana
```

| Variable | Description |
|---|---|
| `POLYMARKET_PRIVATE_KEY` | Clé privée EVM du signer. |
| `POLYMARKET_FUNDER` | Adresse funder si différente de l'EOA, incluant proxy, Safe ou deposit wallet. |
| `POLYMARKET_SIGNATURE_TYPE` | `0` = EOA, `1` = proxy, `2` = Gnosis Safe, `3` = POLY_1271/deposit wallet. |
| `POLYMARKET_API_KEY` / `POLYMARKET_API_SECRET` / `POLYMARKET_API_PASSPHRASE` | Credentials CLOB explicites. Si les trois sont vides, le SDK derive une API key automatiquement. |

## Lancement

Dry-run direct :

```bash
EXECUTION_MODE=dry-run cargo run
```

Avec un fichier de stratégie :

```bash
STRATEGY_CONFIG=configs/btc_ensemble.env cargo run
```

Lancer les quatre stratégies ensemble :

```powershell
.\start_all.ps1
```

Par defaut, le script PowerShell fait un `cargo build --release` une seule fois, puis lance chaque strategie dans une fenetre separee avec l'executable compile, auto-restart, et logs console dans `logs/supervisor/*.console.log`.

Sur Ubuntu/server :

```bash
./start_all.sh start
./start_all.sh status
./start_all.sh stop
```

### Portefeuille Meche 0,50

Le portefeuille Meche utilise un seul processus pour ses 16 sorties logiques
(`boll_fade`, `streak_rsi`, `trio_vote2`, `reversal_pro` x BTC/ETH x 5m/15m).
Il agrege les signaux par ouverture de fenetre, applique W=3,5 % et f=1,2 %
sur le solde commun, puis fusionne les signaux identiques dans un ordre limite a 0,50.
Ne lancez pas en parallele les anciens bots sur le meme compte : ils ne partagent pas ce plafond.

```bash
chmod +x start_meche050.sh
./start_meche050.sh start
./start_meche050.sh status
./start_meche050.sh strategy disable boll_fade
./start_meche050.sh strategy enable boll_fade btc_5m
./start_meche050.sh strategy only trio_vote2 eth_15m
./start_meche050.sh strategy all
```

Pour utiliser un autre profil, `MECHE050_CONFIG` suffit : le lanceur lit automatiquement
`LOGS_DIR` et `PORTFOLIO_ENABLED_CONFIG` dans ce fichier, donc le journal de supervision, le PID et
la grille d'activation restent associes a la bonne instance.

```bash
MECHE050_CONFIG=configs/meche050_forward.env ./start_meche050.sh status
MECHE050_CONFIG=configs/meche050_forward.env ./start_meche050.sh restart
```

Chaque changement d'activation reecrit atomiquement la grille associee au profil et redemarre le
runner unique. Les ordres deja soumis restent persistes dans le `LOGS_DIR` du profil pour leur suivi.

Installer une nouvelle instance systemd depuis le dossier clone courant :

```bash
chmod +x install_systemd.sh
./install_systemd.sh rusty-poly-signal-runner-single
```

Options utiles :

```bash
./install_systemd.sh rusty-poly-signal-runner-single --rust-log info
./install_systemd.sh rusty-poly-signal-runner-single --no-start
```

Commandes de suivi :

```bash
sudo systemctl status rusty-poly-signal-runner-single --no-pager -l
./start_all.sh status
tail -f logs/supervisor/*.console.log
```

Mettre a jour le serveur, arreter les bots, pull Git puis relancer via systemd :

```bash
chmod +x server_update.sh
./server_update.sh
```

Par defaut, ce script utilise `/home/mehdi/rusty-poly-signal-runner` et le service
`rusty-poly-signal-runner`. Vous pouvez surcharger au besoin :

```bash
APP_DIR=/home/mehdi/rusty-poly-signal-runner SERVICE_NAME=rusty-poly-signal-runner ./server_update.sh
```

Resume des trades par strategie :

```bash
chmod +x trade_summary.sh
./trade_summary.sh
```

Variables utiles pour le script Linux :

```bash
CARGO_PROFILE=debug ./start_all.sh start
NO_RESTART=1 ./start_all.sh start
RESTART_DELAY_SECONDS=30 ./start_all.sh restart
```

## Logs et états

Chaque instance doit idéalement utiliser son propre `LOGS_DIR`.

Fichiers produits :

| Fichier | Rôle |
|---|---|
| `trades.csv` | Historique des signaux, ordres, latences et outcomes. |
| `pending_orders.json` | Ordres ouverts à suivre après restart. |
| `money_state.json` | État du money management. |
| `portfolio_state.json` | Ordres combines Meche persistes, incluant les soumissions en cours. |
| `portfolio_events.jsonl` | Journal structure des fenetres, tailles, ordres et reglements Meche. |
| `signals.jsonl` | Signaux individuels avant regroupement et sizing. |
| `session_metrics.jsonl` | Metriques permanentes de touch/fill a 0,50, profondeur, delais, prix et resultat. |
| `stats_summary.json` | Rapport agrege regenerable par strategie et par marche. |
| `stats/*.json` | Compteurs minimaux globaux, majoritaires et par strategie. |
| `trajectories/YYYY-MM-DD/*.jsonl.zst` | Trajectoires compactes Polymarket/Binance, conservees pour les backtests. |
| `trajectory_index.jsonl` | Index, checksum et taille de chaque trajectoire finalisee. |
| `stats/temporal/*.json` | Delais de passage strict sous 0,50 et contexte au signal. |
| `stats/risk/*.json` | MAE/MFE, drawdown et sorties hypothetiques apres entree. |
| `stream_cleanup.jsonl` | Audit des flux bruts supprimes apres validation des metriques. |

Les fichiers CSV et JSON runtime sous `logs/` sont ignorés par Git.

### Statistiques et espace disque du forward test

Le recorder forward reconstruit le carnet en memoire et ne conserve que les changements utiles
du meilleur bid/ask, la profondeur disponible a `0,50`, les trades et les evenements de cycle de
vie. A la fin de chaque session, il ecrit les metriques permanentes, compresse et verifie la
trajectoire, met a jour son index, puis seulement supprime le flux brut si
`PORTFOLIO_RECORDER_DELETE_STREAM_AFTER_SUMMARY=true`.
Apres un redemarrage, l'etat analytique est reconstruit depuis le flux compact et le sizing
durable. Si cette reprise ne peut pas etre validee integralement, la metrique est marquee
incomplete et le flux reste conserve jusqu'a un `backfill` reussi.

Pour convertir les anciennes sessions brutes, produire le rapport, puis recuperer l'espace :

```bash
chmod +x meche050_recorder_stats.sh
./meche050_recorder_stats.sh backfill
./meche050_recorder_stats.sh report
./meche050_recorder_stats.sh purge
./meche050_recorder_stats.sh purge --confirm
./meche050_recorder_stats.sh verify
./meche050_recorder_stats.sh repair-index
```

La premiere commande `purge` est toujours une simulation. La suppression exige `--confirm`,
ignore les sessions actives et refuse tout fichier situe hors de `logs/meche050-forward/streams`.
Lorsque `PORTFOLIO_RECORDER_PRESERVE_TRAJECTORIES=true`, le flux brut reste intact tant que sa
trajectoire compressee n'existe pas ou ne passe pas la verification d'integrite.
Les fichiers `signals.jsonl`, `signal_sizing.jsonl`, `sessions.jsonl`, `session_metrics.jsonl` et
`stats_summary.json` ne sont jamais supprimes.
La commande `purge` ne supprime pas les trajectoires compressees. `verify` valide leur taille,
checksum, decompression et nombre d'observations. `repair-index` reconstruit explicitement les
entrees manquantes apres validation; si une compression interrompue a laisse uniquement le flux
brut, il recree d'abord la trajectoire sans supprimer ce flux source.

Le script charge par defaut `configs/meche050_forward.env`, ce qui inscrit les hypotheses de frais,
slippage et taille minimale d'echantillon dans les rapports. Un autre fichier peut etre fourni avec
`MECHE050_STATS_CONFIG=/chemin/config.env`.

Le rapport distingue `immediate_fak_fills` (liquidite suffisante au moment du signal) et
`resting_limit_fills` (liquidite suffisante plus tard a `0,50`). La ligne `unique_orders` represente
le portefeuille reellement groupe; les lignes par strategie attribuent le resultat complet de
l'ordre a chaque strategie contributrice pour permettre leur comparaison.

La commande `report` regenere aussi `stats/global_all_signals.json`,
`stats/global_majority.json` et un fichier pour chacune des quatre strategies. Pour ces compteurs,
un passage sous `0,50` exige un meilleur ask strictement inferieur a `0,50`; un ask egal a `0,50`
ne compte pas. Le rapport majoritaire retient la direction ayant le plus de votes et compte les
egalites dans `trades_ignored_tie` sans creer de trade.

### Design valide : trajectoires compactes et statistiques temporelles

Cette extension enrichit uniquement l'observation du forward test dry-run. Elle ne modifie ni les
signaux, ni le sizing, ni les ordres, ni les sorties de position. Son objectif est de conserver un
jeu de donnees suffisamment precis pour rechercher et valider plus tard des filtres d'entree et des
regles de sortie sans biais temporel.

#### Collecte et stockage

Une seule trajectoire est conservee par marche et fenetre ayant au moins un signal, meme lorsque
plusieurs strategies votent. Elle reference tous les identifiants de signaux et leur direction. La
courte periode pre-signal disponible dans le recorder est ajoutee lors de l'activation, puis chaque
changement significatif est enregistre jusqu'a la resolution.
Un changement significatif correspond a la modification d'au moins un champ de marche liste
ci-dessous ; deux etats consecutifs identiques sont dedupliques sans echantillonnage temporel.

Chaque observation contient au minimum :

- l'horodatage, le temps depuis le signal et le temps restant avant resolution ;
- les meilleurs bid/ask, leurs tailles, le spread et la liquidite utile pour UP et DOWN ;
- les quantites achetables a `0,50` et les VWAP de revente pour 5 shares et pour le sizing candidat ;
- le dernier prix echange Polymarket ;
- le prix Binance, le prix d'ouverture de la fenetre et leur variation relative ;
- l'etat des connexions, les numeros de sequence, les reconnexions et les trous detectes.

Les fichiers finalises sont compresses et indexes :

```text
logs/meche050-forward/trajectories/YYYY-MM-DD/<session_id>.jsonl.zst
logs/meche050-forward/trajectory_index.jsonl
```

Une trajectoire active reste recuperable apres redemarrage. Sa finalisation ecrit d'abord un fichier
temporaire, valide sa decompression et son nombre d'observations, puis effectue un renommage atomique
avant d'ajouter une entree idempotente a l'index. `verify` detecte une entree d'index manquante et
`repair-index` la reconstruit explicitement, y compris si la compression a reussi avant une panne de
l'index. Les trajectoires sont conservees jusqu'a une purge manuelle ; aucune suppression
automatique n'est autorisee. La commande `verify` signale les fichiers absents, incomplets ou
corrompus ainsi que l'espace occupe.

#### Statistiques temporelles d'entree

Les six rapports minimaux existants gardent exactement leur schema. Six rapports separes sont
generes sous `stats/temporal/` pour `global_all_signals`, `global_majority` et les quatre strategies.
Chaque rapport fournit une vue globale et des ventilations par marche, direction et resultat.

Le temps d'entree commence a la detection du signal et se termine a la premiere observation dont le
meilleur ask est strictement inferieur a `0,50`. Une observation egale a `0,50` ne constitue jamais
un passage. Les rapports contiennent :

- signaux analysables, passages, non-passages et passages immediats ;
- moyenne, mediane, minimum, maximum et percentiles P25, P75, P90 et P95 ;
- passages avant 15 s, 30 s, 60 s, 120 s, 180 s et 300 s ;
- distributions separees pour les gains et les pertes ;
- ask, spread, profondeur et distance a `0,50` au moment du signal.

Le rapport majoritaire applique la regle existante : UP si les votes UP sont plus nombreux, DOWN si
les votes DOWN sont plus nombreux, et egalite ignoree avec compteur separe.

#### Statistiques de risque apres entree

Le premier passage strict sous `0,50` definit une entree hypothetique a `0,50`. Les six rapports sous
`stats/risk/` observent ensuite la position a 15 s, 30 s, 60 s, 120 s, 180 s et 300 s apres cette
entree. Un horizon situe apres la resolution est compte comme indisponible et n'est pas remplace par
la derniere observation. Le prix de sortie est fonde sur le bid executable, jamais sur l'ask.

Les rapports incluent :

- meilleur bid, spread, profondeur, VWAP de sortie et PnL hypothetique pour 5 shares ;
- mouvement Binance signe dans le sens de la prediction et temps restant ;
- MAE, MFE, drawdown depuis le meilleur prix et leurs horodatages ;
- premier passage et duree sous `0,45`, `0,40`, `0,35` et `0,30` ;
- premier passage au-dessus de `0,55`, `0,60`, `0,65` et `0,70` ;
- recuperations apres baisse, pertes evitees et gains sacrifies par sortie hypothetique ;
- EV de conservation jusqu'a resolution, EV de sortie au bid et difference entre les deux, en brut
  et en net avec les hypotheses de frais et de slippage inscrites dans chaque rapport.

Chaque groupe publie sa taille d'echantillon, son taux de perte et un intervalle de confiance. Les
resultats sont ventiles par strategie, marche, direction et nombre de votes concordants. Par defaut,
un groupe de moins de 30 observations completes produit un avertissement et ne devient jamais une
recommandation ; le seuil utilise est inscrit dans chaque rapport.

#### Qualite, anti-lookahead et tests

Une session incomplete ou contenant un trou de donnees n'est jamais classee silencieusement comme
un non-passage. Elle est conservee mais exclue des distributions principales et comptee dans une
categorie de qualite distincte. Les futures simulations de sortie n'utiliseront que les informations
disponibles a l'instant de la decision ; le resultat final sert uniquement d'etiquette. Toute regle
candidate devra ensuite etre validee chronologiquement sur une periode hors echantillon, avec EV
nette et drawdown comme criteres principaux.

Les tests couvrent le seuil strict (`0,49` accepte, `0,50` refuse), les percentiles, les horizons
relatifs au fill, le partage d'une trajectoire entre strategies, les trous de donnees, la profondeur,
la reprise apres redemarrage, la compression, l'index et la finalisation atomique. Les rapports sont
deterministes et ecrits atomiquement.

## Reconciliation officielle Polymarket

Le bot valide les trades en live avec la bougie Binance cible afin de continuer a trader sans attendre la resolution officielle Polymarket. Cette validation est rapide, mais elle reste une estimation operationnelle: les marches Up/Down Polymarket se resolvent selon la source indiquee dans leurs regles, souvent Chainlink.

Le binaire `reconcile_outcomes` sert d'audit quotidien. Il lit `trades.csv`, extrait les slugs `btc-updown-*` et `eth-updown-*`, recupere le marche via Gamma, puis recupere le token gagnant officiel via le CLOB Polymarket. Il ecrit ensuite un rapport append-only dans `reconciliation_report.csv`.

Le script ne modifie pas `trades.csv` et ne change pas `money_state.json`. Il signale seulement les ecarts entre le resultat Binance utilise en live et le resultat officiel Polymarket.

Colonnes principales du rapport :

| Colonne | Role |
|---|---|
| `prediction` | Prediction du bot (`UP` ou `DOWN`). |
| `binance_outcome` | Resultat enregistre en live dans `trades.csv`. |
| `official_winner` | Outcome gagnant selon Polymarket (`UP` ou `DOWN`). |
| `official_outcome` | Resultat officiel calcule pour notre prediction. |
| `reconciliation` | `MATCH`, `MISMATCH`, `PENDING` ou `ERROR`. |

Execution Windows PowerShell :

```powershell
.\reconcile_outcomes.ps1
.\reconcile_outcomes.ps1 configs/eth_ensemble.env
.\reconcile_outcomes.ps1 configs/btc_ensemble.env -Release
```

Execution Linux/macOS :

```bash
chmod +x ./reconcile_outcomes.sh
./reconcile_outcomes.sh
./reconcile_outcomes.sh configs/eth_ensemble.env
RELEASE=1 ./reconcile_outcomes.sh configs/btc_ensemble.env
```

Execution directe Cargo :

```bash
STRATEGY_CONFIG=configs/btc_ensemble.env cargo run --locked --bin reconcile_outcomes
```

## Tests et qualité

```bash
cargo fmt -- --check
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

## Ajouter une stratégie

1. Créer un fichier dans `src/strategies/`.
2. Implémenter le trait `Strategy`.
3. Exporter le module dans `src/strategies/mod.rs`.
4. Ajouter le mapping dans `create_strategy()` dans `src/main.rs`.
5. Ajouter des tests ciblés ou une fixture de bougies.

Les indicateurs partagés pour les stratégies ensemble vivent dans `src/strategies/indicators.rs`.
