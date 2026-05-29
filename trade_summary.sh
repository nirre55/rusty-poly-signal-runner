#!/usr/bin/env bash
set -euo pipefail

ROOT="${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
LOGS_DIR="${1:-$ROOT/logs}"

if [[ ! -d "$LOGS_DIR" ]]; then
  echo "Logs directory not found: $LOGS_DIR" >&2
  exit 1
fi

shopt -s nullglob
files=("$LOGS_DIR"/*/trades.csv)

if [[ "${#files[@]}" -eq 0 ]]; then
  echo "No trades.csv files found under $LOGS_DIR/*/trades.csv"
  exit 0
fi

printf "%-24s %7s %7s %7s %9s %9s %9s\n" "strategy" "total" "win" "loss" "pending" "no_entry" "other"
printf "%-24s %7s %7s %7s %9s %9s %9s\n" "--------" "-----" "---" "----" "-------" "--------" "-----"

grand_total=0
grand_win=0
grand_loss=0
grand_pending=0
grand_no_entry=0
grand_other=0

for file in "${files[@]}"; do
  strategy="$(basename "$(dirname "$file")")"
  stats="$(
    awk -F',' '
      NR == 1 {
        outcome_col = 0
        for (i = 1; i <= NF; i++) {
          gsub(/^"|"$/, "", $i)
          if (tolower($i) == "outcome") {
            outcome_col = i
          }
        }
        next
      }
      outcome_col == 0 || NF == 0 { next }
      {
        outcome = toupper($outcome_col)
        gsub(/^"|"$/, "", outcome)
        total++
        if (outcome == "WIN") win++
        else if (outcome == "LOSS") loss++
        else if (outcome == "PENDING" || outcome == "") pending++
        else if (outcome == "NO_ENTRY") no_entry++
        else other++
      }
      END {
        printf "%d %d %d %d %d %d", total + 0, win + 0, loss + 0, pending + 0, no_entry + 0, other + 0
      }
    ' "$file"
  )"
  read -r total win loss pending no_entry other <<<"$stats"

  grand_total=$((grand_total + total))
  grand_win=$((grand_win + win))
  grand_loss=$((grand_loss + loss))
  grand_pending=$((grand_pending + pending))
  grand_no_entry=$((grand_no_entry + no_entry))
  grand_other=$((grand_other + other))

  printf "%-24s %7d %7d %7d %9d %9d %9d\n" \
    "$strategy" "$total" "$win" "$loss" "$pending" "$no_entry" "$other"
done

printf "%-24s %7s %7s %7s %9s %9s %9s\n" "--------" "-----" "---" "----" "-------" "--------" "-----"
printf "%-24s %7d %7d %7d %9d %9d %9d\n" \
  "TOTAL" "$grand_total" "$grand_win" "$grand_loss" "$grand_pending" "$grand_no_entry" "$grand_other"
