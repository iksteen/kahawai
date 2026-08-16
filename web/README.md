# kahawai's web UI

Vue 3 with `<script setup>`, TypeScript, Vite and Tailwind. Served by the hub
under `/app/`, embedded in its binary by `rust-embed` — so a `cargo build` of
`kahawai-hub` runs `npm run build` here first (see `crates/kahawai-hub/build.rs`)
and a Rust-only checkout builds without a UI rather than failing.

## The API client is generated

`src/api/generated/` is Orval's output from `openapi.json`, which is itself
generated from the hub. Neither is edited by hand, and only the second is
committed:

    npm run api:export     # re-read the hub, re-stamp, regenerate

`openapi.json` carries a fingerprint of the Rust files it was generated from,
and every install, build, test and typecheck checks it. A stale document fails
the gate rather than silently producing a client that disagrees with the hub.

## Layout

    src/api/         the transport, the session, and the generated client
    src/domain/      decisions with no framework in them — the tested half
    src/composables/ state and effects: queries, the queue, the player's health
    src/components/  presentational SFCs. Props in, events out
    src/views/       one per route

`src/domain/` is where anything worth arguing about goes: it can be checked
without mounting anything, and the mounted tests are then about the wiring
rather than about arithmetic.

## Working on it

    npm run dev        # Vite, against a hub on :8080
    npm test           # vitest
    npm run lint       # oxlint, warnings are errors
    npm run fmt        # oxfmt — run it before every commit, CI gates on it
    npm run typecheck  # vue-tsc

The hub can also serve a directory instead of its embedded copy, which is how
you point a running hub at a different bundle:

    kahawai-hub --web-dir web/dist
