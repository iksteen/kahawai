#!/usr/bin/env bash
# Mediadb library checks and copy assignment.
#   kahawai-library.sh                         mediadb and hub integration checks
#   kahawai-library.sh audit DATA_DIRECTORY    open/migrate a mediadb database copy
#   kahawai-library.sh copies LIBRARY ITEM     show physical copies and sources
#   kahawai-library.sh match COPY JSON_FILE    apply a revision-guarded correction
# Match JSON uses revision, action and record_id/library_item_id from enrichment.
# Composition commands: kahawai-mediadb.sh api create-library|set-collections|rescan.
# API commands use KAHAWAI_TOKEN and KAHAWAI_API (default localhost:8420).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
case "${1:-test}" in
    audit)
        [[ $# == 2 ]] || { echo 'usage: kahawai-library.sh audit DATA_DIRECTORY' >&2; exit 2; }
        KAHAWAI_SKIP_WEB_BUILD=1 cargo run --manifest-path "$HERE/../Cargo.toml" --quiet -p kahawai-hub --example library_audit -- "$2"
        ;;
    copies)
        [[ $# == 3 ]] || { echo 'usage: kahawai-library.sh copies LIBRARY ITEM' >&2; exit 2; }
        exec "$HERE/kahawai-mediadb.sh" api item "$2" "$3"
        ;;
    match)
        [[ $# == 3 ]] || { echo 'usage: kahawai-library.sh match COPY JSON_FILE' >&2; exit 2; }
        exec "$HERE/kahawai-mediadb.sh" api match "$2" "$3"
        ;;
    test)
        [[ $# == 0 ]] || shift
        cargo test --manifest-path "$HERE/../Cargo.toml" -p kahawai-mediadb "$@"
        KAHAWAI_SKIP_WEB_BUILD=1 cargo test --manifest-path "$HERE/../Cargo.toml" -p kahawai-hub --test mediadb_ingestion --test admin_api --test segment_scans "$@"
        ;;
    -h|--help) sed -n '2,/^set /{ /^#/s/^# \{0,1\}//p; }' "$0" ;;
    *) echo 'usage: kahawai-library.sh test|audit DATA_DIRECTORY|copies LIBRARY ITEM|match COPY JSON_FILE' >&2; exit 2 ;;
esac
