# Season artwork

Season covers are separate provider answers; an episode catalogue does not need
to win metadata resolution for its provider's cover to be used. Source-backed
season groups returned by the dedicated catalogue `/seasons` endpoint (and
the children API) have optional `metadata`
containing artwork URLs and provider/record provenance. Artwork never creates
seasons or episodes that have no media in the requested library.

Sources:

- TMDB: selected season poster, including season zero. Missing selected posters
  use English, then language-neutral, then other posters, ranked by votes.
- TVDB: season posters from the series' default season order, matching the
  episode lookup. Artwork type IDs come from `/artwork/types`; banners and
  alternate-order seasons are excluded.
- Fanart TV: `seasonposter` answers using a verified TVDB series ID. Numeric
  seasons, including zero, qualify; `all` does not. This is artwork-only evidence
  and cannot assign identities. English precedes neutral and other languages;
  likes and stable IDs break ties.
- AniDB and AniList: native-entry covers, applied only when every contributing
  copy maps the entire single group to the same native entry. They never invent
  numbered season mappings or supply specials art.
- Local: `seasonNN-poster.{jpg,jpeg,png,webp}` beside the show, or a generic
  poster/cover/folder/front image inside a matching `Season NN` directory.
  Specials also accept `season-specials-poster`. Matching is case-insensitive.
  Show posters remain separate. Ordinary scans detect additions, edits and
  removals without reprobes; mediahosts need the updated scanner.

Resolution follows local, primary, ordered supplements, then Fanart. The image
endpoint falls back to cached show artwork:

```text
GET /api/v1/catalogue/libraries/{library}/items/{parent}/seasons
GET /api/v1/catalogue/libraries/{library}/items/{parent}/seasons/2/items?limit=200&offset=0
GET /api/v1/catalogue/libraries/{library}/items/{parent}/artwork?season=2&size=card
GET /api/v1/catalogue/libraries/{library}/items/{parent}/artwork?season=absolute
scripts/kahawai-mediadb.sh api seasons LIBRARY PARENT
scripts/kahawai-mediadb.sh api children LIBRARY PARENT --season 2 --limit 200
```

`0` means specials; `absolute` selects the unnumbered group. Access follows the
library's normal authorization. Season items support `offset` and `limit` (1–200),
include viewer watch state, and return 404 for absent source-backed seasons.
The older `/children?season=…` filter remains a compatibility alias; Android, web
and CLI season browsing use `/seasons/{season}/items`. Season image reads serve cached bytes only.
Background workers prefetch every provider's selected cover and all standard
sizes, including covers that currently lose resolution. Caches are retained.

Migration 12 schedules a one-time backfill for matched, source-backed series and
anime in visible libraries. It preserves active claims, retries, configuration
blocks, force flags, attempts and existing caches. A request generation records
follow-up work arriving during a claim. AniList answers and AniDB XML reuse
retained cover data; old TMDB episode pages discarded posters and therefore need
a new representation. Reopening the database does not schedule the backfill
again. Subtitle backfills and the legacy content-hash import remain independent.
The season endpoint returns all source-backed episode groups with totals, viewer
watched counts and metadata, independently of episode pagination. It does not
resolve individual episodes or decode provider child catalogues. Albums and
movies return an empty season list.

Android consumes this endpoint for season cards and selected-season posters,
then loads paginated children for the chosen season. Single-season shows keep
their flat episode list. Older hubs fall back to the groups on a small children
page and use show art when season metadata is absent. Season images revalidate
on page entry against the hub cache so temporary show-art fallbacks do not
stick in the client cache. Web presentation remains a separate follow-up.
