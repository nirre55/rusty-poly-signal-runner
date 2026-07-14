# Design: strategie ETHUSD_PERP microstructure mixed_13

## Objectif et perimetre

Integrer dans `rusty-poly-signal-runner` la strategie figee
`ethusd_perp_coinm_15m_microstructure_mixed_13`. Elle doit produire une
prediction pour la prochaine bougie brute ETHUSD_PERP COIN-M de 15 minutes,
apres la cloture de la bougie signal.

Le seul artefact de regles faisant foi est
`crypto-up-down/codex/results/workflows/BINANCE_MICROSTRUCTURE_ETHUSD_PERP_JULY_2026_HOLDOUT/ETHUSD_PERP_COINM_15M/ethusd_perp_coinm_15m_microstructure_mixed_13/implementation_rules.json`.

`mixed_1` est explicitement hors perimetre. Aucun module, config, nom de
factory, documentation runnable ou lancement par defaut ne sera ajoute pour
elle.

La strategie sera initialement fournie en `dry_run` et ne sera pas ajoutee a
`start_all.ps1` ni a `start_all.sh`.

## Architecture

Le runner conserve son parcours actuel pour toutes les strategies basees sur
une seule serie de bougies. Un chemin microstructure isole sera ajoute pour
`mixed_13` :

1. `MicrostructureCollector` precharge les series et maintient les buffers de
   marche necessaires.
2. A la cloture de chaque bougie cible COIN-M 15 minutes, il fabrique un
   `MicrostructureSnapshot` immuable, aligne sur ce temps de decision.
3. La strategie evaluate les regles statiques contre le snapshot et retourne
   un `Signal` seulement lorsque sa decision exclusive est definie.
4. Le runtime existant continue de traiter le signal, le marche Polymarket et
   les ordres. Il ne connait pas les details de calcul des variables.

Le trait `Strategy` sera etendu par des methodes avec implementation par
defaut pour declarer le besoin en microstructure et evaluer un snapshot. Les
strategies existantes gardent leur parcours `on_closed_candle` inchange.

## Sources, alignement et variables

Le collecteur utilisera les endpoints Binance officiels associes au registre
frozen :

- COIN-M `ETHUSD_PERP`, klines 1m et 15m ;
- USD-M `ETHUSDT` et `BTCUSDT`, klines 1m et 15m ;
- USD-M mark-price klines `ETHUSDT` et `BTCUSDT` ;
- USD-M index-price klines `ETHUSDT` ;
- historique USD-M d'open interest `ETHUSDT`.

Les flux live maintiennent les bougies fermees et les valeurs periodiques ;
une reconnexion reconstitue tout intervalle manque par REST avant de permettre
une nouvelle decision. Le prechargement apporte le maximum de recul requis
par les indicateurs techniques et les retards d'open interest.

Chaque valeur est associee a l'horodatage de la bougie signal. Le snapshot ne
peut employer que des observations disponibles a cet instant, jamais une
observation posterieure. Les variables reproduisent les calculs Python figes :

- indicateurs COIN-M signal : retour, ratios de bougies vertes et de
  transitions, RSI 7/14, EMA 8/21 normalisees par ATR, stochastique 14 et
  breakout haut 20 ;
- agregats 1m cible/futures : retour du bloc, position de cloture, nombre de
  bougies vertes, retours minute retardes, retours et imbalance taker par
  tiers, accelerations de retour et de taker ;
- derivees 15m : retours et positions de cloture mark/index ;
- open interest ETH : variations de quantite et de valeur sur les horizons
  6, 12 et 24.

Les 39 variables uniques citees dans les 25 regles seront representees par un enum
type. Cette representation interdit les fautes de nom et permet de rendre les
regles et leurs seuils directement auditables.

## Evaluation de `mixed_13`

Les 25 regles resteront des donnees statiques avec leur nom d'origine, leur
vote et toutes leurs conditions. Une condition utilise un operateur `<=`,
`>=` ou `==` et un seuil `f64` du JSON fige.

Pour un snapshot complet :

- toutes les regles GREEN actives comptent comme votes GREEN ;
- toutes les regles RED actives comptent comme votes RED ;
- GREEN est emis seulement si au moins une GREEN est active et aucune RED ne
  l'est ;
- RED est emis seulement si au moins une RED est active et aucune GREEN ne
  l'est ;
- dans tout autre cas, aucune prediction n'est emise.

Ce comportement realise `EXCLUSIVE_ONLY`; il ne transforme pas les votes en
majorite. Les logs de bougie indiqueront le nombre de regles GREEN, RED, le
total et les noms des regles actives.

Une valeur manquante, un flux hors alignement, un retard non rattrape ou une
erreur de calcul invalide le snapshot entier pour cette decision. Le runner
loguera la raison et n'emettra pas de signal. Aucune approximation, valeur
neutre ou repli vers les donnees spot existantes n'est autorise.

## Integration et configuration

La factory enregistrera exclusivement
`ethusd_perp_coinm_15m_microstructure_mixed_13`. Le module de strategie,
le collecteur et le modele de snapshot seront ajoutes sans modifier les noms
ou le comportement des strategies existantes.

La configuration `configs/ethusd_perp_coinm_15m_microstructure_mixed_13.env`
utilisera :

- `STRATEGY=ethusd_perp_coinm_15m_microstructure_mixed_13` ;
- une cible Polymarket ETH 15m ;
- `EXECUTION_MODE=dry_run` ;
- `ENSEMBLE_MIN_VOTES=1` pour rester compatible avec la configuration commune
  (la decision exclusive reste definie par les regles) ;
- un repertoire de logs propre.

Le README documentera le statut experimental, les sources et la commande de
lancement directe. Le bot ne sera pas demarre par les listes de lancement
globales.

## Tests et validation

La validation couvre quatre niveaux :

1. tests unitaires des buffers, formules de variables, alignement temporel,
   indisponibilite de donnees et logique exclusive ;
2. test de factory pour le nouveau nom de strategie ;
3. fixture issue de la reference figee de 1 248 lignes, comparant toutes les
   variables requises et les decisions `mixed_13` dans les tolerances publiees
   (relative `1e-6`, absolue `1e-5`) ;
4. les validations du projet : `cargo fmt`, tests verrouilles, Clippy sans
   avertissement, validation PowerShell de `start_all.ps1` et analyse
   syntaxique de `start_all.sh` lorsque Bash est disponible.

Le succes de compilation et de parite demontre l'integration et la
reproduction des fixtures. Il ne valide pas un avantage de trading en reel ;
la configuration restera donc en `dry_run` jusqu'a la validation forward
operationnelle demandee par le registre frozen.
