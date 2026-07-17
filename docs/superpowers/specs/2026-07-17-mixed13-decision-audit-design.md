# Mixed_13 decision audit design

## Goal

Make every ETHUSD_PERP COIN-M `mixed_13` decision independently verifiable after the fact, even when Binance later returns different open-interest history. The runner must record the exact causal inputs used at decision time, rather than trying to reconstruct them from a later Binance response.

## Scope

- Applies only to strategies that require a microstructure snapshot; initially this is `ethusd_perp_coinm_15m_microstructure_mixed_13`.
- Records every closed 15-minute decision: `UP`, `DOWN`, `SKIP`, and collection errors.
- Does not alter signal rules, Polymarket order placement, money management, or the existing `trades.csv` schema.
- Corrects the shipped `mixed_13` configuration spelling from `EXECUTION_MODE=dry_run` to `EXECUTION_MODE=dry-run`, so the runner and audit command work without a process-level override.
- Never writes secrets, private keys, credentials, or API responses unrelated to the decision.

## Audit journal

The runner appends one JSON object per line to:

```text
<LOGS_DIR>/microstructure_decisions.jsonl
```

Each successful-snapshot record has schema version `1` and contains:

- runner write time and Binance `observed_at` time;
- strategy name, target candle open/close times, and target candle OHLC;
- the 39 float32-rounded feature values used by the rules;
- a feature-source timestamp for each value, proving it was not later than the decision time;
- typed decision outcome (`UP`, `DOWN`, or `SKIP`), green/red vote totals, and active frozen rule names;
- the derived Polymarket slug for the next 15-minute market;
- a deterministic record hash chained to the prior record hash.

A collection failure writes a record with `status=COLLECTION_ERROR`, runner time, strategy name, and a sanitized error message. It has no feature map and cannot produce an order.

The record is written and flushed before `process_signal_for_candle` can resolve a market or place an order. Audit I/O failure fails closed: the decision is logged as an error and no order is sent. This preserves auditability over availability.

## Data flow

1. The collector builds a causal snapshot and retains the Binance observation time plus per-feature source times.
2. `mixed_13` evaluates the snapshot and exposes a typed decision summary: votes, active rules, and outcome.
3. The runtime derives the Polymarket slug, appends the JSONL audit record, then continues through the existing signal/order path.
4. A standalone audit command reads only the JSONL file, re-evaluates the frozen rules against the stored feature values, verifies source-time causality and hash chaining, then prints counts and any divergences. It never calls Binance or Polymarket.

## Interfaces

- `MicrostructureSnapshot` gains immutable metadata for `observed_at` and per-feature source times.
- `Strategy` gains an optional typed last-decision summary method. Existing strategies retain the default `None` implementation.
- A dedicated audit writer owns append/flush/hash-chain behavior in the existing logging layer.
- `audit_microstructure_decisions` is a read-only binary accepting `--config <path>`; it reports total decisions, `UP`, `DOWN`, `SKIP`, collection errors, and rule/causality/hash divergences.

## Failure handling

- Missing feature, source time after decision time, invalid JSON, a broken hash chain, or a re-evaluated decision mismatch is reported as an audit failure.
- A live collection error is independently journaled; it must not result in a trade.
- The audit command exits non-zero when it finds an invalid record, making it suitable for a server check or CI.

## Verification

Tests cover:

1. stable JSONL serialization, chained hashes, and append behavior;
2. all 39 stored features and source times for a complete snapshot;
3. frozen fixture parity: stored values re-evaluate to the recorded decision and votes;
4. rejection of a source timestamp later than the decision time;
5. rejection of a tampered record or mismatched decision;
6. runtime ordering: the audit record is persisted before any market resolution/order call.
7. the shipped `mixed_13` configuration parses with no `EXECUTION_MODE` override.

## Operational use

After deploying the updated runner, the operator runs:

```bash
cargo run --release --locked --bin audit_microstructure_decisions -- \
  --config configs/ethusd_perp_coinm_15m_microstructure_mixed_13.env
```

The command validates the server's recorded decision inputs directly. It will therefore explain a threshold-sensitive OI signal using the value actually seen by the runner, without relying on a later REST response or a Binance Vision archive.
