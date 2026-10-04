#!/usr/bin/env bash
# OPS-RDY-4A: validate every metric family's reviewed label dimensions.
# Optionally check a captured /metrics response against the same policy.
set -euo pipefail
if [ "$#" -ne 0 ]; then
    if [ "$#" -ne 2 ] || [ "$1" != --scrape-file ]; then
        echo 'usage: kahawai-metrics-check.sh [--scrape-file /path/to/scrape.prom]' >&2
        exit 2
    fi
    case "$2" in
        /*) export KAHAWAI_METRICS_SCRAPE_FILE="$2" ;;
        *) export KAHAWAI_METRICS_SCRAPE_FILE="$PWD/$2" ;;
    esac
fi
cd "$(dirname "$0")/.."
cargo test --locked -p kahawai-hub --test metric_labels --test observability -- --nocapture
