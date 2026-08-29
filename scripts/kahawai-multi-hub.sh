#!/usr/bin/env bash
# Runnable protocol-4 smoke check: named-hub routing, durable local catalogue,
# and the real mTLS offer/cursor/delta/ACK projection path.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo test -p kahawai-runtime mediahost_hubs_filter_collections_and_legacy_hub_coexists
cargo test -p kahawai-mediahost catalog::tests
cargo test -p kahawai-hub --test link_wire enrolled_mediahost_links_and_disconnect_is_tracked
