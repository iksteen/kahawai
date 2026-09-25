# Wired TCP control

Follow-up: the [wired Mac playback comparison](2026-09-25-mac-push-pull.md)
tests the previously problematic sources on both push and pull revisions.

The [transport-isolation reproducer](2026-09-24-m08-transport-isolation.md)
completed without a delivery collapse from the NAS to the Mac mini on
2026-09-25. This is a synthetic transport test, not a playback benchmark.

The maintainer confirmed that these machines share an Ethernet switch path.
The earlier NAS-to-desktop test crossed different Deco M9 Plus nodes over
wireless backhaul. Routes selected the NAS's enp3s0 and the Mac's en0; both
interfaces reported 1 Gb/s, with the Mac reporting full duplex. The sender
remained Linux; the receiver changed from Linux to macOS 26.6.2.

## Method

The same NAS Python sender connected to two sockets on the Mac. One socket
transferred 8 MiB and remained idle while the other transferred for 120 seconds.
The idle socket then resumed. Unlike the earlier test, which stopped after
512 MiB on resumption, this test observed another full 120 seconds and recorded
the time to the first 512 MiB separately. The requested byte ceiling was raised
to 64 GiB to avoid finishing before the observation interval on the faster path;
the sender reused a 256 KiB payload.

Both ends enabled TCP_NODELAY. There were no media files, disk reads, TLS,
HTTP/2, Kahawai components or GStreamer pipelines in the traffic path. SSH
orchestrated the test and collected diagnostics; payload travelled directly
between the NAS and Mac. Variants ran sequentially, once each.

## Results

| Variant | Active, 120 s average | Resumed, 120 s average | Resumed first 512 MiB | Lowest resumed 2 s sample |
|---|---:|---:|---:|---:|
| Application acknowledgement per 256 KiB | 91.57 MiB/s | 91.36 MiB/s | 5.60 s | 90.40 MiB/s |
| Continuous writes | 112.16 MiB/s | 112.18 MiB/s | 4.57 s | 112.03 MiB/s |

The earlier desktop continuous test took **279.87 seconds** for the resumed
512 MiB (1.83 MiB/s), versus **4.57 seconds** here (112.11 MiB/s). The earlier
acknowledged transfer collapsed late in its active interval; neither wired
variant collapsed in either interval. Sender TCP RTT samples ranged from
1.10–1.68 ms with acknowledgements and 2.07–3.35 ms with continuous writes;
these snapshots include the idle socket as well as the active socket.

## Interpretation and limits

The collapse is not reproduced on the wired NAS-to-Mac path. Together with its
plain-TCP reproduction on the desktop path, this strongly directs investigation
toward the mesh/desktop path rather than source buffering or GStreamer.
It does not isolate the mesh hardware: receiver OS, NIC and network path all
changed. One successful run per variant also does not establish a failure rate.

An Ethernet-only route is a demonstrated way to avoid the collapse in this
synthetic test. Testing the same desktop over an Ethernet-only path would
separate the path change from the receiver change. Actual playback over the
wired path still needs measurement before claiming the playback issue resolved.
These results do not justify enlarging application buffers or changing TCP
settings, and do not establish push-versus-pull performance.

Both sender and receiver exited successfully. The test listener and NAS sender
were absent afterwards. Both hubs reported the NAS healthy and no active
sessions; services, binaries and transport settings were unchanged.

Private evidence, scripts, per-two-second receiver rates, sender TCP snapshots
and machine metadata are retained in
`target/benchmarks/2026-09-25-wired-tcp-control/`. `analyse.py` produces the
summary from the captured results. The scripts contain no media identities or
authentication tokens.
