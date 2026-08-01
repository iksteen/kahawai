# GStreamer vs ffmpeg: an honest assessment

*2026-08-01. Written after a fresh-eyes audit of the media layer
(~11,600 lines, 87 commits) plus an adversarial review of the audit's
own claims. Assessment only — no migration is proposed, and the
comparison's limits are stated rather than papered over.*

## The question

Kahawai's media layer is built on GStreamer (gstreamer-rs). Was that
right, versus ffmpeg — either driving the CLI as a subprocess, or
linking libav via FFI?

## What GStreamer actually carries

Less than "the media layer" suggests. GStreamer is the **transport,
negotiation, and codec** layer:

- the seekable `appsrc` byte plane (lease-fed remote sources, prefetch
  ring, generation-stamped seeks) — the architectural keystone;
- parsebin/decodebin routing with per-stream copy-vs-encode decisions;
- encoder discovery across NVENC / VAAPI / QSV / VideoToolbox /
  software as preference lists + dry-run verification, with the
  element registry and muxer pad templates serving as **runtime
  oracles** for negotiation (`ts_muxable_names`, `fmp4_muxable_names`,
  `can_decode`) — every hand-maintained capability list we tried was
  wrong;
- pad-probe machinery: the pacing window, seek gates, `guard_pts`, the
  AAC layout pin, the tone-map scene-peak sampler, burn-in blending;
- live subtitle taps out of the *playback* pipeline (subtitles
  materialize during the session, no second read of the source);
- the GL tone-map segment.

The subtitle **domain** and fMP4 **packaging** are already bespoke
Rust that hangs off GStreamer pads: `imagesubs.rs` (hand-written
PGS/VobSub decode), `subindex.rs` (hand-written MKV/MP4 index walkers
for sparse extraction), `fmp4sink.rs` (hand-written HLS/fMP4 writer —
`hlscmafsink` didn't fit), `burnin.rs` (index-driven display sets).
Kahawai is increasingly its own media framework using GStreamer for
what frameworks are bad at replacing: scheduling, negotiation, and
codec/hardware abstraction.

## Positives

1. **The byte plane.** One pipeline demuxes a moov-at-end MP4 over a
   network lease. The ffmpeg CLI cannot express this (a pipe is
   forward-only); libav could, via custom AVIO over unsafe FFI.
2. **One registry, three hardware vendors.** Uniform encoder handling
   plus per-box calibration as configuration (`demote_decoders`
   turned the J5005 and macOS decoder quirks into config lines).
3. **Elements as oracles.** The registry and pad templates answer
   "what can this box do" at runtime; negotiation consumes the answer
   directly (HUB-14/15b).
4. **In-pipeline programmability.** Pacing, gates, taps, probes,
   burn-in — surgical control the ffmpeg CLI does not offer and libav
   offers only by writing your own filters.
5. **Licensing delegation (NFR-8).** Codecs live in distro plugin
   tiers; kahawai stays MIT and links none. An in-house libav build
   would carry x264/x265/fdk-aac consequences itself — this project
   already rejected GPL linkage once (subtile-ocr).

## Negatives, stated honestly

**Where the time went.** Roughly 25 of the media crate's 87 commits
are GStreamer-specific workarounds rather than domain logic. This
number is **not a comparison** — there is no ffmpeg-kahawai control
arm, and any stack accumulates its own workaround ledger. It is also
biased upward twice over: our commit hygiene lands workarounds as
separate, labeled commits (that is why they are countable), and our
corpus is adversarial by construction (fansub muxes, DTS-HD, PGS,
mislabeled tracks) — it stresses the paths any framework handles
worst. Read it as "this is what the road we took cost", nothing more.

**The failure classes** (these carry the weight, not the count):

- *Crashes.* Element bugs abort the process — two hlssink3
  unwrap-panics (upstream fix authored here, merged, unreleased), the
  splitmuxsink mid-GOP `g_assert` that forced the SeekGate design,
  headless vtdec segfaults. But note the correction from review:
  steady-state, this is a solved problem, because kahawai **converged
  on ffmpeg's own containment model** — pipelines run in worker
  processes; a crash kills a session, not a service. The real cost was
  a one-time *discovery tax* (the commits and one three-day
  orphaned-worker incident it took to learn that ffmpeg's shape
  imposes on day one). Residual in-process surfaces remain by choice:
  mediahost discovery, the transcoder's in-process fallback runner,
  startup dry-runs — see the dual model below.
- *Silent wrongness.* fdkaacenc negotiating itself to mono without an
  error; `vapostproc` no-opping a tone-map while relabeling caps;
  libdca discarding DTS-HD's lossless 7.1 with nothing in any log;
  caps at pad-added being re-typed once data flows. This class is
  **indifferent to the process model** — an ffmpeg child exits 0 with
  wrong output just as quietly — and it is where most real debugging
  time went. The countermeasures (buffer-counting round-trip probes,
  the doctor's dry-run rows, facts reporting) are the honest price of
  *any* stack; GStreamer determined their shape, not their necessity.
- *Hostile defaults and fragmentation.* Queue sizes that deadlock HLS
  sinks, a discoverer that returns `Ok` on timeout, and the plugin-set
  lottery that makes `doctor.rs` (475 lines, ~25 matrix rows)
  necessary. A pinned ffmpeg build with known configure flags mostly
  does not charge this tax.
- *The missing tone-mapper.* No libplacebo element exists: HUB-15a
  cost eight commits of hand-written shader work that ffmpeg ships as
  a filter flag. (The result is now preferred over the off-the-shelf
  one, and the scene-adaptive peak probe exists *because* of pad
  probes — cost and product again.)

## The dual model (the actual answer)

The review collapsed the whole comparison into one trade: **being in
the process is simultaneously the cost and the product.** Everything
distinctive — byte plane, taps, probes, gates — exists because kahawai
is inside the pipeline; the crash exposure exists for the same reason.
ffmpeg's process boundary buys safety and opacity with the same coin.

Kahawai's answer is to place the boundary where surface-area ×
blast-radius peaks, and only there:

- **Playback pipelines** (decoders + encoders + GL + muxers, minutes
  of wall time, a live viewer): worker process. Same blast radius as a
  crashed ffmpeg child.
- **Discovery** (bounded header parse, the tamest slice of the API):
  in-process, because a scan probes tens of thousands of files and
  GStreamer's expensive part — registry init — is paid once and
  amortized. Process-per-probe would insure the narrowest surface at
  the price of turning minutes into hours. ffmpeg-CLI *cannot make
  this choice*; its boundary is mandatory everywhere.
- **The corpus sweep** (deliberately feeds hostile files to the decode
  path hunting crashes): child isolation again.

Known accepted edge: a poison file that segfaults discovery downs the
mediahost, and the rescan walks back into it — a crash loop the worker
model does not cover because it is not there. It has not fired across
the real library; if it does, the sweep's isolation pattern or a
per-file blocklist is the ready remedy.

## What an ffmpeg kahawai would look like

**CLI:** HLS/fMP4 muxing, subtitle burn-in, tone-mapping and most of
the caps-lie corpus handled for free; battle-hardened demuxers; a
community-sized debugging surface. But: the byte plane needs a ranged
HTTP shim over the lease, the pacing window and live taps are gone,
negotiation degrades to parsing `-encoders` output, `imagesubs.rs`
survives anyway (per-event PGS→RGBA is not a CLI feature), error
handling becomes stderr scraping, and per-probe process cost forbids
the in-process discovery optimization outright.

**libav:** recovers the byte plane and taps, deletes the hand-written
image decoders, gains the filter graph. But: the entire
threading/scheduling model GStreamer provides gets rebuilt by hand
over an unsafe FFI surface that also crashes on hostile input (the
isolation stays), hardware acceleration becomes per-vendor
hwdevice/hwframes plumbing instead of one registry, and codec
licensing moves in-house.

## The comparison arm, actually run (2026-08-01)

The one cheap experiment the critique above allows was run:
`scripts/kahawai-ffmpeg-compare.sh` samples files deterministically and
executes both stacks' cheapest honest equivalent — our head sweep
(`kahawai-sweep --one`) against a bounded ffmpeg copy-remux to HLS —
and prints the four-way failure diff. 200 files across
movies/series/anime/animore (seed 42), plus the captest torture file:

- **183 both pass.**
- **15 ffmpeg-only failures — all `.avi` → TS**, all the same
  "first pts and dts value must be set" muxer refusal, and **all
  cleared by adding `-fflags +genpts`** (verified on a sample). Not a
  robustness gap: a live demonstration of the CLI's cost shape —
  the default invocation fails, each container quirk needs its flag
  discovered from stderr, and the fix is per-quirk option-string
  knowledge. Our pipeline absorbs the same quirk implicitly because
  the parser/timestamper chain regenerates timestamps by design.
- **1 ours-only failure** — an mp4 whose produced segment had one
  missing DTS, caught by the sweep's own segment validation. The
  asymmetry warning made concrete: ffmpeg "passed" the same file
  because its arm runs no equivalent check. Self-detected quality
  bug on our side; unmeasured dimension on theirs.
- **1 both-fail** — a file with PTS-less packets (genuinely broken).
  Failure *manner* differs: ffmpeg exits with a clean muxer error;
  ours dies on the known hlssink3 unwrap (contained to the child
  process, but the crash class, not an error).

Reading: on this corpus, raw demux/remux robustness is **near
parity** — neither folklore ("ffmpeg swallows anything") nor scar
tissue ("GStreamer chokes constantly") survives contact with the
sample. What the experiment actually measured is the *shape* of each
stack's failure handling: ffmpeg fails at spawn time with a flag as
the fix; GStreamer fails in-pipeline with a probe or fallback as the
fix; and our own validation layer is the only reason one real defect
was visible at all. The unmeasured dimensions (integration depth,
encode quality, silent wrongness) remain unmeasured; nothing here
licenses a stronger claim.

## Verdict

GStreamer was the right call for this architecture — not by a
landslide, and not for folklore reasons. It earns its keep on four
things the design leans on hard: the lease-backed seekable byte plane,
in-pipeline programmability, the registry as a runtime negotiation
oracle across three hardware vendors, and licensing delegation. The
fights were real and are documented above, but the two big cost
classes resolve on inspection: crash exposure was a one-time discovery
tax already paid (the worker model reproduces ffmpeg's containment
exactly where it matters, while keeping in-process speed where the
risk is low), and silent wrongness is stack-independent. An
ffmpeg-CLI kahawai would have shipped the easy 80% faster and hit a
wall precisely at the features that make kahawai more than "Jellyfin
in Rust"; a libav kahawai recovers those by rebuilding the hard 20%
of GStreamer by hand. The division of labor that actually emerged —
GStreamer for transport/negotiation/codecs, bespoke Rust for the
subtitle domain and packaging, process boundaries placed by measured
risk — is deliberate, working, and fenced by the doctor, the sweep,
and the facts channel. Re-platforming would trade known, guarded
failure modes for an unknown set and forfeit the parts ffmpeg cannot
express.
