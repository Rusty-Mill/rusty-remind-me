# Backup and restore runbook

How to back up the remind-me store and how to restore it. The drill steps below passed on the remindme host on 2026-11-26 (Mill Board #1622, pack D36, D73). The backup `manual-20261126T180134011097Z.engine` restored into a scratch folder and matched the live store exactly: 72,765 records (25,753 live, 47,012 deleted), 0 mismatches, 0 drift.

## What a backup holds

`rusty-remind-me admin backup` copies the engine directory, `memory.engine/`, while the tables are held. The copy goes to `backups/manual-<timestamp>.engine` beside the database file.

- It includes every memory, deleted ones too, and the revision files.
- **It keeps only the newest `BACKUP_RETENTION_COUNT` (10) backups.** Taking a backup deletes the oldest one beyond that. Keep a cold copy outside `backups/` before any risky change, as the deploy notes do.
- It does **not** include:
  - the raw-transcript archive blob root (`REMIND_ME_ARCHIVE_DIR`, pack Q20; unset on remindme as of 2026-11-26)
  - `oauth.json` (connector logins), `api_keys.json` and `connector_token`
  - `wiki/`
  - the env files (`~/.config/remind-me/node.env` and friends)
  - the stale pre-engine `memory.db`

  Copy those separately if you need them back.
- The dialog store gets its own `.dialogs` package once #528 wires it in (D78).
- `admin export-memories` (JSON or JSONL) is a portable second copy, but it is not a restore path. Re-importing it re-chunks the content and assigns new ids.

## Restore the live store

1. `sudo systemctl stop remind-me-ui remind-me-connector remind-me-store`
2. Move `~/.remind-me/memory.engine` aside. Don't delete it.
3. `cp -a ~/.remind-me/backups/<chosen>.engine ~/.remind-me/memory.engine`
4. `sudo systemctl start remind-me-store remind-me-connector remind-me-ui`. The store's readiness wait makes starting all three together safe.
5. Check `remind_me_server_status`.

## Drill: restore into a scratch copy

This never touches the live store or the services.

1. Make a folder: `D=~/restore-drill/$(date -u +%Y%m%dT%H%M%SZ); mkdir -p "$D/store"`.
2. With the node env sourced, run `rusty-remind-me admin backup`. Record the path and the time.
3. Right after the backup, with the same env, run `rusty-remind-me impact-export > "$D/live.json"`.
4. Copy the backup: `cp -a <backup>.engine "$D/store/memory.engine"`.
5. In a fresh shell without the node env, set `REMIND_ME_DB_PATH="$D/store/memory.db"`. Run `rusty-remind-me daemon start` for that path, then `rusty-remind-me impact-export > "$D/restored.json"`, then stop that daemon. Its `.daemon.json` sits under `$D/store/`, apart from the live one.
6. Compare the `records` arrays of the two exports. Check totals; counts by category, `memory_type` and project; and, per id, the content sha256, category, type, project, `deleted_at` and `superseded_by`. Ignore vitality and access fields. A record created or updated after the backup time is drift, not a failure.
7. Pass means every non-drift id matches, the counts agree, and the live units still have the same PIDs.

The export doesn't include revision history, so revisions are checked only as files: the copy is byte-identical to the backup.

Delete `$D` when you're done. A drill copy is about 745 MB.
