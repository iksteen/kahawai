# Title and cast artwork

The canonical description keeps poster URLs in `artwork`. On an episode this is
its still image. Optional `logo`, `banner` and `backdrop` fields select one image
each, independently, using the existing local/primary/supplement/Fanart ordering.
Each field carries normal record/provider provenance. Artwork cannot identify a
title or create episodes, seasons or people in the catalogue.

Cast credits retain `name` and `role` and add optional `id` and `image` fields.
IDs are provider-qualified, such as `tmdb:123` or `tvdb:456`. Names are not used to
join people across providers. Credits without a photo or stable ID remain valid.

## Sources

- TMDB: logos and backdrops from images appended to title details; headshots from
  the existing credits response; episode stills from existing season pages.
- TVDB: logos, banners and backdrops from extended title records, using the
  provider's artwork-type dictionary; headshots from character `personImgURL`;
  episode images from the existing episode catalogue.
- Fanart: movie logos/banners/backgrounds through a verified TMDB movie ID;
  series equivalents through a verified TVDB show ID. One response supplies all
  title images and existing season covers. These are auxiliary descriptions,
  never identity candidates.
- AniList: `bannerImage` alongside its existing native cover.
- Local: `clearlogo`/`logo`, `banner`, and `fanart`/`backdrop`/`background` image
  sidecars, accepting JPG/JPEG/PNG/WebP case-insensitively. A numbered season
  directory uses its show's directory. An updated mediahost advertises these
  paths; ordinary scans detect additions, edits and removals without reprobes.

Provider images prefer English, then language-neutral, then other languages;
scores and stable paths break ties. TMDB backdrops and logos retain original
resolution. Originals and the existing `thumb`, `card1x` and `card` derivatives
are retained. Transparent images stay PNG through resizing and cached reads
report the actual image MIME type. Downloads are bounded to 32 MiB.

## API and verification

```text
GET /api/v1/catalogue/libraries/{library}/items/{item}/artwork?kind=logo
GET /api/v1/catalogue/libraries/{library}/items/{item}/artwork?kind=banner
GET /api/v1/catalogue/libraries/{library}/items/{item}/artwork?kind=backdrop
GET /api/v1/catalogue/libraries/{library}/items/{episode}/artwork?kind=still
GET /api/v1/catalogue/libraries/{library}/items/{item}/cast/{person}/artwork?size=thumb
scripts/kahawai-mediadb.sh api artwork LIBRARY ITEM --kind logo --out /tmp/logo.png
scripts/kahawai-mediadb.sh api artwork LIBRARY ITEM --person tmdb:123 --out /tmp/actor.jpg
```

Requests require normal library access. Cast IDs must occur in the accessible
title's resolved cast. The new image reads are cache-only: a miss returns 404
without provider traffic. The default `kind=poster` and existing season-poster
behavior remain compatible. Episode title-level logos/banners/backdrops use the
parent; `kind=still` serves only the episode's own image.

## Prefetch and upgrade

The existing durable artwork lanes prefetch selected title images and cast
headshots from eligible provider records, including records that lose metadata
resolution. Episode prefetch follows actual source coverage, including specials,
absolute numbering and combined-file ranges; a provider's missing library
episodes do not consume image downloads. URL deduplication shares retained
images and derivatives across repeated credits and copies.

Migration 13 schedules matched, source-backed movies/series/anime once. It
preserves running claims, retry deadlines, provider blocks, attempts, force flags
and existing caches. Request generations record work arriving during a claim;
reopening does not schedule the backfill again. Older complete TMDB/TVDB/AniList
answers discarded these fields, so their richer identified representations use
new cache questions while retaining old answers. Fanart retains its complete
response for all image roles. Existing season and subtitle backfills stay
independent.

## Web presentation

Movie and series detail pages show cached backdrops (or banners when no backdrop
is selected), clear logos and posters. Episode headings retain their own title.
Season pages use the selected season cover; the show page has season-cover links
and lazy episode stills. Cast portraits use provider-qualified IDs through the
library-scoped cast route, with names and roles retained when photos are absent.

Logos replace the visual title only after loading; the text heading remains
accessible, and failed images leave a plain readable layout. Narrow screens use
the backdrop and logo without the poster column. Artwork URLs stay stable across
watched-state refreshes, retaining browser caches. A deterministic revision of the
selected image is included in its URL, so changed metadata replaces both successful
and failed images without reusing the browser's previous decoded image.

Run `cd web && npm run test:csp -- artwork.spec.ts` after building the web bundle
(with the patched GStreamer environment and `KAHAWAI_SKIP_WEB_BUILD=1`). It checks
desktop/mobile artwork, missing-image fallbacks, episode stills and season navigation.
