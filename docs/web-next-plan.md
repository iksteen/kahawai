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

## Phase 1 — backend gaps — DONE (on the wire)

Every gap below is closed in the hub. Two of them — UI-4 and UI-27 — are only
half of a UI-checklist entry: the field exists and nothing renders it yet, so
those entries stay open until the page that reads them is rebuilt. Marking
them done because the API moved would be marking a requirement done by
redefining it.

Ranked by whether the UI can be written correctly without them.

### B1. Machine-readable error reasons — blocking — DONE

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
not contractual. The anyhow chain stays in the log. `ApiError` stopped being
`(StatusCode, String)` and became a type with constructors, so there is no
longer a tuple a chain can be dropped into.

The contract is checked rather than described: `web/test/openapi-contract.test.ts`
reads the generated document and asserts that every 4xx and 5xx body is
`application/json`; that every route taking a body declares the 400, 413 and
415 a bad one produces; that every route with a path or query parameter
declares its 400; that no bodyless route declares a 413 or 415; and that every
published code is snake case.

It exists because the prose version was wrong four times. Counts recited in
this file went stale three times on their own — which is the argument against
reciting them at all, and for a test that fails on the change instead of on
the next reviewer.

One response sits outside deliberately: `stream_session`'s 416 has no body,
which is what RFC 9110 asks for. Item artwork's 404 carries the same body as
every other refusal and is merely cacheable.

Seven rounds of review went into this, and rounds two through seven mostly
found defects in the previous round's FIXES — 3, 8, 6, 4, 7, 5 findings. Most
were one mistake wearing different clothes: assuming a boundary held rather
than making it hold. The ones worth carrying forward:

- `ApiError::log` first returned `error.to_string()`, the outermost anyhow
  layer, on the theory that the outermost layer is a sentence. It is not: the
  session layer bails with the worker's stderr *inside* that layer, and the
  fallback-sink path flattens a whole chain into it with
  `with_context(|| format!("{first:#}"))`. The message is a parameter now, so
  no error's text crosses the boundary at all and `ApiErrorBody`'s promise is a
  property of the signature.
- Item artwork's 404 is JSON too. The grant gate answers that same operation
  with an error body, so documenting the miss as `text/plain` left one of two
  shapes undocumented. Both are `not_found` with only the message differing,
  which is correct: a distinct code would leak existence on the one route whose
  denials are meant to look like absence.
- `ApiJson`, `ApiQuery` and `ApiPath` replace every bare axum extractor. Their
  rejections are `text/plain` with no code, so a malformed body — and then,
  once that was fixed, `?limit=abc` and a non-numeric id in a path — were the
  refusals that did not carry one, on routes that mostly declared no 400 at
  all. Fixing the body half and leaving the other two would have made the
  contract true of most refusals, which is not what the document says.
- A wrong `Content-Type` is 415 and a body that will not parse is 400. An
  earlier cut collapsed both into 400 on the grounds that what you sent is
  wrong either way — which quietly changed the status QUERY had been
  answering, and its own test said so. Throwing away a distinction axum
  already makes is against the grain of the whole change.
- Collapsing producers' messages went too far in the other direction. A spent
  OpenSubtitles budget — five downloads into an anonymous day, so an ordinary
  Tuesday — arrived as "the provider did not answer", which sends somebody to
  retry an outage that is not happening instead of adding an account. Its
  sentence is authored, names the way out and leaks nothing. It is a typed
  error with a code of its own now, and the API reads THAT type's `Display`
  rather than the chain around it. Same shape for enrollment approval, where
  a CA that failed to sign was reported as `forbidden` — the one code that
  means "a different account might".
- The opposite error, three times: one code standing in for several refusals.
  A duplicate username was a 400 saying the password might be too short while
  a duplicate library name was a 409; a blank username was told the password
  was too short; and attaching a collection answered one opaque 409 for "no
  such library", "no such collection" and a media-type mismatch. Typed errors
  in `auth` and `registry` are what let the API tell them apart.
- The player's stand-by dialog is entered and left on `wait` alone. Round four
  said the exit should also cover `busy` — the account at its stream cap —
  because it clears by itself; round six showed what that costs. The dialog
  says the machine holding the file has stopped answering and offers one
  button, so a viewer whose host came back and who is merely at the cap sits
  in front of a false cause for ever. Leaving takes them to the hub's own
  sentence, which names the thing that clears it.

Every `format!("{e:#}")` reaching a client is gone from the refusal paths. Two
remain on purpose, on `admin_verify_anidb` and `admin_set_anidb` — admin-only
200s whose entire job is telling the admin why the credential they just typed
did not work. The OpenAPI document is re-stamped.

**The status says whether to retry; the code says what happened.** 429 and
503 clear on their own, 5xx is worth a backoff, every other 4xx is final.
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

### B2. The per-user session cap gets its own status — blocking — DONE

`too many concurrent streams` was a 409, indistinguishable from "this item is
unplayable, forever". It is 429 `session_cap` now, from a `sessions::SessionCap`
type rather than a `bail!` sentence, and the album queue waits it out instead of
guessing at three retries.

Recorded while doing it: six routes documented **410** for a gone session and
the hub returns 404 — the one status the client's recovery machine keys on. The
document was wrong; it says 404 now.

### B3. Grouped cross-library search — WITHDRAWN, measured

`SearchOverlay` fires one request per library for two stated reasons: "the
items endpoint does not say which library a row came from" and "at most five
each is not something a single LIMIT can express".

The first stopped being true when `library_id` was added to browse rows. The
second is a window function, and it was built — `per_library` on the same
endpoint, its own arm so the measured queries stayed byte for byte what they
were — and then measured against the benchmark's adversarial needle, which
matches the whole catalogue:

| shape | 50k | 250k (worst of 3) |
| --- | --- | --- |
| dense search, one library (today) | 20.2 ms | 69.9 ms |
| `per_library`, correlated subquery in `PARTITION BY` | 84.3 ms | 840.8 ms |
| `per_library`, joining `library_collections` | 59.9 ms | 364.6 ms |

The first shape is the 912 ms failure mode `item_page_sql` exists to avoid,
reproduced exactly: the library lookup ran for every LIKE candidate rather
than for a page. Joining the mapping table instead is 2.3× better and still
misses the 200 ms NFR-1 target at 250k, because ranking WITHIN each library
cannot stop early the way `ORDER BY … LIMIT` on an index can — the window has
to see every match before it knows which five are the top five.

So it is withdrawn rather than shipped slow. The client's fan-out is N
CONCURRENT requests, so its wall clock is one query and not N — it was never
the slower option, and the complexity it costs a client is now much smaller
than when the comment was written: `library_id` identifies each hit, and
TanStack Query owns the fan-out and the partial-failure states that the
hand-rolled panel had to track itself.

Reopening this wants a different shape, not a faster version of this one — a
per-library UNION of indexed top-N stops early on a dense needle but pays N
full scans on a rare one, which is the trade to measure next time.

### B4. Part numbers on source rows — DONE

UI-27 / finding 11. Seven part files of one film were published as a flat
array ordered by size, so no client could tell "one film, seven parts" from
"seven encodes, pick one". `source_id`, `part` and `parts` on each row.

The ordering was wrong too, and its comment said otherwise: it ranked
individual FILES by size while claiming to be "the same order playback picks
in", so a two-CD film came back cd2 first. It is playback's own clause now.

### B5. Lost-update guard on user library grants — DONE

UI-25. Two admins editing one user's grants silently discarded the first
write. `users.grants_version`, returned with every read and required on the
write; the check and the bump are one statement, so two writers cannot both
pass it. The loser gets 409 `stale_write`. Required rather than optional,
because a guard a client may omit is not one.

### B6. Track durations on album children — DONE

UI-4. `duration_ms` from the file's own probe, on every item row — distinct
from `resume_duration_ms`, which is what a player last reported and is exactly
what a track nobody has played does not have.

**Deferred, with reasons:** UI-1 (artist entity — a migration and an
enrichment path, and the owner already decided albums), UI-3 (AniDB title
refresh — no plumbing behind it), UI-23 (which files a mediahost could not
read — needs somewhere to store them first).

Each gap lands as its own commit with hub tests, before any Vue is written.

## Phase 2 — clean room — DONE

`web-next/`, beside `web/` (owner decision: the old UI stays readable and
runnable for the whole rebuild, because its comments are the specification),
running against the dev hub through Phase 0's proxy. Vite, Vue 3, Tailwind,
oxlint, oxfmt, Vitest, Orval pointed at the
same `openapi.json` and the same fingerprint check. Nothing but a shell that
boots and a green test run.

Tailwind's theme is the custom properties already at the top of `styles.css`,
ported name for name into an `@theme` block — which emits the same properties
AND generates the utilities, so `--color-bg` is both `var(--color-bg)` and
`bg-bg`. One definition, two ways to reach it.

**TypeScript 5, not 7, and only here.** `vue-tsc` patches `typescript/lib/tsc`,
and TypeScript 7 — the native port `web/` uses — does not export it, so
`vue-tsc` cannot run at all: measured, `ERR_PACKAGE_PATH_NOT_EXPORTED`. The
alternative was `tsc` plus a `declare module '*.vue'` shim, which typechecks
the TypeScript and gives up every prop and every template expression — the
half this stack was chosen for. Two independent npm projects may hold two
compilers; at cutover this one is the one that survives. Revisit when
`vue-tsc` supports 7.

The strict settings earn themselves immediately: `exactOptionalPropertyTypes`
caught `retryAfterSecs = undefined` on the first file written, where absent and
present-but-unknown are genuinely different answers.

**Check:** `npm run build` in `web-next/`, then
`kahawai hub --web-dir web-next/dist` — the shell, its hashed assets and a
client-side route all served by the hub, which is the loop phase 0 exists for.

## Phase 3 — foundations — DONE

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
  from remounting the player. The host is here (`composables/notices.ts`); the
  boundaries land with the shell in phase 4, which is the first thing that has
  a screen to key on.

What the session gained over the port: starting is explicit. The channel and
the transport wiring used to be module side effects, which is why testing them
needed globals stubbed before a dynamic import in the right order. The three
guards that matter — the generation check, clearing memory before the lock,
and the shared in-flight refresh — were each removed in turn to confirm the
tests go red; all three do.

## Phase 4 onward — page by page

In dependency order. Each is: read the old file's comments → tests → domain →
composables → components → view → review → commit.

| # | Page | Notes |
| --- | --- | --- |
| 4 | Shell, header, menus, routing — **DONE** | the two menus, the search box's two meanings, keyboard and ARIA |
| 5 | Auth / setup — **DONE** | including UI-24: a hub that never answers must not leave a dead form |
| 6 | Home (libraries, shelves, continue watching) — **DONE** | inline retries per shelf, the three loading states of UI-22 |
| 7 | Search panel — **DONE** | on B3; combobox pattern, `aria-activedescendant`, every key driven |
| 8 | Library grid — **DONE** | virtualised, fixed cell height (UI-11), `srcset` at both densities (UI-16) |
| 9 | Detail + Season — **DONE** | error split: a refused Play is not a failed item load (UI-13) |
| 10 | Settings — **DONE** | drag ordering with keyboard equivalents (UI-12), per-key write queue |
| 11 | Admin — **DONE** | optimistic writes with rollback throughout; B5; the match button, a library-grid control but the only surface that reads `match_confidence`; OPS-10's per-item and per-session logs, both authenticated downloads |
| 12 | Album queue — **DONE** | survives navigation; per-track removal (UI-2); the album page's two actions, which are queue operations and were absent until it existed |
| 13 | Video player — **DONE** | largest and last; see below. Plus the capability debug panel, on the item page as well as in the player, and the subtitle panel (HUB-21/24) |
| 14 | Accessibility pass — **DONE, except the part a person has to do** | UI-17: the structural half is here and under test; a keyboard-only run and a screen reader are still owed |

### What phase 4 landed

The frame, the two menus, the one search box, the router, and the error
boundary phase 3 said would arrive with the first screen to key on.

Three things came out of it that are worth carrying forward:

- **`role="menu"` is a promise.** The old header had the role and none of the
  keyboard behind it. A menuitem puts a screen reader into focus mode, where
  its own browse keys stop working — so the role without the arrow keys leaves
  that user with nothing that moves. The rebuild implements the pattern:
  focus into the menu on open, arrows with wrapping, Home/End, and focus back
  to the trigger on close. Asked *before* the DOM updates, because the
  watcher runs pre-flush and `activeElement` is still the row that is about to
  be removed.
- **A comment recording an incident is a specification.** Two of the defects
  found in review were places where the port dropped a rule whose reason was
  written beside it: the search box's `z-index: 16` above the menu sheet, and
  the notice's `pointer-events: none`. Both had the incident in the comment
  and neither had a test.
- **A vacuous test can invent a fact.** The debounce test used a default
  (`flush: 'pre'`) watcher, which coalesces two writes in one tick into one
  callback — so the intermediate value it existed to forbid was unobservable,
  and a mutation removing the `clearTimeout` passed. It had already been used
  to conclude that a second `clearTimeout` was dead code. The conclusion
  survived re-measurement with `flush: 'sync'`; the instrument that produced
  it did not.

### What phase 5 landed

The four states before there is an app — nothing, a hub that did not start, a
way in, the app — plus the first-run setup form and the sign-in form.

- **UI-24 is closed for this UI.** `browserLogin` now carries a real
  `AbortSignal` rather than racing a timer: a lost race leaves the request
  running, and a login that lands after the person was told it failed installs
  a session they were never offered. The form re-enables in a `finally`, which
  is the other half. **`web/` still has the bug** — the fix is in
  `web-next/src/api/session.ts` and the old client has its own copy of that
  code, so UI-24 stays open in the checklist until cutover.
- **A timed-out request now says so.** `Offline` covers both "not reached" and
  "reached and never answered", because nothing downstream can act on the
  difference — but on the sign-in screen the sentence is the only thing the
  person has to go on, so the two no longer read the same.
- **The sign-out ordering was reintroduced and then caught in review.** The old
  `App.tsx` signs out in two steps and says why: navigating first unmounts the
  player while its bearer still works, so its final progress report lands. The
  first draft here cleared the tokens first. There is now a test that fails on
  that order, which is what the comment should have had all along.
- **Labels.** The old form named its fields with placeholders alone, so the
  name vanished as soon as anything was typed. Real `<label>` elements now, and
  the password rule is `aria-describedby` rather than a loose paragraph.
- **A login that was superseded is no longer a success.** `installAccess`
  returns false when a peer tab signed out mid-flight; nobody read it, so the
  app rendered signed in with no bearer and only a reload recovered. Inherited
  from the old client, which still has it.
- **Two measurement traps, both recorded in the tests that hit them.** A spy
  asserting `AbortSignal.timeout(20_000)` passed with the login deadline
  removed, because the auth lock asks for 20 seconds too. And the form's
  `@submit.prevent` could be deleted without failing anything — a form with no
  method then does a native GET, putting the password in the address bar.

**For phase 12:** the old cleared-tokens handler also does `setQueue(null)` —
the next account must not inherit tracks it cannot read, and `AlbumPlayer`
retries a 403 for ever. There is no queue yet, so there is nowhere to put it;
it belongs in that handler when there is.

### What phase 6 landed

The home screen: your libraries, what you are part-way through, and one shelf
per library. Plus the query client, the artwork component, the sideways lane,
and the claims reader behind the header's name.

- **The rule the screen exists to keep:** a library that would not load is not
  a library with nothing in it. The empty one is dropped, the failed one keeps
  its heading and offers the button, and neither is judged until it has
  answered.
- **The review found four defects and nineteen blind tests.** The defects: a
  page still in flight when a shelf was retried spliced itself onto the fresh
  first page (the heading then read "6 of 1" and the shelf never asked for
  anything again); the lane never re-read its edges after cards were appended,
  so the arrow that fetches the next page stayed disabled over a lane that had
  just grown; a failed continue-watching row was silent, which reads as having
  nothing on the go; and the libraries query ran before there was a session,
  which is a guaranteed 401 on every sign-in screen.
- **The blind tests were all in the two components that had none.** `Art.vue`
  and `Lane.vue` could each have had six behaviours deleted without failing
  anything, UI-16 and UI-22 among them. Both have mounted tests now, with the
  lane's measurements stubbed onto the element — happy-dom does no layout, so
  every `scrollWidth` it reports is zero, which is also why a lane in a test
  always asks for a second page on mount.
- **The CSS port had lost more than it looked like.** The card had no box, the
  poster had no aspect ratio (so a row of stills and posters came out ragged
  and every card grew as its picture arrived), the swell placeholder was never
  copied across, the progress bars had gone from sand to teal, the scrollbar
  was back under every shelf, and the lane arrows were `display: none` until
  hover — which takes them out of the tab order, leaving no keyboard path to
  the arrow that pages the shelf.

### What phases 7 and 8 landed

The cross-library search panel and the virtualised library grid.

- **A panel labelled for one query, showing another's hits.** `keepPreviousData`
  is what keeps rows actionable between two keystrokes, and it hands the last
  result set to the next key — including across an emptied box. The panel read
  "Results for zzz" over the hits for "heat", and two arrow presses and Enter
  opened a film out of them. It now tracks which query the rows belong to, and
  that is also what the label reads.
- **Two reproduced defects in the grid.** Re-sorting left ten cards inside a
  full-height container — scrolling to the top when already there fires no
  scroll event, and a re-sort changes neither the total nor the metric, so
  every path that would have recomputed was watching something that had not
  moved. And clicking a card that had not arrived threw, because the click
  handler was on the cell rather than on the card and the placeholder is not a
  card.
- **The cell height invariant had quietly broken.** `metaLine` is empty for a
  film with no year, an empty span takes no line box, and one short cell makes
  its whole grid row short — while `reservedHeight` multiplies ONE measured
  row pitch by every row. The dash is back, and so is the two-line clamp.
- **Where the measurements come from.** The grid measures its own columns and
  cell height, which a test environment cannot produce — so the arithmetic
  lives in `domain/virtual.ts` and the mounted tests stub the two numbers.
  Before that stub, six behaviours could be deleted without failing anything,
  including the scroll listener and the measurement itself.
- **A blind spot worth stating:** happy-dom drops an `aspect-ratio` whose value
  is a `var()`, however it is written, so "the placeholder is the same height
  as the card" is checked by its class and by eye, not by its value.

**Deferred, not lost — and landed in phase 11:** the admin match button. It is
a library-grid control, but it is the only surface anywhere that reads
`match_confidence` — a library without it is a library where nothing looks
wrong — so it went with the rest of the admin surfaces.

### What phase 14 landed

UI-17's structural half, and a standing test that stops it coming back.

- **A skip link, and something for it to skip to.** A keyboard user landing on
  a library page walked the search box and two menus before reaching the first
  card, on every navigation, because the focus returns to the top each time.
  The target carries `tabindex="-1"`, or a fragment link scrolls and leaves the
  focus where it was.
- **Focus follows a screen change, and only a screen change.** A real
  navigation puts the focus at the top of the new document; this one does not,
  so pressing a card left the focus on a button that no longer existed. Keyed
  on the same value the error boundary uses, so the player's autoplay handover
  — which changes the URL and nothing else — does not take the focus off
  whatever the viewer was reaching for.
- **A title per screen, and one announcement.** The browser announces a
  document title on a real navigation and has no idea this one happened. The
  title is set twice per navigation — once for the screen, once for the thing
  on it — and announced once: the second is the same screen a beat later, over
  whatever the reader had moved on to.
- **Every control has a name**, checked on five screens by walking the tab
  stops. It found one: the bandwidth cap, which had a placeholder and no label.
- **One `h1` per screen, and no skipped levels.** Heading navigation is how a
  screen reader user finds where they are, and the home screen answered with
  nothing.
- **A heading that looked pressable and was not.** The library's title drops
  the filter — a mouse-only shortcut, because a heading with a click handler is
  unreachable. It is a button when there is something to drop, and plain text
  when there is not.
- **The two dialogs on the item page are modal in the way the attribute
  claims.** Focus moves in — an `alertdialog` is announced when the focus
  arrives, not when it is inserted — Tab stays inside, Escape works from
  `<body>`, and the focus goes back to whatever opened it.
- **`prefers-reduced-motion`.** Every animation here is decoration and every
  animated element says what it is in text, so the honest answer is to stop.

**What the pass still owes, and cannot be automated:** whether an announcement
is intelligible, whether the focus order matches the reading order, and whether
anything is legible at 200% zoom. `test/a11y.test.ts` says so at the top.

#### What driving the running app found afterwards

The bullet above — "a title per screen, and one announcement" — was true of
`documentTitle` and of nothing else. Signed in against the live hub, three of
seven screens showed the bare word `kahawai` in the tab strip and announced it
out loud, because no view ever called the function with a name. The pure
function was right and thoroughly tested; nothing asked whether it was ever
reached. A test of a function is not a test of the behaviour.

- **The title is published by the view, tagged with its screen.** Only the
  detail view learns an item's title, and it learns it a round trip after the
  route changed. Untagged, that publication is read by a `pre`-flush watcher
  that runs BEFORE the outgoing view is torn down — so arriving on an item
  announced the LIBRARY's name, and the item's own title, landing a beat later,
  was swallowed as a repeat of a screen already announced. The tag makes the
  mismatched pair unrepresentable. Both defects were found by a review in a
  clean context, after the first fix; the second was worse than the first.
- **The gate is a screen.** A session that ends on its own leaves the route
  alone, so the router still says `libraries` while a sign-in form is on
  screen, and a boot error can land over a running app. Titled by what is
  actually rendered, in the order the template renders it.
- **A screen that FAILED is still a screen.** The screens with no word of their
  own wait for the thing that titles them; when the request fails, that thing
  is never coming, so the state with the most to explain had no title and got
  no announcement. Each publishes what its failure panel says — except a season
  whose show details went missing, which is a notice over a page of working
  episodes and not a failure at all.
- **The player had no `h1`.** The only thing on the screen is the picture, so
  there was nothing to make a heading out of and none was written. It was also
  the one screen missing from the audit's list, which is why the standing test
  did not say so. Outside the branch, so a refusal keeps it.
- **A name is not the same as a good name.** The search panel's group rows are
  buttons whose content is a library's name beside a bare number, so the
  accessible name was "3d 1" — named, and the audit's "every control has a
  name" rule passes, and it still tells a screen reader nothing.

Two findings from the same session were **retracted after measuring**: the
player's controls do reveal on focus (the browser harness never dispatched the
event — a listener counted zero), and two tabs cannot race a refresh, because
`session.ts:alone` holds an origin-wide Web Lock. Raw `fetch` calls bypassed it
and revoked the family, which measured the hub rather than the app.

- **An arrival is an address, not a route name.** Armed on the name alone, the
  navigation a viewer does most — pressing an episode on a series page, a
  related film, the next season — was silent, because an item and the next item
  are one screen. It arms on the error boundary's key instead, which already
  draws exactly this line and for the same reason: the player's autoplay
  handover changes the URL and is not somewhere the viewer went. That made the
  publication's tag the address too, which is what it should always have been —
  and deleted four hand-typed screen literals, the thing most likely to rot.

**Known and not fixed:** back and forward between two `/play` addresses in one
library is not announced, because the player's key deliberately holds still
across a handover and cannot tell the two apart. The title is right.

### What phase 13 landed

The player: the route that owns a session, the picture that plays one, the
three subtitle renderers, and the capability mask editor — which lands on the
item page in the same change, because a mask only takes effect on the NEXT
session and has to be settable before Play is pressed. Plus HUB-24's subtitle
panel, which is the other half of the same subject: the player picks a track,
the item page manages which tracks there are.

- **Two components, and the split is the design.** The ROUTE holds the session
  and the frame; the PICTURE is keyed on the session id and replaced by every
  restart, stand-by resume, capability change and next episode. That is what
  keeps the window and the way out of it from blinking while the thing behind
  them is rebuilt — and it is why releasing a session belongs to the route:
  the picture's teardown runs whenever Vue feels like it, and ending a session
  is the one irreversible act in it.
- **A `<script setup>` block is not module scope**, and the volume carried
  across sessions found that out. The body of `<script setup>` IS the setup
  function, so a `let` there starts over with every instance — an evening of a
  series at 15% became an episode at 100% at every boundary, which is the exact
  incident the reference recorded and fixed by putting the value at module
  scope. In an SFC that needs a second, plain `<script>` block. Caught by the
  test, not by review.
- **One health ref, no shadow.** The reference kept a reducer AND a ref holding
  the same value, because a React listener reads whatever the render that
  created it captured. A Vue ref has no such problem, so the shadow is gone and
  the comment says why it was there.
- **`steer` is the three restart paths' shared answer.** A seek, a track switch
  and a burn transition all POST, then write the origin, then re-attach — and
  they used to disagree about which failures meant what. One function now
  decides: a 404 is the recovery contract, a ceiling of `null` is weather and
  stands by, and anything else hands the retry back rather than leaving the
  picture frozen with the keepalive holding the session alive.
- **What happy-dom cannot do, said out loud.** It does not fire `timeupdate`
  when the playhead is moved, does not fire `volumechange` for a volume it
  accepted, and has no media to start — so a `play()` that resolves has to be
  stubbed wherever the veil is the subject. Each of those is done by hand in
  the test that needs it, with the reason next to it. hls.js is replaced by a
  recorder: the picture's own logic is what is under test.

Reviewed in a clean context before this commit. What it found:

- **A session per route-param change, released by nobody.** Back and Forward
  across two `/play` entries reuse the component with a new id, and `start`
  overwrote the session without ending what was there. Four of those and the
  account is at its per-user cap. There is ONE writer now — a `watch` on the
  session — and it is `flush: 'post'`, because the picture posts the final
  position in its teardown and a pre-flush release beat it: the report went to
  a session the route had just ended.
- **The track switch stopped reverting, and remembered a track that never
  played.** `steer` returned a boolean nobody could read, so a 404 left the
  selector naming the new track and recovery opened its session on it —
  Japanese audio under a selector reading English. It returns an outcome now,
  and the revert runs BEFORE the recovery it would otherwise race.
- **Four of the five session starts dropped the bandwidth cap.** `prefs` had a
  default of `[]`, which is not nullish, so every recovery, every hand-pressed
  Try again, the capability restart and the stand-by resume started uncapped —
  on the metered link the setting exists for. Saying so is now `'read'`.
- **The `.videobox` CSS was half-ported.** No stacking context, so a dialog at
  `z-9` sat under the shell's menus; no `bar-up` transform, so the ASS and
  bitmap canvases stayed under the transport; the ratio pinned to the box
  rather than `auto var()` on the video, so a probe that disagreed with the
  file letterboxed for the whole film; and no `max-height`, so at 1080p the
  transport started below the fold.
- **The player chunk did not go through `loadChunk`.** A tab that outlives a
  hub upgrade asks for a name the new binary does not have, and the Play button
  did nothing, silently, for ever.
- **UI-17 findings on the one screen with no page furniture.** The seekbar was
  a `div` with a click handler — not focusable, no value, no keys, so the only
  transport a keyboard user had was ±10/+30 and "skip the recap" was
  unreachable. It is an `<input type="range">` now, which is what the volume
  control beside it already was. Both dialogs were `aria-modal` and neither was
  modal: nothing moved the focus in, so an `alertdialog` announced nothing, and
  six controls behind the scrim stayed in the tab order. The hidden bar was
  still tabbable, and the two nudge buttons announced as "10" and "30".

**Three guards in `Picture.vue` are unreachable and stay**: `settle(gen)`
rather than `settle(seekGen)`, `giveUp`'s generation check, and `beginRestart`
re-arming rather than stacking a ceiling. All three answer two restarts being
outstanding at once, and `seekTo`, `switchTracks` and `switchBurn` all refuse
to start while `isFrozen` — which reads the current health, where the reference
read a React closure that could be stale. `syncOrigin`'s two checks are
likewise redundant with each other; either alone is enough, so a mutation of
one is masked by the other.

### What phase 12 landed

The music dock: a queue that outlives every page, gapless handover between
tracks (HUB-19), ReplayGain, per-track removal (UI-2), and the record page's
two actions.

- **Where it is mounted is the design.** Inside the shell and outside the
  router: it survives navigation, which is the whole point of a queue, and it
  must unmount when the session ends while the bearer still works — its
  teardown DELETEs two sessions, and an unauthenticated one leaks a slot. It
  has its OWN error boundary, because it has the longest life and the most
  state to get wrong, and a throw in it outside one was a white page with no
  header and no way back. That boundary is keyed on a GENERATION rather than on
  the first track: putting the same record on again produces the same first
  track, so a caught throw stayed caught and Play did nothing at all, silently.
- **Two elements, not one.** Preparing the next track once the current one ends
  costs a session start plus a buffer fill — audible on every boundary, worst
  on a record mixed to run continuously. The idle element holds the next
  track's session and buffers it; `ended` is a `play()` on something already
  loaded.
- **Every session is claimed by its TRACK, never by its index.** Index 5 of one
  record counted as a match for index 5 of the next, so the dock went on
  playing the album you had left.
- **The refusal ladder is gone**, and this is the phase that proves the backend
  gap was worth closing: the old queue counted attempts and gave up after
  three, because 409 meant both "no sources, ever" and "too many streams,
  close one". `unplayable` (409) and `session_cap` (429) are now distinct, so
  `retry` in `api/errors.ts` decides it — a stream cap is waited out for as
  long as it takes, and an unplayable track stops asking at once.
- **A retry per slot, armed by the failure that needs it.** The old client
  re-armed one shared effect through a counter, because an identical error
  string is not a state change any dependency can see. Vue's reactivity does
  not need the counter, and each failure scheduling its own timer is what the
  counter was standing in for.
- **What the tests could not otherwise tell apart.** Four mutations survived
  the first pass and each named a real hole: counting session starts cannot
  distinguish "handed over to a dead slot" from "restarted it", so the audible
  ELEMENT is what is asserted; a playhead already at 42 seconds cannot show
  that it was restored, so the element is reset first; and two of the four were
  the harness lying — a mutation that left unbalanced braces reads as
  "survived" unless the runner is asked whether it ran at all.
- **The dock must be unmounted between tests.** The queue is at module scope on
  purpose, so a dock left mounted by an earlier test goes on reacting to the
  next test's record and starts sessions for it.

Reviewed in a clean context before this commit, and it found the following.

- **The ceiling on asking again came back, split by code.** Dropping the old
  three-try ladder was right for `unplayable`, and wrong for the two conditions
  that can be permanent: a 500 is the hub answering that it failed, and a
  `session_cap` can be one this very queue is holding — it takes two sessions,
  so with a low enough per-user cap the album's warm slot is refused by the
  album's own active one and the condition can NEVER clear. Unbounded, that tab
  posted a session start every five seconds for as long as it was open.
  `startCeiling` in `domain/recovery.ts` is the rule: no ceiling for weather, a
  minute for the cap, three for a 500.
- **One keepalive clock per slot.** Rebuilding both pings whenever either
  session changed reset the AUDIBLE slot's stall counter every time a preload
  landed, so a paused queue with a retrying preload beside it bought itself a
  fresh half hour indefinitely — which is the whole thing `IDLE_LIMIT_MS`
  exists to stop.
- **An element whose `src` attribute is REMOVED is not defined to stop.** Both
  `<audio>` elements always exist now — that is what stops the gapless handover
  swapping to one Vue has just created — so releasing a session takes the
  attribute off an element that may still be playing. The media load algorithm
  runs when `src` is set or changed, not when it goes away. `release` pauses it.
- **A suspended AudioContext had no way back.** Both elements feed it, so while
  it is suspended there is no sound at all; the reference retried `resume()` on
  every render and the port retried it only when a session changed.
- **The bottom reserve is conditional and generous again.** A flat 88px against
  a bar that wraps to two rows at 375px, and grows another when it has an error
  to show: the last row of a page sat under it, and its buttons hit-tested to
  the dock.

**Four guards in `QueueBar.vue` are unreachable and stay.** The loser check's
`|| slots[which].session`, the handover's key comparison, `arm`'s
`clearTimeout` and `onEnded`'s `release(finished)` all restate an invariant
something else already maintains — the claim, the orphan watcher, the call
graph. Each encodes an ordering between a watcher, an interval and a DOM event
that the function holding it does not control, and each one being wrong costs a
leaked session against a per-user cap of four. They are recorded here rather
than deleted, and the mutation sweep is expected to survive them.

**A blind spot worth stating:** happy-dom has no Web Audio and does not act on
the `autoplay` attribute. The first is stubbed, so the gain node, the
once-per-element wiring and the factor are covered; the second is not, so two
recovery tests play the element by hand where a browser would have done it, and
say so.

### What phase 11 landed

The operator's panel — the fleet, the composer, the providers, the accounts and
the sessions — plus the two surfaces that are admin controls living elsewhere:
hand-matching from the library grid (HUB-8) and the per-item session log
(OPS-10).

Reviewed in a clean context before this commit, which found twenty-one things.
The ones worth keeping in writing:

- **An optimistic override nobody released.** The view owned the overlay and
  `useGrants` only ever wrote to it, so from the first grant click a row was
  frozen until a reload: another admin narrowing that account changed nothing
  on screen. The version override starved the `stale_write` re-read of the
  fresher version that is the entire point of it, so every later click on that
  row was refused for ever, on an idle hub, blaming an admin who was not there.
  The overlay now belongs to the state machine that knows when a write is
  outstanding, and the re-read that releases it runs BEFORE the release — the
  row underneath predates the write, so dropping it first flicks the chips back
  for the rest of the polling interval.
- **A `Failed` that could not render.** `loaded` asked whether any query was
  still pending; a query that has ERRORED is not pending either, so the panel's
  only Try again was behind a condition that could never hold at the same time
  as an error. `api/query.ts` justifies its three-attempt cap with "every
  screen offers Try again"; this one did not.
- **Empty states that lied.** Six separate reads mean one can fail while the
  others are fine — and each accessor fell back to `[]`, so a 503 on
  `adminSessions` rendered "Nobody is playing anything" as a statement. Each
  section now asks whether ITS read failed.
- **`act` waited for six requests.** Everything after a mutation — clearing the
  form, disarming the confirmation, saying what happened — was behind a full
  re-read, so on a slow hub the create form kept its values with Create still
  live, one Enter from creating the account twice.
- **A confirmation that threw focus away.** Swapping the Delete button for a
  question destroys the focused element: focus falls to `body` and a screen
  reader is told nothing. One button whose LABEL changes keeps both. The
  library delete was not armed at all, and it is the most destructive of the
  three — it cascades every grant, and re-creating the library mints a new id.
- **Disable on a mediahost.** The hub takes the flag for any satellite and only
  transcoder placement reads it, so draining a mediahost gave a 204, a
  persisted flag, and a host still serving every byte.
- **Chips disabled for the round trip.** The `SerialQueue` exists so a second
  click is ORDERED behind the first; disabling swallowed it instead, and took
  the just-pressed button out of the tab order with nothing announcing why.
- **The refusal ladder is gone.** The queue used to count attempts and give up
  after three, because the hub said 409 both for "no sources, ever" and for
  "too many streams, close one". `unplayable` (409) and `session_cap` (429) are
  now distinct codes, so `retry` in `api/errors.ts` answers it for every
  request in the app and `startRetry` does not exist here. (A CEILING came back
  in phase 12 for the two conditions that can be permanent — see below.)

Reviewed a second time after the rework, which found eight more. The ones worth
keeping in writing:

- **HUB-11's hint channel was missing entirely.** The panel was polling at
  fifteen seconds where the old one pushed at 250 ms, and nothing recorded the
  loss. `composables/hints.ts` is the one place the plan promised: it filters on
  the hint's KIND, because a scan emits one every five hundred files and can
  change only two of the six reads — every one of them used to re-read the
  accounts and the provider credentials as well.
- **A re-read that threw froze the row for ever.** The release of the
  optimistic override sits behind an `await` in a `finally`; a rejection there
  skipped it, which is the same incident this phase was reworked to fix,
  restored through the error path — and it rejected a promise neither caller
  awaits.
- **The attach menu kept the row that had just been refused.** It is a menu, not
  a value: leaving the picked collection selected shows one that is not
  attached, and the operator cannot re-pick it to try again.
- **Two presses of Disable sent the same value twice.** The row does not move
  until the re-read lands, so "disable — no wait, enable" was silently one
  write. It reads a per-satellite override now, the same shape the grants use.
- **The match dialog's search raced, and Escape stopped at the backdrop.** The
  grid an operator clicks is the one they are looking at, and clicking it
  applies a match; and clicking any prose in the dialog puts the focus on
  `<body>`, where a handler bound to the dialog's subtree never sees the key.
- **Downloading a log went through `act`.** That is for writes: it re-reads all
  six lists and clears the last refusal on success, so a read-only download
  wiped an error the operator had not read.
- **Three disabled controls explained themselves only in a `title`.** A
  disabled button is out of the tab order, so the sentence was in the one place
  a keyboard or screen-reader user can never reach it. Two say it in text now
  and one is `aria-disabled` and inert.
- **The tabs pattern: focus follows the selection.** The panel is focusable but
  NOT focused — focusing it took the focus off the tab the arrow keys had just
  moved to, which is the opposite of what a roving tablist promises.

### What phases 9 and 10 landed

The item pages — film, series, episode, season — and Settings. Plus the
capability probe, which the item query needs to ask what THIS client would be
served.

- **Two ways to destroy a page somebody was reading.** A failed background
  re-ask replaced the item page with "Could not load this item" — over a mark
  that had just succeeded, since a tick invalidates that query. And the season
  page had the fatality split backwards: the episodes ARE that page, and it
  treated the title on the back button as the fatal load instead.
- **The probe threw where the original did not.** It checked for
  `MediaSource` but called `isTypeSupported` unconditionally; a browser with
  one and not the other turned every item page into a failure, because the
  probe runs inside the query.
- **A capability mask is applied whether or not anything says so.**
  `buildProfile` reads the stored mask today, so the badge saying "masked"
  could not wait for the debug panel — a mask set in the old UI silently
  changes what this page reports about your playback.
- **The TMDB attribution is a term of use**, not a design choice; it was
  dropped in the port along with the three other providers' credits.
- **The settings' hard part is the revert target.** Two changes in flight and
  one failing: reverting to the value the failing write started from puts the
  list back past a change that was saved. The target is the last value the
  server CONFIRMED, and only the newest write may use it.

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

## Cutover — **DONE**

`web/` was deleted and this directory took its name, in one commit, with every
row above done. What moved with it, because it is shared build machinery rather
than part of either app: `openapi.json`, `scripts/openapi-fingerprint.mjs` and
`public/favicon.svg`. What had to change: the two paths that pointed at `../web`
(the fingerprint gate and Orval's input), the package name in `package.json` and
its lockfile, and `api:export`, `preview` and the `scripts` directory in the
lint and format globs — all carried over from the old `package.json`, which had
them and the new one did not.

Nothing outside the directory needed a path change. The hub's `build.rs`,
`rust-embed`'s `#[folder]`, `api.rs`'s `include_str!`, both CI jobs and both
release scripts address `web/` by name and by npm script, and the new tree
answers to all of them — which is what the rename was for. `build.rs` gained
two watched inputs it did not have: `openapi.json` and `orval.config.ts`, since
the client is regenerated from them on every build.

Requirements this closed, each updated in the same commit: UI-24 (a sign-in that
could hang), UI-26 (twenty-five suppressed hook warnings — the rule is gone, not
the warnings), UI-27 (a multi-part film reading as seven alternative encodes),
and the machine half of UI-17.

## Review

`/code-review` on the working tree before every commit. A PR-wide
`/code-review ultra` after the branch is complete — that one is
maintainer-triggered.
