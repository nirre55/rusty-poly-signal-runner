# Global Trade History Summary

## Goal

Add a standalone script that scans every `logs/*/trades.csv` file in one
repository and prints a global history summary.

The script is named `trade_history_summary.sh` and lives at the repository
root, next to `trade_summary.sh`.

## Input

- Default input directory: `<repo>/logs`
- Optional first argument: another logs directory
- Files considered: `<logs-dir>/*/trades.csv`
- CSV files may use either the current schema or an older schema without
  `execution_price` and `size_matched`

## Output

The script prints:

1. The UTC opening date of the earliest trade across the repository.
2. The elapsed time from that date to the current time, expressed in days,
   hours, minutes, and seconds.
3. The most recent executed trade across the repository, including:
   - strategy directory name
   - UTC opening date
   - execution price
   - matched shares

## Selection Rules

- A trade date comes from `target_candle_open_time_utc`.
- Rows with a missing or invalid trade date are ignored.
- The first trade is the valid row with the earliest trade date.
- An executed trade must have a valid, non-empty `execution_price`.
- The latest executed trade is the qualifying row with the greatest trade
  date.
- `size_matched` is displayed when available; otherwise, the script prints
  `n/a`.
- Older CSV files without execution columns remain eligible for determining
  the first trade but cannot qualify as the latest executed trade.

## Implementation

Use a Bash entrypoint with an embedded Python 3 program. Python's standard
`csv` and `datetime` modules provide reliable CSV parsing, ISO-8601 date
handling, and elapsed-time calculation without adding project dependencies.

The script exits with an error for a missing logs directory. It exits
successfully with an explanatory message when no trade rows or no executed
trade can be found.

## Verification

Run the script against the repository's current mixed-schema log files and
verify:

- the earliest known trade is selected globally;
- old CSV files do not cause parsing failures;
- an absent executed trade produces a clear message;
- supplied execution price and matched shares retain their numeric values.
