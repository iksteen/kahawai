#!/usr/bin/env bash
# Browser acceptance against the isolated hub + real mediahost ingestion fixture.
set -euo pipefail
cd "$(dirname "$0")/.."
export KAHAWAI_MEDIADB_UI_CHECK=1
exec scripts/kahawai-mediadb.sh check-live
