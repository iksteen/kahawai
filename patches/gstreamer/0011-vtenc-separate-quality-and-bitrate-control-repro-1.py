#!/usr/bin/env python3
"""Encode generated media on Apple Silicon; check rates, mode changes and decode.

Requires GStreamer development files, a C compiler and ffmpeg/ffprobe. Uses the
installed plugin unless --plugin-dir selects an isolated patched build.
"""
import argparse
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile


def run(directory, plugin_dir):
    env = os.environ.copy()
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env["GST_REGISTRY"] = str(directory / "registry.bin")
    if plugin_dir:
        env["GST_PLUGIN_PATH"] = str(plugin_dir.resolve())
        env["VT_TEST_PLUGIN"] = str(plugin_dir.resolve() / "libgstapplemedia.dylib")
    flags = shlex.split(subprocess.check_output(
        ["pkg-config", "--cflags", "--libs", "gstreamer-app-1.0"], text=True,
        env=env))
    binary = directory / "vt-regression"
    subprocess.run(["cc", str(Path(__file__).with_suffix(".c")), *flags,
                    "-o", str(binary)], check=True, env=env)
    subprocess.run([str(binary), str(directory)], check=True, env=env, timeout=600)
    for extension, codec in (("h264", "h264"), ("hevc", "h265")):
        video = directory / f"vtenc_{codec}_hw.{extension}"
        decoded = subprocess.run(["ffmpeg", "-v", "error", "-xerror", "-i",
                                  str(video), "-f", "null", "-"], check=True,
                                 capture_output=True, text=True, timeout=180)
        # Some decoder corruption diagnostics do not change ffmpeg's exit code.
        if decoded.stderr.strip():
            raise RuntimeError(f"{codec}: decoder errors: {decoded.stderr}")
        frames = subprocess.check_output([
            "ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
            "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0",
            str(video)], text=True, timeout=180).strip()
        if frames != "2880":
            raise RuntimeError(f"{codec}: decoded {frames} frames, expected 2880")
        print(f"{codec}: all 2880 frames decode across mode changes", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plugin-dir", type=Path)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    if sys.platform != "darwin":
        print("VideoToolbox regression requires macOS and Apple hardware", file=sys.stderr)
        return 77
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
        run(args.output_dir.resolve(), args.plugin_dir)
    else:
        with tempfile.TemporaryDirectory(prefix="vt-rate-control-") as temp:
            run(Path(temp), args.plugin_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
