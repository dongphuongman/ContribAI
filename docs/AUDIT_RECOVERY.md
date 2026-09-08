# Admission audit recovery

An admission append verifies the complete retained ledger and writes the new receipt in one
SQLite write transaction. Invalid receipt hashes, broken predecessor links, inconsistent indexed
fields, malformed payloads, and unsupported schemas prevent new records from being committed.
An approved submission cannot proceed when that append fails. Existing rows are preserved.

This guard validates retained history. It cannot detect a deleted suffix or an entirely replaced
database without an independently retained receipt. It does not establish reviewer identity.

## Inspect without submitting

Use the same configuration as the affected process:

```bash
contribai --config config.yaml admissions --json
```

The command verifies the complete chain even when filters or a listing limit are supplied. An
integrity failure returns a non-zero exit status. A malformed payload can prevent JSON output;
its stored values are intentionally omitted from error messages. Do not interpret missing output
as an empty, healthy ledger.

Confirm `storage.db_path` in the configuration. The default is `~/.contribai/memory.db`. Opening a
different or nonexistent database can produce a valid empty ledger, which is not proof that the
original history is intact. The same database also holds analysis and other operator state.

## Preserve evidence

1. Stop ContribAI CLI runs, web servers, and schedulers that use this database.
2. Preserve a copy of the database and any remaining `-wal` and `-shm` sidecars after writers have
   stopped. Keep the originals. A raw copy taken while writers are active is not a reliable backup.
3. Retain the command's exit status and sanitized error, the ContribAI version, and the last
   independently saved receipt. Avoid posting the database or its repository metadata publicly.

Do not delete failing rows, recompute historical hashes, or point the process at a new empty
database merely to obtain a successful check. Those actions destroy or conceal the evidence.

## Restore and verify

Restore a known-good consistent backup to a separate path. Make an operator configuration that
points `storage.db_path` at that restored copy, then run:

```bash
contribai --config recovery.yaml admissions --limit 1 --json
```

Require a successful exit and compare the newest full receipt in `records[0].receipt` with the
independently retained receipt for that backup. A valid chain alone cannot distinguish an intact
database from one rolled back to an earlier valid prefix. Account for any decisions made after the
backup before resuming submissions. Preserve both the damaged database and restoration evidence.

If no trustworthy backup or independent evidence is available, keep external writes disabled and
investigate the cause with the project maintainers. ContribAI does not automatically repair or
reset admission history. After restoration, run admission again against current maintainer consent
and a fresh permit; do not reuse a previously approved candidate's expired evidence.

## Operational limits

Verification reads one retained receipt at a time; it does not collect the full chain in memory.
Append latency grows with retained history. Writers serialize through SQLite's write transaction.
A database busy or I/O error fails closed. There is no retention cleanup that silently removes
receipts to make the check faster.
