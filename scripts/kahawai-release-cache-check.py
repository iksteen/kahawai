#!/usr/bin/env python3
"""Prove the Docker dependency recipe survives source edits and RC stamping.

Usage: scripts/kahawai-release-cache-check.py /path/to/cargo-chef
Requires cargo-chef 0.1.74. Only temporary copies are modified.
"""
import pathlib
import shutil
import subprocess
import sys
import tempfile

repo = pathlib.Path(__file__).resolve().parents[1]
chef = str(pathlib.Path(sys.argv[1]).resolve()) if len(sys.argv) > 1 else "cargo-chef"
with tempfile.TemporaryDirectory(prefix="kahawai-recipe-") as directory:
    root = pathlib.Path(directory)
    files = subprocess.check_output(["git", "ls-files", "-z"], cwd=repo).decode().split("\0")
    for name in filter(None, files):
        if not (name.startswith("crates/") or name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")):
            continue
        dest = root / name
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(repo / name, dest)

    def recipe():
        subprocess.run([chef, "chef", "prepare", "--recipe-path", "recipe.json"], cwd=root, check=True)
        return (root / "recipe.json").read_bytes()

    original = recipe()
    source = root / "crates/kahawai-core/src/lib.rs"
    source.write_text(source.read_text() + "\n// Source edit must reuse dependencies.\n")
    assert recipe() == original, "source edit invalidated dependency recipe"
    for name in ("Cargo.toml", "Cargo.lock"):
        path = root / name
        path.write_text(path.read_text().replace("0.0.0-dev", "0.0.14-rc.99"))
    assert recipe() == original, "release stamping invalidated dependency recipe"
    manifest = root / "Cargo.toml"
    manifest.write_text(manifest.read_text().replace('anyhow = "1"', 'anyhow = { version = "1", default-features = false }'))
    assert recipe() != original, "dependency change did not invalidate recipe"
print("PASS: source/RC changes reuse the recipe; dependency changes invalidate it")
