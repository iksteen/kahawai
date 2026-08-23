#!/usr/bin/env bash
# Exercise manifest-v3 backup, restore and corruption refusal through the CLI.
#
#   KAHAWAI_BIN=target/debug/kahawai scripts/kahawai-backup-cycle.sh
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
bin=${KAHAWAI_BIN:-"$repo/target/debug/kahawai"}
[ -x "$bin" ] || { echo "missing $bin; run cargo build first" >&2; exit 2; }

work=$(mktemp -d -t kahawai-backup-XXXXXX)
trap 'rm -rf "$work"' EXIT

config() {
    printf '[hub]\ndata_dir = "%s"\n' "$2" >"$1"
}

config "$work/live.toml" "$work/live"
config "$work/restored.toml" "$work/restored"
config "$work/standing.toml" "$work/standing"

"$bin" --config "$work/live.toml" hub backup "$work/snapshot"
python3 - "$work/snapshot" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / "kahawai-backup.json").read_bytes())
if manifest["format"] != 3:
    raise SystemExit("backup did not write manifest format 3")
paths = [artifact["path"] for artifact in manifest["artifacts"]]
if paths != sorted(paths) or set(paths) != {"hub.db", "kahawai.toml"}:
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
PY

mkdir -p "$work/standing"
printf 'standing database' >"$work/standing/hub.db"
printf 'standing wal' >"$work/standing/hub.db-wal"
python3 - "$work/snapshot/hub.db" <<'PY'
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
PY

echo "backup manifest-v3 cycle passed"
