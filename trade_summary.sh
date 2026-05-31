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

printf "%-24s %7s %7s %7s %8s %9s %9s %9s %11s %11s %9s %9s\n" \
  "strategy" "total" "win" "loss" "winrate" "pending" "no_entry" "other" "avg_win_px" "avg_loss_px" "rr_real" "rr_be"
printf "%-24s %7s %7s %7s %8s %9s %9s %9s %11s %11s %9s %9s\n" \
  "--------" "-----" "---" "----" "-------" "-------" "--------" "-----" "----------" "-----------" "-------" "-----"

grand_total=0
grand_win=0
grand_loss=0
grand_pending=0
grand_no_entry=0
grand_other=0
grand_win_price_sum="0"
grand_loss_price_sum="0"
grand_win_price_count=0
grand_loss_price_count=0

for file in "${files[@]}"; do
  strategy="$(basename "$(dirname "$file")")"
  stats="$(
    awk -F',' '
      NR == 1 {
        outcome_col = 0
        price_col = 0
        for (i = 1; i <= NF; i++) {
          gsub(/^"|"$/, "", $i)
          header = tolower($i)
          if (header == "outcome") {
            outcome_col = i
          } else if (header == "execution_price" || header == "entry_price") {
            price_col = i
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

        if (price_col > 0 && $price_col ~ /^([0-9]+(\.[0-9]+)?|\.[0-9]+)$/) {
          price = $price_col + 0
          if (price > 0 && price < 1) {
            if (outcome == "WIN") {
              win_price_sum += price
              win_price_count++
            } else if (outcome == "LOSS") {
              loss_price_sum += price
              loss_price_count++
            }
          }
        }
      }
      END {
        closed = win + loss
        winrate = closed > 0 ? win / closed * 100 : -1
        avg_win_price = win_price_count > 0 ? win_price_sum / win_price_count : -1
        avg_loss_price = loss_price_count > 0 ? loss_price_sum / loss_price_count : -1
        rr_real = (avg_win_price > 0 && avg_loss_price > 0) ? (1 - avg_win_price) / avg_loss_price : -1
        rr_be = win > 0 ? loss / win : -1
        printf "%d %d %d %d %d %d %.4f %.6f %.6f %.6f %.6f %.6f %.6f %d %d", \
          total + 0, win + 0, loss + 0, pending + 0, no_entry + 0, other + 0, \
          winrate, avg_win_price, avg_loss_price, rr_real, rr_be, \
          win_price_sum + 0, loss_price_sum + 0, \
          win_price_count + 0, loss_price_count + 0
      }
    ' "$file"
  )"
  read -r total win loss pending no_entry other winrate avg_win_price avg_loss_price rr_real rr_be win_price_sum loss_price_sum win_price_count loss_price_count <<<"$stats"

  grand_total=$((grand_total + total))
  grand_win=$((grand_win + win))
  grand_loss=$((grand_loss + loss))
  grand_pending=$((grand_pending + pending))
  grand_no_entry=$((grand_no_entry + no_entry))
  grand_other=$((grand_other + other))
  grand_win_price_sum="$(awk -v a="$grand_win_price_sum" -v b="$win_price_sum" 'BEGIN { printf "%.6f", a + b }')"
  grand_loss_price_sum="$(awk -v a="$grand_loss_price_sum" -v b="$loss_price_sum" 'BEGIN { printf "%.6f", a + b }')"
  grand_win_price_count=$((grand_win_price_count + win_price_count))
  grand_loss_price_count=$((grand_loss_price_count + loss_price_count))

  winrate_text="$(awk -v v="$winrate" 'BEGIN { if (v < 0) print "n/a"; else printf "%.1f%%", v }')"
  avg_win_price_text="$(awk -v v="$avg_win_price" 'BEGIN { if (v < 0) print "n/a"; else printf "%.4f", v }')"
  avg_loss_price_text="$(awk -v v="$avg_loss_price" 'BEGIN { if (v < 0) print "n/a"; else printf "%.4f", v }')"
  rr_real_text="$(awk -v v="$rr_real" 'BEGIN { if (v < 0) print "n/a"; else printf "%.2f", v }')"
  rr_be_text="$(awk -v v="$rr_be" 'BEGIN { if (v < 0) print "n/a"; else printf "%.2f", v }')"

  printf "%-24s %7d %7d %7d %8s %9d %9d %9d %11s %11s %9s %9s\n" \
    "$strategy" "$total" "$win" "$loss" "$winrate_text" "$pending" "$no_entry" "$other" \
    "$avg_win_price_text" "$avg_loss_price_text" "$rr_real_text" "$rr_be_text"
done

grand_closed=$((grand_win + grand_loss))
grand_winrate_text="$(awk -v w="$grand_win" -v c="$grand_closed" 'BEGIN { if (c == 0) print "n/a"; else printf "%.1f%%", w / c * 100 }')"
grand_avg_win_price_text="$(awk -v s="$grand_win_price_sum" -v c="$grand_win_price_count" 'BEGIN { if (c == 0) print "n/a"; else printf "%.4f", s / c }')"
grand_avg_loss_price_text="$(awk -v s="$grand_loss_price_sum" -v c="$grand_loss_price_count" 'BEGIN { if (c == 0) print "n/a"; else printf "%.4f", s / c }')"
grand_rr_real_text="$(awk -v ws="$grand_win_price_sum" -v wc="$grand_win_price_count" -v ls="$grand_loss_price_sum" -v lc="$grand_loss_price_count" 'BEGIN { if (wc == 0 || lc == 0 || ls == 0) print "n/a"; else printf "%.2f", (1 - (ws / wc)) / (ls / lc) }')"
grand_rr_be_text="$(awk -v w="$grand_win" -v l="$grand_loss" 'BEGIN { if (w == 0) print "n/a"; else printf "%.2f", l / w }')"

printf "%-24s %7s %7s %7s %8s %9s %9s %9s %11s %11s %9s %9s\n" \
  "--------" "-----" "---" "----" "-------" "-------" "--------" "-----" "----------" "-----------" "-------" "-----"
printf "%-24s %7d %7d %7d %8s %9d %9d %9d %11s %11s %9s %9s\n" \
  "TOTAL" "$grand_total" "$grand_win" "$grand_loss" "$grand_winrate_text" \
  "$grand_pending" "$grand_no_entry" "$grand_other" "$grand_avg_win_price_text" \
  "$grand_avg_loss_price_text" "$grand_rr_real_text" "$grand_rr_be_text"

echo
echo "Notes:"
echo "- winrate = WIN / (WIN + LOSS), PENDING/NO_ENTRY/other exclus."
echo "- avg_win_px / avg_loss_px = prix d'entree moyen des trades WIN/LOSS."
echo "- rr_be = LOSS / WIN; c'est le RR minimal pour etre breakeven avec ce winrate."
echo "- rr_real = calcule seulement si trades.csv contient execution_price ou entry_price."
