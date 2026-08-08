#!/usr/bin/env bash
# Create or verify the reproducible, version-stamped branch for a release tag.
#
#   kahawai-release-prepare.sh v1.2.3
#   kahawai-release-prepare.sh v1.2.3-rc.1 --dry-run
#
# The tag is the maintainer's release request and carries the release notes.
# The same-named branch contains one additional commit changing only the Cargo
# workspace version and lockfile; release artifacts are built from that commit.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,8s/^# \{0,1\}//p' "$0"; }

tag="${1:-}"
[ -n "$tag" ] || { usage >&2; exit 2; }
shift
dry_run=""
if [ "${1:-}" = "--dry-run" ]; then
    dry_run=1
    shift
fi
[ "$#" -eq 0 ] || die "unknown argument: $1"

case "$tag" in
    v0.0.0-dev) die "the development placeholder is not releasable" ;;
esac
if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.(0|[1-9][0-9]*))?$ ]]; then
    die "tag must be vX.Y.Z or vX.Y.Z-rc.N without leading zeroes"
fi
version="${tag#v}"
tag_ref="refs/tags/$tag"
branch_ref="refs/heads/$tag"
remote_ref="refs/remotes/origin/$tag"

[ "$(git cat-file -t "$tag_ref" 2>/dev/null || true)" = tag ] \
    || die "$tag must be an annotated tag"
tag_commit="$(git rev-parse "$tag_ref^{commit}")"

verify_versions() {
    local metadata bad
    metadata="$(cargo metadata --locked --no-deps --format-version 1)"
    bad="$(python3 -c '
import json, sys
want = sys.argv[1]
data = json.load(sys.stdin)
bad = [p["name"] + "=" + p["version"] for p in data["packages"] if p["version"] != want]
print(" ".join(bad))
' "$version" <<<"$metadata")"
    [ -z "$bad" ] || die "workspace version mismatch: $bad"
}

verify_branch() {
    local ref="$1" head parent count changed
    head="$(git rev-parse "$ref^{commit}")"
    parent="$(git rev-parse "$head^")"
    [ "$parent" = "$tag_commit" ] || die "$ref is not one commit above $tag_ref"
    count="$(git rev-list --count "$tag_commit..$head")"
    [ "$count" = 1 ] || die "$ref contains $count commits beyond the tag"
    changed="$(git diff --name-only "$tag_commit" "$head" | sort)"
    [ "$changed" = $'Cargo.lock\nCargo.toml' ] \
        || die "$ref changes files other than Cargo.toml and Cargo.lock: $changed"
    git switch --detach "$head" >/dev/null
    verify_versions
    release_commit="$head"
}

if git show-ref --verify --quiet "$remote_ref"; then
    verify_branch "$remote_ref"
elif git show-ref --verify --quiet "$branch_ref"; then
    verify_branch "$branch_ref"
else
    git switch --detach "$tag_commit" >/dev/null
    current="$(cargo metadata --locked --no-deps --format-version 1 \
        | python3 -c '
import json, sys
versions = {p["version"] for p in json.load(sys.stdin)["packages"]}
print(next(iter(versions)) if len(versions) == 1 else ",".join(sorted(versions)))
')"
    [ "$current" = "0.0.0-dev" ] \
        || die "tag source must use the committed 0.0.0-dev placeholder, found $current"
    git switch -c "$tag" >/dev/null
    cargo set-version --workspace "$version"
    verify_versions
    changed="$(git diff --name-only | sort)"
    [ "$changed" = $'Cargo.lock\nCargo.toml' ] \
        || die "version stamping changed unexpected files: $changed"
    git add Cargo.toml Cargo.lock
    git -c user.name='github-actions[bot]' \
        -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
        commit -m "release: $tag" >/dev/null
    release_commit="$(git rev-parse HEAD)"
    if [ -z "$dry_run" ]; then
        git push origin "HEAD:$branch_ref"
    fi
fi

prerelease=false
case "$version" in *-rc.*) prerelease=true ;; esac
printf 'tag=%s\nversion=%s\nprerelease=%s\ntag_commit=%s\nrelease_commit=%s\n' \
    "$tag" "$version" "$prerelease" "$tag_commit" "$release_commit"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        printf 'tag=%s\n' "$tag"
        printf 'version=%s\n' "$version"
        printf 'prerelease=%s\n' "$prerelease"
        printf 'tag_commit=%s\n' "$tag_commit"
        printf 'release_commit=%s\n' "$release_commit"
    } >> "$GITHUB_OUTPUT"
fi
