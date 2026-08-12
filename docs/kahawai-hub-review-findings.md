# Hub findings from the UI branch review

Five rounds of adversarial review of `ui-redesign` turned up fifteen things in
`crates/` rather than in `web/`. Most are a report rather than a patch: it is a
frontend branch and `auth.rs` in particular has live AUTH work in it.
Three are exceptions, marked where they appear, and the reason is the same in
each case — leaving them would have meant shipping a client built on the broken
behaviour. Item 5's link teardown, its transcoder twin, and the admission slot a
dropped request used to keep.

Two are ours in origin: AR-6 gave the hub a new reason to end sessions from a
link-loss path, which widened windows that already existed. Those are marked.

Ordered by what would hurt most. Items 6 to 8 came from a second review round
after the first five were written, and 9 and 10 from a third; the fixes each
round produced are already on the branch, and what is left here is what belongs
to `crates/`.

**Citations are on symbols, not line numbers.** Every line number in the first
draft of this document had rotted by 4 to 227 lines within two days — `api.rs`
line 1640, cited as the session-start refusal, had become the middle of
`put_pref`. A function name survives an edit above it.

---

## 1. `delete_user` can still take the last admin away — FIXED

`crates/kahawai-hub/src/auth.rs`, `Auth::delete_user`

Deletion now uses `BEGIN IMMEDIATE` and the same guarded-statement shape as
`set_admin`: the admin count and delete are one SQLite write-lock decision.
`DeleteUser` reports deleted, absent and last-admin outcomes without parsing an
error string. `auth_api::delete_racing_demotion_keeps_an_admin` releases a
delete and demotion together for thirty rounds and proves exactly one
admin-removing operation wins each round. The stale restart comment was replaced
with the correct invariant: ordinary users do not make a zero-admin hub enter
setup mode, before or after restart.

---

## 2. A refused session start hands the client the error chain

`crates/kahawai-hub/src/api.rs`, `session_refusal`

```rust
(status, format!("{e:#}"))
```

`{e:#}` is the whole anyhow chain. Whatever `Sessions::start` failed on —
module ids, collection ids, scratch paths, the underlying io error — goes to
the client verbatim, and the web player puts it on screen (`capsError`
renders it, and stand-by shows it when a retry gives up).

Every other public shape here is deliberate about this. Suggest keeping the
chain in the log and returning the outermost context only, or a fixed string
per status.

---

## 3. `Artwork::inflight` is only ever inserted into

`crates/kahawai-hub/src/artwork.rs`, the `inflight` field of `struct Artwork`,
with the two insert sites in `original` and `remote_poster`.

```rust
inflight: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
```

`map.entry(cache_key.clone()).or_default().clone()` on the way in, nothing on
the way out. One `String` plus one `Arc<Mutex<()>>` per distinct artwork key
ever requested, held for the life of the process.

Bounded by library size rather than unbounded, so this is growth, not a
runaway leak — but a large library plus the poster variants makes it a
permanent resident, and the map exists only to dedupe concurrent fetches of a
key that is usually cached on disk within a second. Dropping the entry when
the last holder releases it would keep it to what is actually in flight.

---

## 4. A reschedule racing a session end orphans the new encode

`crates/kahawai-hub/src/sessions.rs`: `Sessions::reschedule_inner` against
`Sessions::end`.

`reschedule_inner` releases the old slot, reserves a new one, and then awaits
— a `watch_state` query, possibly `open_part_leases`, then `start_transcode`.
Only after all that does it publish the result:

```rust
self.start_transcode(registry, &new_tc, id, plan, parts, idx, local_ms, "", sets, ass).await?;
*transcoder.lock().unwrap() = new_tc.clone();
*reserved = None; // running now; the session owns the slot
```

If `end(id)` runs during that await, it takes the session out of `active`,
reads `transcoder` — still the *old* box — releases that slot and sends
`EndSession` there. Then the reschedule finishes and starts an encode on the
*new* box for a session that no longer exists. Nothing will ever end it:
`end`, `end_for_module` and `end_for_user` all iterate `active`. `*reserved =
None` on the way out means the failure path will not give the slot back
either, so the new transcoder is down one slot permanently.

Also worth a look while you are in there: `reschedule_inner` calls
`registry.tc_session_ended(&old_tc)` at the top, and a concurrent `end` calls
it again for the same box.

**AR-6 made this easier to hit.** A mediahost link loss now ends that host's
sessions (`link_service.rs`, `end_sessions_on`, reached from `forget_link` and
from the `link` stream's teardown), which is a new concurrent caller
of `end` — and a fleet blip that drops a mediahost is exactly the kind of
event that also drops a transcoder. Previously the concurrent enders were
satellite deletion and account deletion, both operator actions.

Not run — verifying it means a test in this crate, which is why it is here
rather than fixed. The ordering is unambiguous from the code.

---

## 5. Link cleanup removed by name, not by connection — FIXED on this branch
       (both halves: mediahost and transcoder)

Recorded for the history, because the fix is in `crates/` and you will see it:
`Registry::unregister_link` removed by module id with no check that the link
being removed was the one the caller owned, and the teardown ran *after* the
worker drain. A host reconnecting during that drain had its fresh sender
deleted while `is_connected` stayed true, so its files were still offered,
`open_lease` failed with a plain error, and the client was told **409 — give up
on this item** about a healthy host, which also never got another manifest and
so never scanned again. AR-6 made it worse: the same cleanup ends sessions, so
the stale task ended the live connection's.

Now `Registry::unregister_link_if_current` compares the sender
(`Sender::same_channel`) and `link_service::forget_link` does every map
mutation before the drain, state before sender, so the worst instant reads
"absent" — a 503 stand-by — rather than "present but unreachable".
`tests/link_teardown_state.rs` pins the part that was easy to get wrong twice:
a disconnect must not clear the operator's drain, and deleting a satellite
must.

The transcoder link had the identical fault and was missed when the mediahost
one was fixed. `unregister_tc_link_if_current` closes it, and the by-name
version is deleted rather than left beside it. That one was worse in its
consequences: capabilities are sent once per connection, so deleting a
reconnected box's caps meant placement never chose it again until the transcoder
process itself restarted, and its load accounting was lost so the box could be
oversubscribed afterwards.

One more of the same shape, in `Sessions::start`: the per-user admission slot
was released by the statement after the await, which covers the early returns
inside and not the caller going away — an abandoned request drops the whole
future and runs no such statement. Four closed tabs mid-start and the account
could not begin anything until the hub restarted. It is a `Drop` guard now.

**What is left, and it is yours to weigh.** `run_local` — the in-process
mediahost — still calls the by-name `unregister_link` on its way out, and that
is correct for it, since there is no second connection to confuse it with. The
part worth a look is that `run_local` returning an error is not only reached on
shutdown: it logs, exits, and leaves the hub serving with its own library's
`is_connected` false and nothing to re-establish it, so artwork, subtitle and
scan paths for the hub's own collections stop working silently. The comment
there says as much. Not fixed here.


---

## 6. The anyhow chain reaches clients from three more routes

Finding 2 above covers `session_refusal`, which both `start_session` and
`seek_session` go through — so seek is already counted there, and an earlier
draft of this item double-counted it and said "five more routes". The same
`format!("{e:#}")` treatment, reaching a client on a route that is not
admin-only, is on:

| route | where |
|---|---|
| `item_query` | the CONFLICT it maps, and the `unavailable` field it puts on the item body |
| `transcode_file` | its NOT_FOUND |
| `item_subtitle_file` | two NOT_FOUND arms |

The admin routes do it too — `admin_approve`, `admin_set_chain`,
`subtitle_search`, `admin_create_user`, `admin_delete_satellite` and others —
which is a different risk and probably an acceptable one.

What the chain carries, from `sessions.rs`: `creating {dir}`, `writing {}`,
`binding {}`, `spawning worker {}`, and `pipeline worker exited at start
({status}): {tail}` — where `tail` is the worker's raw stderr.

This got sharper on our side rather than yours: the web UI now renders those
strings where it used to swallow them (`Failed.tsx`, and 22 toast sites), so a
viewer whose transcode fails is shown the hub's scratch layout, the worker's
path and GStreamer's stderr. We are not going to stop rendering hub errors —
they are usually the only clue anyone gets — so the fix belongs at the source.

The pattern to copy is already here: `item_artwork` logs the error and returns
a fixed `"artwork unavailable"`, citing SEC-WEB-7.

## 7. `seek` and `end` take a session id and no owner

`end_session` and `seek_session` destructure only `Path(id)`. `post_progress`
rejects `session.user_id != claims.sub` with 403. So any authenticated account
holding a session ULID can restart another user's pipeline or end their
session, while the third endpoint on the same resource refuses.

Bounded by ULID unguessability, and the ids only ever go to their owner — a
capability rather than an open door. But the asymmetry looks unintended, and
`end_session` is one of the ways a session dies with nothing recording who did
it.

## 8. Ours, recorded here because it is a rule about your constants

`web/src/views/AlbumPlayer.tsx` preloads the next track 30 seconds out, and
the comment justifying that number reasons from the hub's ~90 s idle reap. A
client picking a lead time from a server timeout is what
`no-backend-constants-in-clients` forbids, and lowering the reaper to 20 s
would break gapless playback with no error anywhere.

Mitigated in passing during this round — the preloaded session is pinged, so
the reaper is not actually racing it — which means the constant is now
load-bearing for nothing. Flagged so that nobody restores the reasoning if the
ping is ever removed. No action needed on your side; it is here so the rule
has a written instance.


---

## 9. `logout` reports success whether or not it revoked anything

`Auth::logout` matches the family on the presented token's own hash:

```sql
UPDATE refresh_families SET revoked_at = unixepoch()
  WHERE id = ? AND user_id = ? AND current_token_hash = ?
    AND revoked_at IS NULL
```

`rows_affected` is discarded and the route answers 204 either way. A malformed
token returns `Ok(())` before the statement runs, which is also a 204.

So a client whose stored refresh token has since rotated revokes nothing and is
told it worked, and the family stays valid for its full 30 days.

This one bit us, and the trap is worth spelling out because the client is the
thing doing the rotating. The endpoint needs a live access token, so an expired
one is answered 401; a generic retry-after-refresh wrapper then repairs that —
which ROTATES the refresh token — and retries with the body it built before the
refresh. The hub matches nothing and answers 204. Verified against a running hub:
identical 204s, and the family still accepted a `/auth/refresh` afterwards. Our
sign-out now does its own repair and re-reads the token per attempt, so the body
always carries the family's current token.

Nothing in the response distinguishes the two outcomes, which is the part that
belongs to you. Two ways out, and the choice is a product call rather than a bug
fix: revoke by family id and let the hash mismatch pass (a client asking to end
its own session does not need to prove which rotation it is on), or return the
row count so the caller can tell. We have not touched it.

## 10. Nothing invalidates the access token or the cookie on sign-out

Also not a defect, but worth stating because the web client now depends on it:
`verify` only checks signature and expiry, so the access token stays good for
its remaining life after `logout`, and the hub never clears `kahawai_token`.
Dropping the browser's copies is entirely the client's job — which it does — so
a copy taken beforehand outlives the sign-out by up to the token's lifetime.
That is the usual stateless-JWT trade and matches AUTH-2's framing, but it means
"signed out" is a statement about this browser, not about the credential.

## 11. Source rows do not say which part they are

The item body's `sources` array carries `path_rel`, `size`, `available` and
`revision`, ordered `height DESC, revision DESC, size DESC` — which for a part
set, whose rows share height and revision, comes out as largest-part-first.
`item_sources` has the part number —
there is a film in this library held as seven numbered parts on one host,
correctly folded, and playback assembles the timeline properly — but the client
is handed seven entries in an order that means nothing and no way to tell one
film in seven files from seven encodes to choose between. The detail page can
only say "7 sources", which reads as the second thing.

One field on the row would settle it: `part`, `null` for a whole-file source.
The number is already in the table and already read by the session assembly, so
this is publishing something you have rather than computing anything.

While you are there: that ordering puts the biggest part first, so part 7 can
lead. Ordering by `part` when any row has one would make the array readable
without the client sorting it back.

---

## Not a hub finding, recorded because there is nowhere better

`remux::concat_spike::concat_over_appsrc_yields_one_continuous_playlist` in
`kahawai-media` failed once in three `cargo test --workspace` runs on
2026-08-10, and passed three times in a row when run on its own, both with and
without that day's changes. `kahawai-media` does not depend on the hub, so it is
not the branch. Not investigated — a flake in a spike test is worth knowing
about before someone spends an afternoon on a red workspace run that is not
their fault.

## 12. A demoted admin can re-promote itself for as long as its token lives — FIXED

Every authenticated request now resolves administrator status from the current
user row and compares the token's durable `auth_version`. A role change bumps
that generation in the same statement, so the token that authorized a demotion
is rejected on its next request and cannot create or promote another admin. The
self-change guard and its stale-claim rationale are gone; self-demotion is safe
when the independent last-admin predicate permits it.

## Checked and NOT a defect: the cacheable artwork 404

Recorded because it looks like one and cost an afternoon to disprove. This branch
made a 404 from `item_artwork` cacheable for an hour when the URL carries
`?v=art_version`, and the worry was that `art_version` comes from provider
metadata — so dropping a `cover.jpg` next to the file would leave the card blank
for up to an hour under a URL that had not changed.

It does change. Local artwork goes through the chain like any other answer
(`enrich.rs` stores it as provider `local`), `store_answer` writes
`updated_at = unixepoch()`, and `resolved_metadata.updated_at` — which is what
`art_version` selects — is `MAX` over every answer the item holds, with a comment
saying it exists precisely so this moves when an answer lands. Verified on a copy
of a real database: inserting a `local` answer moved the item's `art_version`.

## 13. "No correct record" leaves the refused record supplying everything

`crates/kahawai-hub/src/providers.rs`, `reject_matches` against the field order
in `resolved_metadata_sql`.

Rejection removes the assignment and the pins, and keeps the answers on purpose
— but with no `item_match` row every answer has `not_chosen = 1`, and the field
subqueries only exclude `confidence = 'weak' AND not_chosen = 1`. An `auto`
answer from the refused provider is still a candidate, every candidate is tied on
the first sort key, and the top-ranked refused one wins every field.

So an operator who opens a mis-matched film and clicks "no correct record" gets
a changed badge and nothing else: the title, overview, poster, rating, premiered
date, genres and cast all still come from the record they just refused, and the
artwork endpoint still serves that film's poster. The comment claiming rejection
"drops local behind the chain" is describing a reordering that happens among
candidates who are all tied anyway.

## 14. Every scan batch does two whole-`items` scans inside the write lock

`registry.rs`, the orphan sweep in scan-sync and the identical pair in
`reconcile_files`. Both are global — no module or collection scope — and run once
per `upsert_files` call, so once per manifest batch whether or not anything was
orphaned. The second builds an ephemeral index over every `parent_id` in the
table.

On a large catalogue each batch takes the single SQLite write lock and pays two
full passes plus the repick trigger cascade for anything deleted, while
`post_progress`, `PUT /watched` and session starts queue behind it. It scales
with the catalogue rather than with what changed, which is why a rescan of one
collection can look like the hub hanging.

## 15. An item's media type, and so its whole provider chain, is picked arbitrarily

`providers.rs`: the `media_type` subquery is `LIMIT 1` with no `ORDER BY`.

A show with episodes in both a `series` and an `anime` collection — or a film
whose copies straddle two — takes whichever row SQLite happens to return. An
unrelated rescan that rewrites `item_sources` can flip it, the repick triggers
fire, and the assignment is re-ranked against a different chain. The item's
identity changes on its own: new provider, new title, new poster, and
`art_version` moves, with no operator action to point at. It wants a
deterministic tie-break.

---

## 9. The probe stores coded dimensions, so no client can predict a picture's shape

`streams.video[]` carries `width` and `height` as stored, which is the display
shape only for square-pixel sources. For anamorphic ones it is not, and nothing
in the payload says which a file is.

Measured on an anamorphic episode: the item answers `720x480` (1.50), the
`<video>` element reports the same coded numbers, and the browser lays it out at
1.79 — the display aspect from the container, which only the decoder sees.

Why it matters here: the player now shows its frame within about 15ms of a
`/play` URL, well before any media loads, and the box has to be given a shape.
The item's own answer arrives in time and would be exactly right for most of a
library; on DVD-sourced content it is wrong by the pixel aspect, and the picture
visibly resizes when metadata lands. We assume 16:9 instead, which is right more
often than the numbers we are given.

A sample aspect ratio, or a display width/height beside the coded pair, would
let every client size the frame correctly before the first byte of media. Any of
`sar`, `dar`, or `display_width`/`display_height` would do; ffprobe reports all
three.

Not urgent, and nothing is broken without it — a resize on the first frame of
playback is cosmetic. Recorded because the shape is knowable at probe time and
is not knowable anywhere else until the media is decoding.

## 16. One status for a refusal that is forever and one that clears in a minute

`session_refusal` maps every failure that is not `SourceOffline` to 409:

    let status = if e.downcast_ref::<crate::sessions::SourceOffline>().is_some() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::CONFLICT
    };

and its doc comment reads "409 says 'not with this item'… will refuse again
forever". That is true of "no sources" and "unplayable". It is not true of

    bail!("too many concurrent streams ({held}); close one first");

which is the hub telling the client the condition clears — and it does, as soon
as a session ends. Both arrive as 409 with the difference only in the prose,
which no client should branch on.

It matters for anything that plays a list rather than one item. The album queue
holds two sessions at once, so a film playing beside it can put the account at
the per-user cap; the next track is refused, and a client that believes the
documented contract stops asking for good. Ours now asks three more times before
giving up, which is a guess standing in for an answer the hub could give.

A distinct status would settle it — 429 for the cap, keeping 409 for the item
itself — or a machine-readable reason on the body. Either lets a client wait out
the one and give up on the other, without reading English.

---

---

## Fixed here, listed so the next reader is not surprised

Beside item 5's link teardowns and the admission slot: a torn artwork cache file
is now avoided by writing to a temporary name and renaming, since a half-written
original read back as a complete one for ever and this branch's fixed-string 500
made that a permanent failure for the item; a provider's "no poster" is
remembered for an hour so a coverless shelf cannot saturate the outbound gate;
the transcoder ack path no longer marks a reconnected link absent; and
`post_progress` stores no resume position for a track, with `0051` clearing what
was already stored — the played mark that the album page renders is untouched.
