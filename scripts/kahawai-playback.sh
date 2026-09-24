#!/usr/bin/env bash
# kahawai-playback: the crate's tests, a real pipeline through its executor,
# and the dependency rule that keeps it out of the satellites' way.
set -euo pipefail
task_repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$task_repo"
. scripts/kahawai-gst-env.sh
case "${1:-check}" in
    check)
        cargo test -p kahawai-playback
        cargo run --quiet -p kahawai-playback --example playback_check
        ;;
    pull)
        export KAHAWAI_MEDIA_TEST_STRICT=1
        # Exact pull requests, deduplicated nearby/converging cursors, distant offsets, bounded transport,
        # protocol-6 dual-grant descriptors, real mTLS remux/transcode, and the supervised worker.
        cargo test -p kahawai-media --lib remux:: -- --test-threads=1 --nocapture
        cargo test -p kahawai-transport --lib
        cargo test -p kahawai-playback --test job_codecs
        cargo test -p kahawai-hub --test link_wire --test transcoder_wire
        cargo test -p kahawai-transcoder --lib stream_rate
        cargo test -p kahawai-transcoder --lib source_
        cargo test -p kahawai-hub --lib source_
        cargo test -p kahawai-hub --lib streaming_tests
        cargo test -p kahawai-hub --lib cancellation_tests
        cargo test -p kahawai-hub --test remux_play --test transcode_dispatch
        cargo build --bin kahawai
        cargo run --quiet -p kahawai-playback --example playback_check -- --worker-exe target/debug/kahawai
        ;;
    slow)
        # The deadline test spends thirty seconds of wall clock on purpose.
        cargo test -p kahawai-playback --test executor -- --ignored
        ;;
    worker)
        # The child-process path every real session takes: the worker is the
        # kahawai binary's hidden subcommand, so build it first.
        cargo build --bin kahawai
        cargo run --quiet -p kahawai-playback --example playback_check -- --worker-exe target/debug/kahawai
        ;;
    lean)
        # The transcoder daemon depends on this crate and must never link
        # what only the hub needs. axum is NOT in this list: tonic's server
        # side depends on it, so every satellite has always carried it; the
        # hub-only markers are the database, the OCR engine, the provider
        # HTTP client and the embedded UI. Gate on the grep's exit code, inverted.
        if cargo tree -p kahawai-transcoderd -e normal --prefix none \
            | grep -E '^(kahawai-hub|kahawai-mediadb|kahawai-sqlite|sqlx|leptess|reqwest|rust-embed|utoipa-swagger-ui) '; then
            echo "kahawai-transcoderd links hub-only code" >&2
            exit 1
        fi
        echo "kahawai-transcoderd stays lean"
        ;;
    *)
        echo "usage: kahawai-playback.sh [check|pull|slow|worker|lean]" >&2
        exit 2
        ;;
esac
