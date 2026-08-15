# Releasing Kahawai

The supported release artifact is the multi-architecture container image.
Linux binaries are attached for debugging and expert use, but they do not
bundle GStreamer or its plugins and are not a supported native installation.

## Create a release

1. Make an annotated tag named `vX.Y.Z` or `vX.Y.Z-rc.N` on a commit whose
   workspace Cargo version is the permanent `0.0.0-dev` placeholder. Put the
   release title in the annotation subject and the notes in its body.
2. Push the tag. The release workflow creates a same-named branch containing
   one additional `release: <tag>` commit. That commit stamps every workspace
   package in `Cargo.toml` and `Cargo.lock`, then updates `info.version` and
   `x-kahawai-source-sha256` in `web/openapi.json` to make the checked-in API
   document match those stamped sources.
3. CI builds from the fully qualified branch ref, never the original tag. It
   runs source checks, builds and tests the pinned media stack natively on
   Linux amd64 and arm64, and smoke-tests each exact pushed image digest.
4. Only after both architectures pass does CI create the named Docker Hub
   manifest and publish the draft GitHub Release.

The tag and branch deliberately have the same short name. Use explicit refs
when inspecting them:

```sh
git show refs/tags/v1.2.3
git show refs/heads/v1.2.3
```

Stable releases publish Docker tags `X.Y.Z`, `X.Y`, and `latest`. Release
candidates publish only their exact `X.Y.Z-rc.N` tag.

## Failure and retry

The workflow never force-updates the stamped branch. A retry reuses it only
when it is exactly one commit above the annotated tag, contains the expected
workspace and OpenAPI versions and source fingerprint, and changes only the
two Cargo files plus those two OpenAPI values. Any other OpenAPI change or
branch conflict stops the run for manual inspection.

A failed release may leave a stamped branch, a draft GitHub Release, workflow
artifacts, and untagged Docker Hub digests. Rerunning the workflow reuses valid
branch and draft state. Named Docker tags and the public GitHub Release are not
updated until every gate succeeds.

The workflow currently depends on GitHub's public-preview `ubuntu-26.04` and
`ubuntu-26.04-arm` hosted runners. There is intentionally no fallback to
Ubuntu 24.04: its GStreamer stack is too old to provide meaningful media-path
evidence for this project.

## Required repository secrets

- `DOCKERHUB_USERNAME`: Docker Hub namespace; images publish as
  `<username>/kahawai`.
- `DOCKERHUB_TOKEN`: token allowed to push that repository.

The workflow's GitHub token needs permission to create the stamped branch and
draft/publish releases. The workflow records the stamped commit in release
notes, OCI labels, the attached source archive, and checksums.
