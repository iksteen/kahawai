#!/usr/bin/env python3
"""Check local packaging against the newest published release (including RCs).

Usage: scripts/kahawai-packaging-check.py [--arch-dir ../arch] [--tag vX.Y.Z]
Requires makepkg and local kahawai / kahawai-gstreamer AUR checkouts.
Checks only: never updates recipes, installs packages or publishes anything.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import urllib.request

repo = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--arch-dir", type=Path, default=repo.parent / "arch")
parser.add_argument("--tag", help="Check a specific published release instead of the newest")
args = parser.parse_args()
endpoint = "https://api.github.com/repos/iksteen/kahawai/releases"
endpoint += f"/tags/{args.tag}" if args.tag else "?per_page=1"
request = urllib.request.Request(endpoint, headers={"User-Agent": "kahawai-packaging-check"})
with urllib.request.urlopen(request, timeout=30) as response:
    release = json.load(response)
if isinstance(release, list):
    release = release[0]
tag = release["tag_name"]
if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-rc\.\d+)?", tag):
    parser.error(f"unsupported release tag: {tag}")
version = tag[1:]
asset = next(a for a in release["assets"] if a["name"] == f"kahawai-{version}-source.tar.gz")
digest = asset["digest"].removeprefix("sha256:")
commit = re.search(r"Release source commit: `([0-9a-f]{40})`", release["body"]).group(1)


def check(ok, message):
    if not ok:
        raise SystemExit(f"FAIL: {message}")
    print(f"PASS: {message}")


recipes = {}
for package, pin in [("kahawai", "_tag"), ("kahawai-gstreamer", "_kahawai")]:
    directory = args.arch_dir / package
    recipe = (directory / "PKGBUILD").read_text()
    recipes[package] = recipe
    check(f"{pin}={version}\n" in recipe, f"{package} uses {tag}")
    check(digest in recipe, f"{package} pins the published source checksum")
    generated = subprocess.check_output(["makepkg", "--printsrcinfo"], cwd=directory, text=True)
    check(generated == (directory / ".SRCINFO").read_text(), f"{package} .SRCINFO is current")
check(f"_commit={commit}\n" in recipes["kahawai"], "kahawai stamps the published source commit")

docker = (repo / "Dockerfile").read_text()
gst = re.search(r"^ARG GSTREAMER_VERSION=(.+)$", docker, re.M).group(1)
check(f"ARG GST_PLUGINS_RS_VERSION=gstreamer-{gst}\n" in docker, "Docker GStreamer and Rust plugins agree")
check(f"_gst={gst}\n" in recipes["kahawai"], "AUR Kahawai requires the shipping GStreamer version")
check(f"pkgver={gst}\n" in recipes["kahawai-gstreamer"], "AUR GStreamer matches Docker")
brew = (repo / "HomebrewFormula/kahawai-gstreamer.rb").read_text()
check(f"/archive/{gst}/gstreamer-{gst}.tar.bz2" in brew, "Homebrew GStreamer matches Docker")
check(f"gst-plugins-rs-gstreamer-{gst}.tar.bz2" in brew, "Homebrew Rust plugins match Docker")
mac = (repo / "scripts/kahawai-mac.sh").read_text()
check(f"KEG=/opt/homebrew/Cellar/kahawai-gstreamer/{gst}\n" in mac, "Mac deployment selects the shipping GStreamer keg")
print(f"Packaging metadata matches {tag}. Run makepkg to verify sources, build and check each package.")
