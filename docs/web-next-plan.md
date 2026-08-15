# Rebuilding the web UI

The design is right. The code under it is not, and this plan replaces the code
without touching the design.

`web/src` is 12.4k lines of TSX over 3.2k lines of CSS, with the three largest
views at 1,883, 1,372 and 1,209 lines each. Those files mix fetching, retry
policy, history manipulation, pipeline state and markup in one scope. UI-26
counts 25 suppressed `react-hooks/exhaustive-deps` warnings against 23 real
warnings, all of them in the seek, recovery, next-episode and queue machinery
— which is where the mixing is worst and where the bugs were.

## The one rule that overrides the rest

**The comments in `web/src` are the specification.** Almost every long comment
there records a bug that was found, understood and fixed: a leaked transcoder
slot per episode boundary, a sign-out that 401'd its own final progress
report, a search panel that told a screen reader it was collapsed while
showing "No matches", a history entry that made the first press of Back do
nothing after every watch. None of that is derivable from the rendered output.

Each phase below begins by reading the comments in the files it replaces and
turning every behavioural claim into a test. A behaviour that survives without
a test is a behaviour we will lose twice.

## Rules

1. **The design is frozen.** `web/src/styles.css` and the current DOM are the
   spec. Visual change is a separate conversation, not a side effect.
2. **Test first.** Domain functions get unit tests before the function;
   components get a mounted test before the markup. Backfilled tests are the
   thing this rebuild exists to avoid.
3. **Layers are directories**, and the boundary is enforced by what a file is
   allowed to import (below).
4. **One page per branch step.** Each lands reviewed, green and runnable.
5. **Backend gaps first** (phase 1), because every one of them is currently
   paid for with client-side compensation that we would otherwise rebuild.
6. **The old UI stays runnable** until cutover, side by side.

## Layers

```
web-next/src/
  api/          generated Orval client, transport, error mapping.  No Vue.
  domain/       pure functions.  No Vue, no fetch, no DOM.
  composables/  reactive state + TanStack Query.  No markup.
  components/   presentational SFCs.  Props in, events out.  No fetching.
  views/        route components.  Wire composables to components.  Thin.
```

Import direction is one-way down that list. A component that fetches, or a
domain function that touches `window`, is the defect this rebuild is fixing;
a lint rule pins the boundary once the directories exist.

## Stack

| Choice | Why |
| --- | --- |
| Vue 3 SFC, composition API | asked for |
| Vite | asked for; already the build |
| Tailwind | asked for; theme derived from the CSS custom properties in `styles.css` |
| oxlint + oxfmt | asked for; **verified** to parse and format `.vue` SFCs (oxlint lints the `<script>` block, oxfmt formats script and template) |
| vue-router | the current app hand-rolls history, and every subtle bit of it — push vs replace, the `ours` marker, scroll reset — is a thing vue-router does natively |
| TanStack Query | see below |
| Pinia | **not** taken. Global state here is three things (auth session, play queue, notices); module-scope refs in a composable cover it. Add Pinia when that stops being true. |
| Vitest + @vue/test-utils + happy-dom | vite-native, and component tests need a DOM that `node --test` does not have |
| Orval | asked for; already generating `web/src/generated` |

**oxlint has no Vue template rules.** There is no `eslint-plugin-vue`
equivalent, so template correctness is unlinted. `vue-tsc` covers the half
that matters (types across the template boundary) and runs in `typecheck`.

### TanStack Query earns its place

Four things the current app hand-rolls that are this library's defaults:

- **Rollback on a failed write.** "When admin changes fail, just showing an
  error is fine as long as the on-screen values revert to what the server
  holds" is `onMutate` snapshot + `onError` restore. Admin has dozens of these.
- **Keep the previous data while refetching.** `SearchOverlay` holds ~40 lines
  of `shownQuery`/`stale`/`setSearching` to keep old rows actionable while new
  ones load.
- **Retry policy per error class.** Transient retries, permanent does not,
  401 refreshes once. Today that decision is spread over every call site.
- **Invalidation from the SSE hint channel.** `openEvents` emits `{kind}`
  hints; those become `queryClient.invalidateQueries` calls in one place
  instead of ad-hoc refetches in each view.

Prefs writes keep their per-key `SerialQueue` — mutation ordering per key is
not something a query cache provides, and the comment on `putPref` explains
what an out-of-order commit costs.

## Phase 0 — iterate without rebuilding the binary — DONE

Two halves, serving different loops. One of them already existed.

- **Vite dev server with a proxy to the hub** — already in
  `web/vite.config.ts`, forwarding `/api` and `/admin` to `:8420`. HMR is
  Vite's own and no Rust code proxies websockets; cookies survive because the
  proxy keeps the origin. This is the editing loop.
- **`--web-dir <path>`** on `kahawai hub`, `kahawai all-in-one` and
  `kahawai-hub`: serve `/app/` from a directory instead of the bundle embedded
  at build time. For trying a bundle a binary does not carry — a second UI, a
  release hub, a colleague's build — without a Cargo rebuild.

`--web-dir` decides only WHERE a file comes from: the SPA fallback, the
`assets/` 404 and the cache headers are one code path either way. The
embedded bundle stays the default and the only thing a release ships.

**Check:** `crates/kahawai-hub/tests/web_dir.rs` — serves a directory the
embedded assets cannot contain, keeps the `assets/` 404 rule, and refuses
every traversal spelling. That last one is the reason the test exists: a URI
path is the only untrusted string in the hub that reaches a filesystem.

## Phase 1 — backend gaps

Ranked by whether the UI can be written correctly without them.

### B1. Machine-readable error reasons — blocking

Findings 2, 6 and 16 in `kahawai-hub-review-findings.md`. Today the hub
returns `format!("{e:#}")` — the whole anyhow chain, including scratch paths,
worker argv and GStreamer stderr — and the difference between a refusal that
clears in a minute and one that is permanent lives only in English prose that
no client may branch on.

The instruction asks for an error taxonomy (transient, permanent,
authentication, authorization, admin, playback). That taxonomy cannot be
built on top of prose. `item_artwork` already returns a fixed string instead
of the chain, citing SEC-WEB-7 — the pattern is in the codebase, it is just
not the rule.

**Decision (owner, 2026-08-15): a JSON body, and it may break clients.**

```
HTTP/1.1 409 Conflict
content-type: application/json

{"code": "session_cap", "message": "too many concurrent streams; close one first"}
```

One shape for every 4xx and 5xx. `code` is an enumerated, stable identifier
published in the OpenAPI schema; `message` is for a human and its wording is
not contractual. The anyhow chain stays in the log. `ApiError` stops being
`(StatusCode, String)` and becomes a type with constructors, so a bare tuple
can no longer smuggle a chain out — 62 construction sites and 209 declared
`text/plain` responses, all converted, and the OpenAPI document re-stamped.

**The status says whether to retry; the code says what happened.** 429 and
503 clear on their own, 5xx retries with backoff, every other 4xx is final.
That split is HTTP's own and needs no kahawai-specific knowledge, which is
the point: a third-party client gets the retry decision right without a
table of our codes in it. `retryable` is deliberately *not* a field — it
would be the same decision computed in two places, free to disagree.

Rejected: an `X-Kahawai-Error` header beside the existing text body. It is
additive and breaks nothing, and that is its problem — the code is out of
band, easy to not send, and easy to not read, so the prose would have stayed
the real answer.

Rejected: an `X-Kahawai-Error` header beside the existing text body. It is
additive and breaks nothing, and that is its problem — the code is out of
band, easy to not send, and easy to not read, so the prose would have stayed
the real answer.

### B2. The per-user session cap gets its own status — blocking

`too many concurrent streams` is a 409 today, indistinguishable from "this
item is unplayable, forever". 429 (or a `code` from B1) lets the album queue
wait it out instead of guessing at three retries. Falls out of B1; listed
separately because the status matters as much as the body.

### B3. Grouped cross-library search

`SearchOverlay` fires one request per library because "at most five each is
not something a single LIMIT can express". One endpoint returning grouped
hits removes the fan-out, the per-library partial-failure state, and the
`Promise.all([])`-answers-immediately trap the comment there warns about.

### B4. Part numbers on source rows

UI-27 / finding 11. Seven part files of one film are published as a flat
array ordered by size, so no client can tell "one film, seven parts" from
"seven encodes, pick one". A `part` field; the detail page reads it.

### B5. Lost-update guard on user library grants

UI-25. Two admins editing one user's grants silently discard the first write.
Needs a version on the request or a delta. Being fixed here rather than
carried because Admin is a page this rebuild writes from scratch, and because
"changes revert to what the server holds" is a stated requirement of it.

### B6. Track durations on album children

UI-4. One field; the album track list wants it.

**Deferred, with reasons:** UI-1 (artist entity — a migration and an
enrichment path, and the owner already decided albums), UI-3 (AniDB title
refresh — no plumbing behind it), UI-23 (which files a mediahost could not
read — needs somewhere to store them first).

Each gap lands as its own commit with hub tests, before any Vue is written.

## Phase 2 — clean room

`web-next/`, beside `web/` (owner decision: the old UI stays readable and
runnable for the whole rebuild, because its comments are the specification),
running against the dev hub through Phase 0's proxy. Vite, Vue 3, Tailwind, oxlint, oxfmt, Vitest, Orval pointed at the
same `openapi.json` and the same fingerprint check. Nothing but a shell that
boots and a green test run.

Tailwind's theme is extracted from the custom properties already at the top of
`styles.css`, so the palette has one definition, as it does now.

## Phase 3 — foundations

Written before any page, because every page depends on them and because these
are the three places the instruction warns will hurt if backfilled.

- **Auth session.** In-memory access token, refresh scheduled off `expires_in`,
  `navigator.locks` for the two-tab race, `BroadcastChannel` for cross-tab
  sign-out, generation counter so a late response cannot install a token into
  a session that ended. The hub re-sets the HttpOnly `kahawai_media` cookie
  (path `/api/v1`, same lifetime as the access token) on every login and
  refresh, so the playback credential follows the access token by
  construction — the client's job is only to keep refreshing on time. Port
  the existing logic behaviour-for-behaviour; it is correct and expensive to
  rediscover.
- **Error model.** B1's codes mapped to the six classes, one place. Retry
  policy, what reaches a toast, what reaches an inline retry, what signs you
  out. UI-21's test — *is the control that caused this still on screen?* —
  decides toast versus inline, and is encoded here rather than re-argued per
  call site.
- **Notice host and boundaries.** One toast host; error boundaries keyed on
  the screen rather than the address, which is what keeps an autoplay handover
  from remounting the player.

## Phase 4 onward — page by page

In dependency order. Each is: read the old file's comments → tests → domain →
composables → components → view → review → commit.

| # | Page | Notes |
| --- | --- | --- |
| 4 | Shell, header, menus, routing | the two menus, the search box's two meanings, keyboard and ARIA |
| 5 | Auth / setup | including UI-24: a hub that never answers must not leave a dead form |
| 6 | Home (libraries, shelves, continue watching) | inline retries per shelf, the three loading states of UI-22 |
| 7 | Search panel | on B3; combobox pattern, `aria-activedescendant`, every key driven |
| 8 | Library grid | virtualised, fixed cell height (UI-11), `srcset` at both densities (UI-16) |
| 9 | Detail + Season | error split: a refused Play is not a failed item load (UI-13) |
| 10 | Settings | drag ordering with keyboard equivalents (UI-12), per-key write queue |
| 11 | Admin | optimistic writes with rollback throughout; B5 |
| 12 | Album queue | survives navigation; per-track removal (UI-2) now that it is cheap |
| 13 | Video player | largest and last; see below |
| 14 | Accessibility pass | UI-17: keyboard-only run and a screen reader, which has never happened |

### The player

1,883 lines today, and the file the rest of the plan exists to make
approachable. It splits into: session lifecycle (start, seek-restart,
release, who owns the id), transport state (loading, buffering, playing,
paused, ended, recovering), track selection, subtitle delivery across five
kinds, capability probing, and the recovery machine for a satellite that goes
away mid-playback (UI-19).

Every one of those is a state machine that is testable without a DOM, and
none of them is tested that way today. They become domain modules with tests
first; the SFC is what is left over.

## Cutover

`web/` is deleted and `web-next/` takes its name and its build.rs wiring, in
one commit, once every row above is done and the accessibility pass has run.
Not before: two half-built UIs is the state this plan is designed to spend as
little time in as possible.

## Review

`/code-review` on the working tree before every commit. A PR-wide
`/code-review ultra` after the branch is complete — that one is
maintainer-triggered.
