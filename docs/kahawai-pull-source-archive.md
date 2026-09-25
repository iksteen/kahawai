# Pull-source experiment archive

The complete pull-appsrc experiment is preserved on
`archive/pull-appsrc-2026-09-25`, at commit
`3f212ec4a9fb83361601356e57607f390d2e5b83`.

Master was returned to `origin/master` at `48fa253` on 2026-09-25. The wired
Mac/NAS comparison showed broadly equivalent playback on the tested sources,
with additional production headroom on pull, especially one MP4 sample, but
mixed startup and seek latency. The additional implementation and maintenance
cost was not justified by the observed playback benefit. The earlier wireless
transport stalls confounded the desktop performance comparison.

The archive retains the protocol 5/6 changes, source construction and ownership
refactor, range buffers, two-window streaming, flow-control experiments and
benchmark reports. For the final comparison:

```sh
git show archive/pull-appsrc-2026-09-25:docs/benchmarks/2026-09-25-mac-push-pull.md
```

Independent ports retain deployment staging and verification, the offline
interleaving scanner, session-end cancellation of detached seeks, and cleanup
of cancelled lease/startup registrations. The push source and protocol 4.5
remain the baseline. This is a source-history change, not a fleet deployment.
