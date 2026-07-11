# Prix limite fixe pour BTC combined

## Objectif

Configurer la stratégie `btc_5m_rules_23_min_votes_1` pour soumettre des ordres
limite à un prix fixe de `0.50` au lieu de calculer le prix à partir du carnet
avec une entrée de marché.

## Portée

- Modifier uniquement `configs/btc_combined.env`.
- Conserver `EXECUTION_MODE=limit`.
- Ajouter `LIMIT_PRICE_FIXED=0.50`.
- Conserver les paramètres de référence, d’offset et de garde-fou existants
  pour permettre un retour simple au comportement dynamique.

## Comportement attendu

Lorsque `LIMIT_PRICE_FIXED` est défini, le chargeur de configuration et le
client Polymarket utilisent `0.50` comme prix limite et ignorent la référence
`best_ask`, l’offset et le garde-fou de prix limite.

## Validation

- Vérifier que le fichier contient bien `EXECUTION_MODE=limit` et
  `LIMIT_PRICE_FIXED=0.50`.
- Exécuter les tests de configuration et de soumission d’ordres limite.

