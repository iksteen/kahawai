#!/usr/bin/env bash
# Check signed candidate-artwork delivery and its production-browser CSP path.
set -euo pipefail
cd "$(dirname "$0")/.."
KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --lib candidate_artwork
npm --prefix web run build
npm --prefix web run test:csp -- --grep 'match selector'
