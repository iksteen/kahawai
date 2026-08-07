# matroskademux: a hole in an element's body kills the file; a hole in front of one does not

**Upstream:** not submitted yet. Branch
`matroskademux-resync-damaged-body` on the fork, based on `main`; the
patch here is the same commit against 1.28.5, which is what the
measurements below ran on. **Observed on:** GStreamer 1.28.5.
**Reproducer:** `…-repro-1.py` (videotestsrc + audiotestsrc, no media
needed).

    Could not demultiplex stream. (matroska-demux.c(6288):
    gst_matroska_demux_parse_id (): Failed to parse Element 0xe7)

A 2.8 GB series episode is missing the last 127 KiB of every 1 MiB
block — a download that wrote 897 KiB per chunk and left the rest zero.
`matroskademux` dies at the first hole. `ffmpeg` plays the same file
with artefacts at each gap, and so does `matroskademux` **in pull
mode**, which walks it end to end without a single error.

## Why streaming gives up where pulling does not

Push mode already recovers from corruption. `gst_matroska_demux_chain()`
catches a failure from `peek_id_length_push` — the element's ID or
length will not read — records `start_resync_offset`, switches to
`GST_MATROSKA_READ_STATE_SCANNING`, and scans on for the next Cluster,
giving up only after `INVALID_DATA_THRESHOLD` (2 MB) of looking.

That is the whole recovery, and it is keyed on the *header* failing.
Damage does not always fail there. A corrupt region can present a
header that parses perfectly — a plausible id, a plausible length — and
fail one level in, on the contents. Here a Cluster Timestamp (`0xe7`)
announces a 125-byte body; EBML permits at most 8 for an unsigned
integer, so `gst_ebml_read_uint` refuses it. `parse_id` then reaches
`parse_failed:`, posts `GST_ELEMENT_ERROR`, and the file is over —
without the resync it was already carrying ever being consulted.

Pull mode is unaffected because it reaches
`gst_matroska_demux_search_cluster()`, which is built on
`gst_pad_pull_range` and so cannot serve the streaming path. The policy
is therefore not in question, only its availability: **the same file,
the same demuxer, recovers or dies depending on how the bytes arrive.**

## The fix

Route the streaming case at `parse_failed:` into the resync the header
path already uses: record the resync origin if this is the first
failure, switch to `SCANNING`, return `GST_FLOW_OK`, and let the
existing machinery find the next Cluster.

Three properties are deliberate:

- **The bound is the existing one.** `INVALID_DATA_THRESHOLD` already
  governs how far the header path will scan; reusing it means a file
  that is damage all the way down still fails, after the same amount of
  looking, rather than acquiring a second limit with its own behaviour.
- **Pull mode is untouched**, gated on `demux->streaming`. The mode that
  already worked keeps working the way it did.
- **Genuine corruption still fails.** The guard in
  `gst_matroska_demux_check_read_size` is a different path and is not
  reached from here: a library file claiming a 128,041,197-byte element
  inside 88,797,249 bytes is still refused, which is what that guard is
  for.

## Numbers

The affected episode, fed through `appsrc` (push, seekable):

    before   FAIL at the first hole, 0 output
    after    demuxes to the end, 21 resyncs over the head of the file

Cost is not the concern the resync count suggests: sweeping the damaged
file takes **4.0 s against 5.4 s for a healthy file of the same kind**.
The byte-by-byte scan through each hole belongs to the pre-existing
header path, and only becomes visible as log volume when `GST_DEBUG` is
raised.

Regression: the 80 files a full library sweep had ever called FAIL were
re-swept with this patch. 55 → 56 pass; the two that still fail are the
oversized-element file above and an unrelated qtdemux case. Nothing that
previously passed changed.

## Running the reproducer

    python3 0009-…-repro-1.py [outdir]

It muxes its own file, overwrites one Cluster Timestamp's length with a
value no unsigned integer may have — leaving the id and length field
readable, which is the case the header-level resync cannot see — and
pushes the result through `appsrc`. Exits 0 when the bug reproduces, 1
when the plugin is fixed.

It fails loudly if no pads appear. An earlier draft linked the demuxer's
sometimes-pads by name in a `parse_launch` string, which links nothing:
the file was never demuxed and both the patched and unpatched plugins
"passed".
