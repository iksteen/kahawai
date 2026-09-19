#!/usr/bin/env bash
# Mediadb creation/migration and fresh-database persistence checks.
set -euo pipefail
task_invocation_dir=$PWD
task_repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$task_repo"
case "${1:-check}" in
    api)
        shift
        cd "$task_invocation_dir"
        exec python3 "$task_repo/scripts/kahawai-mediadb-api.py" "$@"
        ;;
    check-live|check-cli)
        if [[ $1 == check-cli ]]; then
            python3 scripts/kahawai-cli-check.py
            export KAHAWAI_MEDIADB_CLI_CHECK=1
        fi
        cargo build
        . scripts/kahawai-gst-env.sh
        exec python3 scripts/kahawai-mediadb-live.py target/debug/kahawai
        ;;
    create|migrate)
        if [[ $# != 2 ]]; then
            echo "usage: kahawai-mediadb.sh $1 DATABASE" >&2
            exit 2
        fi
        task_database=$2
        if [[ "$task_database" != /* ]]; then
            task_database="$task_invocation_dir/$task_database"
        fi
        cargo run --quiet -p kahawai-mediadb --example catalogue_check -- "$1" "$task_database"
        ;;
    check)
        cargo test -p kahawai-mediadb
        task_dir=$(mktemp -d)
        trap 'rm -rf "$task_dir"' EXIT
        cargo run --quiet -p kahawai-mediadb --example catalogue_check -- seed "$task_dir/mediadb.db"
        cargo run --quiet -p kahawai-mediadb --example catalogue_check -- migrate "$task_dir/mediadb.db"
        cargo run --quiet -p kahawai-mediadb --example catalogue_check -- verify "$task_dir/mediadb.db"
        for phase in archive verify-archive resurrect verify-restored; do
            cargo run --quiet -p kahawai-mediadb --example catalogue_check -- "$phase" "$task_dir/mediadb.db"
        done
        ;;
    scale)
        task_dir=$(mktemp -d)
        trap 'rm -rf "$task_dir"' EXIT
        cargo run --release --quiet -p kahawai-mediadb --example catalogue_check -- scale "$task_dir/mediadb.db"
        ;;
    *) echo 'usage: kahawai-mediadb.sh check|check-live|check-cli|scale|create DATABASE|migrate DATABASE|api COMMAND' >&2; exit 2 ;;
esac
