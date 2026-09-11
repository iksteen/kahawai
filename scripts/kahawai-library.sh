#!/usr/bin/env bash
# Library checks and copy assignment.
#   kahawai-library.sh                         regression checks
#   kahawai-library.sh audit DATA_DIRECTORY    open/migrate a database copy
#   kahawai-library.sh copies LIBRARY_ITEM_ID       show copies and their revisions
#   kahawai-library.sh match COPY_ID JSON_FILE apply a revision-guarded decision
#   kahawai-library.sh metadata LIBRARY_ITEM_ID JSON_FILE replace work descriptions
# API commands use KAHAWAI_TOKEN and KAHAWAI_API (default localhost:8420).
# A match body includes action and expected_revision; see web/openapi.json.
set -euo pipefail
cd "$(dirname "$0")/.."
case "${1:-test}" in
    audit)
        [[ $# == 2 ]] || { echo 'usage: kahawai-library.sh audit DATA_DIRECTORY' >&2; exit 2; }
        KAHAWAI_SKIP_WEB_BUILD=1 cargo run --quiet -p kahawai-hub --example library_audit -- "$2"
        ;;
    copies|match|metadata)
        [[ -n "${KAHAWAI_TOKEN:-}" ]] || { echo 'KAHAWAI_TOKEN is required' >&2; exit 2; }
        [[ $# -ge 2 && "$2" =~ ^[A-Za-z0-9_-]+$ ]] || { echo 'a library/copy ID is required' >&2; exit 2; }
        api="http://${KAHAWAI_API:-localhost:8420}"
        if [[ "$1" == copies ]]; then
            curl --fail-with-body --silent --show-error -H "Authorization: Bearer $KAHAWAI_TOKEN" "$api/api/v1/items/$2"
        else
            [[ $# == 3 && -f "$3" ]] || { echo 'a JSON body file is required' >&2; exit 2; }
            method=POST; route="/admin/v1/collection-items/$2/match"
            if [[ "$1" == metadata ]]; then method=PUT; route="/admin/v1/library-items/$2/metadata"; fi
            curl --fail-with-body --silent --show-error -X "$method" -H "Authorization: Bearer $KAHAWAI_TOKEN" -H 'Content-Type: application/json' --data-binary "@$3" "$api$route"
        fi
        ;;
    *) KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test admin_api --test library_items --test library_api --test library_playback --test library_session_resources --test library_preferences --test library_identity_upgrade --test library_rejection_aliases --test library_grants --test up_next --test music_album_identity --test music_song_identity --test library_provider_rank_bridge "$@" ;;
esac
