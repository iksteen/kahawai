# CLI tooling with mediadb

`kahawai-list.sh` prints a library ID and stable item ID on each item row.
Episodes and tracks have `child1:…` IDs; use those unchanged with query, play
and watched commands. Collection-copy IDs identify enrichment targets;
media-entry IDs identify physical playback renditions.

All examples run from the repository. `KAHAWAI_API=host:port` selects the hub
(default `localhost:8420`). A password of `-` prompts for it.

```sh
scripts/kahawai-list.sh -L USER -                       # libraries
scripts/kahawai-list.sh USER - "title"                  # search all libraries
scripts/kahawai-list.sh -l LIBRARY USER -               # library items
scripts/kahawai-list.sh -l LIBRARY -i PARENT USER -     # episodes/tracks
scripts/kahawai-list.sh -l LIBRARY -r USER -             # album artists
scripts/kahawai-list.sh -l LIBRARY -A ARTIST_KEY USER - # artist albums
scripts/kahawai-list.sh -p USER -                       # continue watching
scripts/kahawai-list.sh -n USER -                       # up next
scripts/kahawai-query.sh -l LIBRARY -j USER - ITEM       # playback preview JSON
scripts/kahawai-query.sh -l LIBRARY -e ENTRY USER - ITEM # preview a rendition
scripts/kahawai-play.sh -l LIBRARY -e ENTRY USER - ITEM  # play it in mpv
scripts/kahawai-watched.sh -l LIBRARY USER - ITEM        # mark watched
```

Browse reads every page. `-l` also restricts the home feeds. Artist keys are
opaque and may contain spaces or punctuation; quote them in the shell.
Playback previews use the catalogue POST endpoint without starting a session.
The JSON includes `sources`, `copy_ids` and the negotiated `subtitle_source`.

`kahawai-users.sh ADMIN - list` lists accounts and their catalogue library access.
Use `grant USER LIBRARY` or `revoke USER LIBRARY` with a library name or mediadb
library ID. Access changes send the account's grants version; concurrent edits
fail with 409 so another administrator's changes are not overwritten.

The JSON API companion and library/enrichment helpers use `KAHAWAI_TOKEN`.
`KAHAWAI_URL` can override the full URL, including HTTPS, for these helpers.

```sh
scripts/kahawai-mediadb.sh api login USER
# Set KAHAWAI_TOKEN to the returned access_token.
scripts/kahawai-mediadb.sh api collections
scripts/kahawai-mediadb.sh api create-library "Films" movies COLLECTION_ID
scripts/kahawai-mediadb.sh api set-collections LIBRARY COLLECTION_A COLLECTION_B
scripts/kahawai-mediadb.sh api rescan LIBRARY --deep
scripts/kahawai-library.sh copies LIBRARY ITEM
scripts/kahawai-enrich.sh items --library LIBRARY --offset 0 --limit 200
scripts/kahawai-enrich.sh detail COPY
scripts/kahawai-enrich.sh search COPY REVISION "title"
scripts/kahawai-enrich.sh pick COPY REVISION RECORD_ID
scripts/kahawai-library.sh match COPY correction.json
scripts/kahawai-mediadb.sh api item-log ITEM
scripts/kahawai-mediadb.sh api segments
scripts/kahawai-work.sh status
scripts/kahawai-work.sh rerun subtitles ocr
```

Correction JSON uses `revision`, `action`, and (where needed) `record_id` or
`library_item_id`. Read the current revision from enrichment detail; stale
writes fail.
Relative JSON filenames are resolved from the caller's working directory.

`kahawai-parts.sh`, `kahawai-avsync.sh`, and `kahawai-latency.sh` require
`-l LIBRARY` and accept `-e ENTRY`. Latency measurements require explicit item
IDs rather than IDs from an old developer database. Fanout and container smoke
checks compose their fixture collections into a library before browsing or
starting playback.

```sh
python3 scripts/kahawai-cli-check.py           # isolated HTTP fixture
scripts/kahawai-mediadb.sh check-cli           # CLI against a real disposable hub/mediahost
scripts/kahawai-mediadb.sh check-live          # full ingestion and restart checks
scripts/kahawai-library.sh                    # mediadb and integration tests
scripts/kahawai-library.sh audit DATA_COPY    # open/migrate DATA_COPY/mediadb.db
```

The audit reads active items through Store; archived identities are not browse
rows. It does not open `hub.db`. Retired hub catalogue recovery tooling has
been removed along with its tables.
