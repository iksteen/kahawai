# VideoToolbox quality and bitrate control

GStreamer 1.28.7 applies `kVTCompressionPropertyKey_Quality` unconditionally,
including after configuring AverageBitRate or ConstantBitRate. On the tested
Apple hardware this makes changing AverageBitRate ineffective. ConstantBitRate
can hide the failure by padding output with filler NAL units.

Apple describes Quality as making the quantization parameter “adhere to a
fixed value” in its [ConstantQualityFactor documentation](https://developer.apple.com/documentation/videotoolbox/kvtcompressionpropertykey_constantqualityfactor).
The installed VideoToolbox SDK describes ConstantBitRate as padding frames,
and recommends against using it for general streaming/export. More output
bytes therefore do not by themselves establish better picture quality.

The patch separates the controls without removing the quality property:

| Configuration | VideoToolbox control |
| --- | --- |
| `bitrate=0`, no active `data-rate-limits` | Quality, including the existing default 0.5 |
| Nonzero bitrate, ABR | AverageBitRate; optional DataRateLimits |
| Nonzero bitrate, CBR | ConstantBitRate; existing fallback on older hardware |
| `bitrate=0`, active rate limit, ABR | Automatic bitrate with DataRateLimits |

Quality is retained as a property, but is not applied in a bitrate-controlled
session. It becomes active when returning to quality mode. ProRes retains its
existing exclusion from the Quality property.

Mode switches and removal of a rate limit recreate the compression session:
omitting a previously set property does not clear it. Quality changes also
recreate the session. Testing showed VideoToolbox accepting a live Quality
update while continuing to encode at the previous quality. Changes to a
nonzero bitrate within the same mode retain lightweight reconfiguration.

A planned recreation uses a separate flag from error recovery, whose callbacks
intentionally discard frames. It drains VideoToolbox **and the output queue**,
then publishes codec headers from the new session. Reusing recovery directly
lost pending frames; failing to update the headers corrupted HEVC decoding.
Recreation can introduce an extra keyframe and a short encoding pause.

The patch also fixes the CoreFoundation number type for the rate-limit window:
the local variable is a double, but the old call read it as a float. That could
send an invalid window instead of the requested hard limit.

## Reproduction and validation

The companion C/Python regression generates 1920×1080 NV12 SMPTE video at
24 fps. It runs twelve ten-second phases for each of hardware H.264 and HEVC:
bitrate changes, quality changes, quality/ABR/CBR transitions, and addition and
removal of rate limits. It measures encoded bytes separately from filler,
checks every phase's frame count, and decodes the complete output. Decoder
error diagnostics fail the check even if FFmpeg returns exit status zero.
No external media or service is required.

On macOS with Apple Silicon, GStreamer development files and FFmpeg:

```sh
export PKG_CONFIG_PATH=/opt/homebrew/Cellar/kahawai-gstreamer/1.28.7/lib/pkgconfig
python3 patches/gstreamer/0011-vtenc-separate-quality-and-bitrate-control-repro-1.py
```

Use `--plugin-dir /path/to/isolated/plugins` to test a separately built
`libgstapplemedia.dylib`; the check asserts that exact plugin was loaded.
Use `--output-dir /tmp/vt-results` to retain encoded artifacts. The wrapper
uses a private registry and disables DYLD interposition. Linux verification
reports this Apple-only patch as N/A, not LIVE.

Measured on the Mac mini with macOS 26.6.2, using the exact 1.28.7 applemedia
sources and linking to the patched Kahawai keg. Values below are kbit/s,
excluding the first second of each phase for settling:

| Phase | H.264 | HEVC |
| --- | ---: | ---: |
| ABR target 1500 | 1489 | 1466 |
| ABR target 6000 | 5916 | 6004 |
| Same target, quality changed to 0.75 | 5931 | 5996 |
| Quality 0.25, bitrate disabled | 4431 | 4471 |
| Quality changed to 0.75 | 18293 | 18514 |
| Quality → ABR 1500 | 1487 | 1473 |
| ABR → CBR 1500 | 1513 | 1512 |
| CBR → ABR 1500 | 1487 | 1463 |
| Rate limit 1500, automatic bitrate | 1001 | 991 |
| Rate limit removed, quality 0.75 | 18292 | 18512 |
| ABR 6000 with limit 1500 | 1050 | 1042 |
| Rate limit removed, ABR 6000 | 5880 | 5959 |

The same regression against the installed, unmodified plugin failed: its
first 1500 kbit/s phase produced 4432 kbit/s, and changing the target to
6000 kbit/s left output at 4432 kbit/s (quality 0.25 in both phases).

All 2,880 frames per codec were preserved and decoded without errors.
These numbers establish control behavior on this hardware, not a universal
quality score or a promise that ABR always reaches its target on easy content.
The regression uses broad bitrate bounds for that reason.

The installed plugin remains untouched during validation. The patch alone
does not change Kahawai's current software-encoder preference or deploy a new
GStreamer keg.
