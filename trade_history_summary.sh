#!/usr/bin/env bash
set -euo pipefail

ROOT="${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
LOGS_DIR="${1:-$ROOT/logs}"

if [[ ! -d "$LOGS_DIR" ]]; then
  echo "Logs directory not found: $LOGS_DIR" >&2
  exit 1
fi

python3 - "$LOGS_DIR" <<'PY'
import csv
import glob
import os
import sys
from datetime import datetime, timezone


def parse_utc(value):
    value = (value or "").strip()
    if not value:
        return None

    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None

    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def format_utc(value):
    return value.strftime("%Y-%m-%d %H:%M:%S UTC")


def format_elapsed(total_seconds):
    total_seconds = max(0, int(total_seconds))
    days, remainder = divmod(total_seconds, 86400)
    hours, remainder = divmod(remainder, 3600)
    minutes, seconds = divmod(remainder, 60)
    return f"{days} jours, {hours} heures, {minutes} minutes, {seconds} secondes"


logs_dir = os.path.abspath(sys.argv[1])
files = sorted(glob.glob(os.path.join(logs_dir, "*", "trades.csv")))

if not files:
    print(f"No trades.csv files found under {logs_dir}/*/trades.csv")
    raise SystemExit(0)

first_trade = None
last_execution = None

for path in files:
    strategy = os.path.basename(os.path.dirname(path))

    try:
        with open(path, "r", encoding="utf-8-sig", newline="") as handle:
            reader = csv.DictReader(handle)
            if not reader.fieldnames:
                continue

            for row in reader:
                opened_at = parse_utc(row.get("target_candle_open_time_utc"))
                if opened_at is None:
                    continue

                candidate = {
                    "opened_at": opened_at,
                    "strategy": strategy,
                    "price": (row.get("execution_price") or "").strip(),
                    "shares": (row.get("size_matched") or "").strip(),
                }

                if first_trade is None or opened_at < first_trade["opened_at"]:
                    first_trade = candidate

                if candidate["price"]:
                    try:
                        float(candidate["price"])
                    except ValueError:
                        continue

                    if (
                        last_execution is None
                        or opened_at > last_execution["opened_at"]
                    ):
                        last_execution = candidate
    except (OSError, csv.Error) as error:
        print(f"Warning: unable to read {path}: {error}", file=sys.stderr)

if first_trade is None:
    print("No valid trade rows found.")
    raise SystemExit(0)

now = datetime.now(timezone.utc)

print("Global trade history")
print("====================")
print(f"Premier trade : {format_utc(first_trade['opened_at'])}")
print(f"Temps ecoule  : {format_elapsed((now - first_trade['opened_at']).total_seconds())}")
print()

if last_execution is None:
    print("Dernier trade execute : aucun prix d'execution trouve")
    raise SystemExit(0)

shares = last_execution["shares"] or "n/a"
print("Dernier trade execute")
print("---------------------")
print(f"Strategie     : {last_execution['strategy']}")
print(f"Date          : {format_utc(last_execution['opened_at'])}")
print(f"Prix execute  : {last_execution['price']}")
print(f"Shares achetes: {shares}")
PY
