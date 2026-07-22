# Kahawai

**Kahawai** (*kah-hah-why*) is Hawaiian for *stream* — the channel a river carves through the land — which is exactly what this is: a self-hosted media streaming server for the series, movies, music, and anime you've backed up from your own media. It's also, happily, the Māori name of a strong, fast-schooling New Zealand fish ("strong water"), which we're keeping as the unofficial mascot.

## What it is

A Rust backend built on GStreamer, shipped two ways from one codebase:

- **All-in-one** — a single binary for a NAS or home server.
- **Modular** — a **hub** (the only thing clients talk to), one or more **mediahosts** (announce collections of media from their disks), and optional **transcoders** (handle playback for clients that can't play the source as-is). Satellites dial out to the hub and enroll via a console-code certificate flow — the hub is its own CA.

## What makes it different

- **Plays the cheapest sufficient path, always.** Direct play when possible; container remuxing happens *in the hub* with no transcoder needed; re-encoding is a last resort, per-stream, hardware-accelerated, and scheduled across however many transcoder machines you attach.
- **Anime as a first-class citizen.** AniDB exact-file matching via ED2K hashes, AniList relations and watch orders, fansub filename conventions, and ASS subtitles rendered faithfully — client-side with real fonts where the player can, burn-in or opt-in flattening where it can't.
- **Honest capability negotiation.** Clients report what they can actually decode; the server explains every playback decision ("why is this transcoding?") right in the UI.
- **Batteries included.** Embedded web app for admin and playback, metadata from TheTVDB/TMDB/MusicBrainz, user-initiated subtitle downloads via OpenSubtitles, multi-user watch state — all in the binary, no external services.

## Status

Pre-implementation. See the design documents:

- [Technical requirements](./docs/kahawai-technical-requirements.md)
- [Implementation design](./docs/kahawai-implementation.md)

## Name

*Kahawai* is Hawaiian for **stream**. The same word in te reo Māori names the kahawai fish (*Arripis trutta*), from *kaha* (strong) + *wai* (water). A streaming server could hardly ask for a better pair of meanings, and we use the word with respect for both origins.
