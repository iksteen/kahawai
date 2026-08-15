# Kahawai web UI: design-vs-built ledger

The clickable prototype in `web-mockup/` was treated as the specification for
the web UI and ported screen by screen on `ui-redesign`. This file records
where the built interface and the prototype disagree, and why — so that a
difference is a decision somebody can look up rather than a bug somebody
rediscovers.

**The prototype is not in the repository.** `4c21edd` added `web-mockup/` to
`.gitignore`, so every claim below about what the prototype does is, to
anybody who clones this, unverifiable — they have the verdict and not the
evidence. That was a deliberate choice about repository weight, and it is
recorded here rather than quietly lived with: either commit the prototype, or
read this file as one team's account of a comparison nobody else can repeat.

A `[x]` here means the difference is resolved, not that a runnable check
proves it. The release gates live in `kahawai-readiness-checklist.md`, and
that file's stricter rule — implemented, exercised by a named check, verified
against the real runtime — is the one that decides a ship.

Three kinds of entry:

- **Narrower than the prototype** — the prototype shows something the built UI
  does not. Each says what it would cost.
- **No data behind it** — the prototype invented a field. The prototype is a
  design tool with sample data; it was never bound by the API.
- **Built past the prototype** — where following the prototype exactly would
  have been worse.

The prototype is a prototype. Where it was wrong about behaviour it was not
followed, and those cases are listed too.

## Narrower than the prototype

- [ ] UI-1 **No artist screen.** The prototype browses a music library by
      artist — artist cards, then an artist page listing that artist's albums
      with per-album Play and Queue. The built UI shows albums, as before
      (owner decision during planning).

      What exists: `items.norm_artist`, which album search already folds.
      What does not: any artist entity, any id to route to, and any source of
      artwork for the round avatars. Three ways out, cheapest first:

      1. Group by `norm_artist` client-side within a page. Cheap, and wrong
         across page boundaries — an artist whose albums straddle a page break
         appears twice.
      2. A folded-artist mode on the existing browse endpoint: group and count
         server-side, return synthetic rows keyed by `norm_artist`. No schema
         change, no artwork, and the id is a name — so a rename splits the
         artist.
      3. A first-class artist entity, with somewhere for MusicBrainz artist art
         to land. The only option that survives a rename and can carry an
         image, and the only one that is a migration plus an enrichment path.

      This is the one place the ported UI is knowingly less capable than the
      design it was ported from.

- [ ] UI-2 **No per-track removal from the play queue.** The prototype offers
      a × on each queued track. The queue supports replacing, appending and
      jumping; it cannot drop one entry. `AlbumPlayer` matches its warmed
      sessions to tracks by id, so removal is a state change it would handle —
      the omission is unbuilt UI, not a structural obstacle.

- [ ] UI-3 **No "Refresh titles" action** in the Providers → Anime card. The
      prototype offers one. There is no endpoint behind it: nothing on the hub
      re-fetches the AniDB title dump on request. Needs plumbing before UI.

- [ ] UI-4 **Album tracks show no duration.** The prototype prints a running
      time per track. A track's children carried watch state, not file
      duration, so the number was not in the response the track list is built
      from — and watch state is exactly what a track nobody has played does not
      have.
      **The API half is done**: `duration_ms` is on children and on a detail,
      from the files' own probes, summed across a source's parts and minimised
      across the sources that could actually play. The track list still prints
      no times, because nothing client-side reads it — `Item` in
      `web/src/api.ts` does not declare the field. Open until it is on screen;
      the rebuild's album page (phase 12) is where that lands.

## No data behind it

- [x] UI-5 **The user row's note.** The prototype shows `watch state: 214
      items` per account. No such count exists on `AdminUser`. The row shows
      `created_at` instead, which is real. If the count is wanted it is a
      `COUNT(*)` per user on `watch_state`, joined into `/admin/v1/users`.

- [x] UI-6 **Session throughput.** The prototype's sessions section promises
      "throughput is the realtime multiple of the producing pipeline" per row.
      `AdminSession` carries mode, streams, username and idle seconds — no
      throughput. The claim was dropped from the section's intro rather than
      shown as a blank. The transcoder does measure realtime multiples
      (HUB-36); they are reported per satellite, not per session.

- [x] UI-7 **Freshness on a pending enrollment.** The prototype pulses a
      badge for a CSR that "just arrived", from a `fresh` flag it invented.
      `PendingEnrollment` has no timestamp. The nav badge pulses whenever
      something is pending and you are not on that section, which is the
      signal that was actually wanted: something is waiting for you.

## Built past the prototype

- [x] UI-8 **Admin rights are settable.** The prototype draws the admin flag
      as a clickable toggle with no endpoint behind it. Built with a durable
      access generation: promotion/demotion invalidates old access immediately,
      so self-demotion is safe when another admin remains; the client refreshes
      into its ordinary role and returns home. The last admin may not be
      demoted. See HUB-10 and AUTH-3.

- [x] UI-9 **Marking a season is one request.** The prototype marks episodes
      one at a time. `PUT /api/v1/items/{id}/watched` takes a list, so a
      season is one call that either applies or does not — seven requests
      became one, and a half-marked season stopped being possible.

- [x] UI-10 **Lane arrows stay in place at the ends.** The prototype removes
      the arrow that cannot move anything. Removing it takes the target out
      from under the cursor mid-click, and the click lands on the card behind
      it and opens something. The dead arrow is disabled, not removed.

- [x] UI-11 **The library grid keeps a fixed cell height.** The prototype lets
      a card title wrap freely, which it can afford with eight sample items.
      The real grid is virtualised: it measures one cell and reserves the whole
      scroll height from it, so cells that differ in height make the reserved
      height drift as you scroll. Titles clamp to two lines.

- [x] UI-12 **Reordering is dragging, in all three places.** The prototype
      drags language pills and provider precedence. The subtitle-fallback
      ladder in Settings was arrows; it is a drag now too, sharing one
      `useDragOrder` hook. Rows take the arrow keys so the gesture is not the
      only way in.

- [x] UI-19 **A lost mediahost is a wait, not an error.** Nothing in the
      prototype covers a satellite going away mid-playback; it has one
      machine and it never drops. A transcoder's sessions already moved to
      another box (AR-6), but a mediahost's kept a dead lease and the picture
      simply stopped. They are ended now, the player pauses on the frame it
      reached, and a dialog says the machine holding the file has stopped
      answering while it retries every five seconds. One button, going home,
      because there is nothing else to decide. It resumes at the point it
      paused, not the point the failed restart was attempted from.

- [x] UI-20 **The picture says when it is waiting for a click.** Chrome will
      not start a video for a viewer who has not interacted with the page, and
      a reloaded player sat on its first frame with no sign of it. A play glyph
      in the middle whenever paused. The state behind it was wrong in exactly
      the case that mattered: a refused autoplay fires no `pause` event, so
      nothing corrected the initial guess — the transport button had the same
      bug and read "Pause" over a stopped picture.

- [x] UI-21 **Toasts carry no actions, on purpose.** UX-1 asks for
      "inline/toast retry states", which reads as a menu rather than a mandate.
      Auditing every notice site — 27 `notify` and 11 `showNote`, counted
      2026-08-10 — the test that discriminates is: **is the control that caused
      this still on screen?** For a failed watched-mark,
      a Settings write, a refused next-episode, the button is right there and
      pressing it again IS the retry — a toast button would duplicate it, five
      seconds before vanishing. Two are confirmations with nothing to retry, and
      the admin poll retries itself every fifteen seconds.
      The two cases where the affordance genuinely was missing — a failed shelf
      and continue-watching — were given inline retries instead, anchored to
      where the content is absent. A toast is a poor home for an action that
      matters: it is not attached to anything and it leaves.

- [x] UI-22 **Three loading states that used to look alike.** A card that has
      not arrived is a half-strength ghost. A picture still in flight is a
      half-strength slot — the image's own background, which it paints until
      its content lands and which is opaque, so it covers what is behind it. A
      poster that does not exist is the kahawai tilde at full strength, revealed
      because `.art-failed` hides the image and its background with it.
      An `onError` handler per image adds `.art-failed`, which is what tells
      the third state from the second; the browser distinguishes "in flight"
      from "arrived" on its own, but not "arrived" from "will never arrive".
      An earlier draft of this entry claimed no handlers at all, which would
      invite deleting the seven that make it work. Before, every unpainted image
      showed the tilde, so a slow page looked like a library with no artwork.

- [x] UI-23 **A mediahost says when it could not read files.** MH-8 has the
      host report unreadable files rather than skipping them silently, and
      "reported" meant a line in the hub log. A warn chip on the satellite now,
      summed across its collections, on the actions side where a transcoder's
      toggle is.
      A count and no more: `FileError` carries the path and the reason and the
      hub only logs them, so nothing could say WHICH files without somewhere to
      put them and an endpoint to read them. Scan progress also lives in
      memory, so this reflects scans since the hub last started rather than the
      library's history.

## Known gaps in the built UI

- [x] UI-13 **`Detail`'s error path was a dead end.** A failed load rendered
      the error string and nothing else. It offers Try again and a way back to
      the library now, and one `error` state that had been doing two jobs is
      split: a Play the hub refused used to replace the whole item page with
      "Could not load this item", which was false — the item had loaded, and it
      was the play that failed. See UX-1.

- [ ] UI-14 **Styled-subtitle overlap while the controls are up.** When the
      transport bar is visible the subtitle overlay lifts above it rather than
      the video resizing under it. Lifting is a temporary state and was
      accepted as the trade (owner decision); a resize would reflow libass
      mid-playback.

- [x] UI-15 **`oxlint` lints `web/dist`.** Fixed on `master` in `0a1b2ff`,
      which scoped the project's own command to the sources:
      `"lint": "oxlint --deny-warnings src test"`. Every lint run used to
      report warnings from minified bundles and a real finding had to be
      spotted among them.
      The other half is done too: `.oxlintrc.json` now carries
      `"ignorePatterns": ["dist"]`. Scoping the command was an exemption the
      tool could not see, so a bare `oxlint` — or an editor, which does not run
      npm scripts and puts lint diagnostics in the gutter — still walked twelve
      committed minified bundles. `dist` is now generated and ignored rather
      than committed: native Rolldown output differed between developer Linux
      and Ubuntu 26.04 despite the pinned Node/package lock, making a generated-
      diff gate demand a container merely to edit TypeScript. Release and image
      builds generate it before Cargo and fail if it is absent. The explicit
      source/test scope remains useful for a developer who already has a local
      bundle. `npm run fmt:check` and bare `oxlint` are clean.

- [x] UI-16 **Home-screen artwork was sized for a display nobody here has.**
      A first load fetched 59 artwork requests totalling 2.05 MB, against
      87 KiB of JavaScript once the player was split out and the hub started
      compressing. The resize endpoint was being used correctly — `thumb`
      (96 px) costs 2.4 kB a piece — and the whole weight was `card`: 480 px
      generated, 320x480 sent, displayed at **128x192 on a dpr-1 display**.
      2.5x per axis, 6.25x the pixels.

      480 was not wrong; it is right for a 240 px card at 2x. It was simply
      not what these shelves are. Fixed by offering both densities and letting
      the client choose: `SIZES` gains `card1x` at 320 px, and `artworkSrcSet`
      emits `card1x 1x, card 2x` for every element that shows a card — shelves,
      the library grid, both season stills and the detail poster. Density
      descriptors rather than `w`, because the CSS widths here are fixed and
      what varies between clients is the display.

      320 was chosen as the widest 1x use rather than the narrowest: the detail
      episode still is exactly 320 CSS px, so nothing upscales anywhere.
      Measured across all 94 card posters on the home screen, fetched fresh at
      both densities: 3,975.8 kB at `card` against 2,061.5 kB at `card1x` —
      **51.9%, so a dpr-1 first load drops from about 2.05 MB to about
      1.07 MB.** A 2x display is unaffected and still gets 480.

      Two things worth knowing for anyone re-measuring this. `naturalWidth` on
      an element with a density `srcset` is density-corrected, so a 2x pick
      reports half its real pixels and looks like a smaller image was served.
      And Chrome will reuse an already-cached higher-density candidate rather
      than fetch the 1x one, so a browser that has loaded these pages before
      keeps choosing `card`; with a cache-busted URL it picks `card1x` at
      dpr 1, with or without a `src` attribute.

      **Not pursued beyond this, by decision.** `loading="lazy"` is on every
      img and does defer, but Chrome's threshold is its own: on a first load
      36 posters arrive from shelves below the fold. Deferring whole shelves
      behind an observer would stop that, and would also save six of the seven
      shelf fetches — but at roughly 1.07 MB, progressively loaded, arriving
      behind content you are already looking at, it is not a problem worth
      structure even on mobile (owner decision, 2026-08-09). The `card1x` half
      was worth doing because it was one constant and a `srcset`; the rest is
      machinery in exchange for bytes nobody waits on.

- [ ] UI-24 **Sign-in hangs when the hub cannot be reached.** `Auth` disables
      the form for the duration of the request and has no timeout, so a hub
      that accepts the connection and never answers leaves the screen with a
      dead form and no way to cancel — the one screen with nothing else to
      navigate to. Pre-existing, and not adjacent to anything the redesign
      touched, so it is recorded rather than folded into that branch.

- [x] UI-25 **The whole-libraries grant can lose an update.** Admin sends the
      complete set of libraries for a user rather than a delta, so two admins
      editing the same user's grants concurrently gave the second write the
      whole answer — the first one's change gone with nothing said. The request
      carries a version now: `users.grants_version`, returned with every read
      and required on the write, checked and bumped in the same statement so
      two writers cannot both pass. The loser gets 409 `stale_write` and is
      told to reload; an account deleted under them still gets 404, because
      "reload and try again" is advice that would never come true.

- [ ] UI-26 **Twenty-five suppressed `react-hooks/exhaustive-deps` warnings.**
      Deleting the inline disables and running the project's own config
      produces 23 warnings, which `--deny-warnings` would turn red; **9 of the
      suppressions were added on this branch** (`git diff master..HEAD`), and
      spread over four files. The FILE totals are different numbers, and an
      earlier draft printed them as though they were the same one: `Player.tsx`
      7 and `AlbumPlayer.tsx` 6 are totals, not additions. Either way the weight
      is in the seek, recovery, next-episode and queue machinery, whose
      stale-closure bugs this branch spent days fixing. Twenty-five suppressions against 23 warnings
      because two of them no longer cover anything: worth removing, but they
      are the cheap half of this item. Some are genuinely load-bearing (the
      session effect's cleanup destroys the hls instance, so re-running it on
      an unrelated render would tear playback down and rebuild it; it used to
      end the session there too, which was worse and is now App's job); the
      rest are unreviewed. Accepted
      as debt with the reason stated, because CI cannot catch what it has been
      told to ignore.
      Counts measured 2026-08-10 by deleting every disable comment and running
      `oxlint src`. The previous figures — 20/20/7 — were a month stale in
      three places at once, which is the argument for measuring them in the
      same commit as any claim about them.

- [ ] UI-27 **A multi-part film is indistinguishable from a set of alternative
      files.** One film split across seven numbered part files on a single host
      is a real case in this library. The hub gets it right — `item_sources`
      holds the parts, contiguous, and playback assembles them into a single
      timeline — but the item body publishes `sources` as a flat array of
      `{path_rel, size, available, revision}` with **no part number**, ordered
      by size descending. So the client is handed seven entries in an order
      that means nothing and cannot tell "one film, seven files" from "seven
      encodes, pick one", which is exactly what the detail page's "7 sources"
      reads as.
      Nothing was broken for a viewer who pressed play. What was missing is the
      client's ability to say what it is looking at, and no amount of UI work
      fixes that from here.
      **The API half is done**: each source row carries `source_id`, `part` and
      `parts`, so rows sharing an id are parts of one work in part order and
      rows with different ones are alternatives. The order was wrong as well as
      unlabelled — it ranked individual FILES by size while claiming to be "the
      order playback picks in", so a two-CD film listed cd2 first. It is
      playback's own ordering now.
      The detail page still reads "7 sources": `Source` in `web/src/api.ts`
      declares none of the three fields and nothing groups on them. Open until
      it does; the rebuild's detail page (phase 9) is where that lands.

- [ ] UI-17 **No accessibility pass.** Keyboard reachability was kept in mind
      where a pointer gesture was added (click-to-promote beside pill dragging,
      arrow keys on the fallback ladder, a disabled rather than absent lane
      arrow), but nothing here was verified with a screen reader or a
      keyboard-only run. UX-3 remains unaudited.
      One exception, which is not the pass: the search results panel is built to
      the combobox pattern deliberately — the box carries the role, the expanded
      state and `aria-activedescendant`, the rows are options in a listbox that
      contains nothing else, and the two message lines sit outside it because a
      listbox may not hold them. Every key on it was driven and measured. Still
      no screen reader has been near it, so the announcements are inference: in
      particular the panel's `role="status"` lines arrive together with their
      text, which is the case live regions are least reliable at, and the
      failure path therefore also raises an ordinary notice.

## Tried and rejected

- [x] UI-18 **Lazy-loading Admin and Settings.** Split out, measured, put
      back. They are 9 kB gzip of an 83 kB bundle and cost a chunk request the
      moment either is opened. The player split is the one that pays: it takes
      `hls.js` and `jassub` with it, 164 kB that browsing never touches.
