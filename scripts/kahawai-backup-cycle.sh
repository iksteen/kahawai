#!/usr/bin/env bash
# Exercise two-database backup, restore and corruption refusal through the CLI.
#
#   KAHAWAI_BIN=target/debug/kahawai scripts/kahawai-backup-cycle.sh
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
bin=${KAHAWAI_BIN:-"$repo/target/debug/kahawai"}
[ -x "$bin" ] || { echo "missing $bin; run cargo build first" >&2; exit 2; }

work=$(mktemp -d -t kahawai-backup-XXXXXX)
hub_pid=""
cleanup() {
    if [ -n "$hub_pid" ]; then
        kill "$hub_pid" 2>/dev/null || true
        wait "$hub_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

config() {
    printf '[hub]\ndata_dir = "%s"\nbind = "127.0.0.1:0"\nsatellite_bind = "127.0.0.1:0"\n' "$2" >"$1"
}

config "$work/live.toml" "$work/live"
config "$work/restored.toml" "$work/restored"
config "$work/standing.toml" "$work/standing"

# Let the real hub initialize both schemas; keep it online during backup.
"$bin" --config "$work/live.toml" hub >"$work/hub.log" 2>&1 </dev/null &
hub_pid=$!
python3 - "$work/live" "$hub_pid" <<'PY'
import os, pathlib, sqlite3, sys, time
root = pathlib.Path(sys.argv[1])
for attempt in range(300):
    os.kill(int(sys.argv[2]), 0)
    if "hub up" not in (root.parent / "hub.log").read_text():
        time.sleep(0.1)
        continue
    try:
        with sqlite3.connect(f"file:{root}/hub.db?mode=rw", uri=True) as db:
            db.execute("INSERT OR REPLACE INTO settings(key,value) VALUES('backup-check','hub state')")
        with sqlite3.connect(f"file:{root}/mediadb.db?mode=rw", uri=True) as db:
            db.execute("INSERT INTO libraries(id,name,media_type) VALUES('backup-library','Backup library','movies')")
        break
    except sqlite3.OperationalError:
        time.sleep(0.1)
else:
    raise SystemExit("isolated hub did not initialize its databases")
PY
"$bin" --config "$work/live.toml" hub backup "$work/snapshot"
kill "$hub_pid"
wait "$hub_pid" || true
hub_pid=""
python3 - "$work/snapshot" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / "kahawai-backup.json").read_bytes())
if manifest["format"] != 4:
    raise SystemExit("backup did not write manifest format 4")
paths = [artifact["path"] for artifact in manifest["artifacts"]]
if paths != sorted(paths) or not {"hub.db", "mediadb.db", "kahawai.toml"}.issubset(paths):
    raise SystemExit("unexpected artifact inventory: %r" % paths)
for artifact in manifest["artifacts"]:
    body = (root / artifact["path"]).read_bytes()
    if artifact["bytes"] != len(body) or artifact["sha256"] != hashlib.sha256(body).hexdigest():
        raise SystemExit("bad manifest metadata for %s" % artifact["path"])
PY

"$bin" --config "$work/restored.toml" hub restore "$work/snapshot"
python3 - "$work/restored/hub.db" <<'PY'
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as db:
    if db.execute("PRAGMA integrity_check").fetchone() != ("ok",):
        raise SystemExit("restored database failed SQLite integrity_check")
    if db.execute("SELECT max(version) FROM _sqlx_migrations").fetchone()[0] is None:
        raise SystemExit("restored database has no migrations")
    assert db.execute("SELECT value FROM settings WHERE key='backup-check'").fetchone() == ("hub state",)
with sqlite3.connect(sys.argv[1].replace("hub.db", "mediadb.db")) as db:
    assert db.execute("SELECT name FROM libraries WHERE id='backup-library'").fetchone() == ("Backup library",)
    assert db.execute("SELECT max(version) FROM _sqlx_migrations").fetchone()[0] is not None
PY

mkdir -p "$work/standing"
printf 'standing database' >"$work/standing/hub.db"
printf 'standing wal' >"$work/standing/hub.db-wal"
printf 'standing mediadb' >"$work/standing/mediadb.db"
printf 'standing media wal' >"$work/standing/mediadb.db-wal"
python3 - "$work/snapshot/mediadb.db" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
body = bytearray(path.read_bytes())
body[0] ^= 1
path.write_bytes(body)
PY
if "$bin" --config "$work/standing.toml" hub restore "$work/snapshot" --force; then
    echo "corrupt snapshot was restored" >&2
    exit 1
fi
python3 - "$work/standing" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
if (root / "hub.db").read_bytes() != b"standing database":
    raise SystemExit("failed restore replaced the standing database")
if (root / "hub.db-wal").read_bytes() != b"standing wal":
    raise SystemExit("failed restore removed the standing WAL")
if (root / "mediadb.db").read_bytes() != b"standing mediadb":
    raise SystemExit("failed restore replaced the standing mediadb")
if (root / "mediadb.db-wal").read_bytes() != b"standing media wal":
    raise SystemExit("failed restore removed the standing mediadb WAL")
PY

echo "two-database backup/restore cycle passed"
