#!/usr/bin/env python3
"""Measure Kahawai's intro/credits detection against intro-skipper's own code.

Three levels, because one end-to-end number cannot say which half is wrong
(docs/intro-detection-plan.md):

  l1 FILE            fingerprints: ours against theirs, point by point
  l2 FILE FILE       the search: identical points into both implementations
  l3 DIR|FILE...     end to end: both run from the media file

l3 scores against ground truth when the directory holds a labels.json
({"episode.mkv": {"intro": [start, end], "credits": [start, end]}}), and
otherwise reports agreement between the two implementations.
"""

import argparse
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
REF = ROOT / "scripts" / "kahawai-intro-ref.sh"
OURS = ROOT / "scripts" / "kahawai-intro.sh"


def target_dir() -> pathlib.Path:
    meta = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return pathlib.Path(json.loads(meta.stdout)["target_directory"])


def run(cmd, **kwargs) -> str:
    result = subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    if result.returncode != 0:
        sys.exit(f"{cmd[0]} failed:\n{result.stderr.strip()}")
    return result.stdout


def points(text: str) -> list[int]:
    return [int(line) for line in text.splitlines() if line.strip().isdigit()]


def bit_report(ours: list[int], theirs: list[int]) -> dict:
    n = min(len(ours), len(theirs))
    diffs = [bin(ours[i] ^ theirs[i]).count("1") for i in range(n)]
    return {
        "points_ours": len(ours),
        "points_theirs": len(theirs),
        "identical": sum(1 for d in diffs if d == 0),
        "mean_bits": round(sum(diffs) / n, 3) if n else None,
        "max_bits": max(diffs) if diffs else None,
        # Their search calls two points equal at six differing bits or fewer,
        # so this is the number that decides whether the fingerprints are
        # interchangeable in practice.
        "within_tolerance": round(sum(1 for d in diffs if d <= 6) / n, 4) if n else None,
    }


def level1(args) -> dict:
    window = f"0:{args.window}"
    ours = points(run([str(OURS), "--fingerprint", "--window", window, args.files[0]]))
    theirs = points(run([str(REF), "fingerprint", args.files[0], "0", str(args.window)]))
    report = bit_report(ours, theirs)
    if report["within_tolerance"] is None:
        sys.exit(f"no fingerprint points to compare (ours {len(ours)}, theirs {len(theirs)})")
    print(f"fingerprint parity: {report['identical']}/{min(len(ours), len(theirs))} points identical, "
          f"mean {report['mean_bits']} bits, max {report['max_bits']}, "
          f"{report['within_tolerance'] * 100:.1f}% within their 6-bit tolerance")
    return report


def level2(args) -> dict:
    """Both searches over the same points — theirs, so the input is not in doubt."""
    examples = target_dir() / "release" / "examples" / "compare_points"
    with tempfile.TemporaryDirectory() as tmp:
        prints = []
        for i, path in enumerate(args.files[:2]):
            out = pathlib.Path(tmp) / f"fp{i}.txt"
            out.write_text(run([str(REF), "fingerprint", path, "0", str(args.window)]))
            prints.append(str(out))
        ours = json.loads(run([str(examples), *prints]))
        theirs = json.loads(run([str(REF), "compare", *prints]))

    same = ours == theirs
    print(f"ours   {json.dumps(ours)}")
    print(f"theirs {json.dumps(theirs)}")
    print("identical" if same else "DIFFERENT")
    return {"ours": ours, "theirs": theirs, "identical": same}


def segments_ours(paths: list[str], anime: bool, refine: bool = True) -> dict:
    cmd = [str(OURS), "--json"] + (["--anime"] if anime else []) \
        + ([] if refine else ["--no-refine"]) + paths
    started = time.monotonic()
    out = run(cmd)
    report = json.loads(out[out.index("{"):])
    report["wall_seconds"] = time.monotonic() - started
    return report


def segments_theirs(paths: list[str], anime: bool, refine: bool = True) -> dict:
    cmd = [str(REF), "segments"] + (["--anime"] if anime else []) \
        + ([] if refine else ["--no-refine"]) + paths
    started = time.monotonic()
    report = json.loads(run(cmd))
    report["wall_seconds"] = time.monotonic() - started
    return report


def span(seg) -> str:
    return f"{seg['start']:7.1f}-{seg['end']:7.1f}" if seg else "      -        "


def iou(a, b) -> float | None:
    if not a or not b:
        return None
    overlap = max(0.0, min(a["end"], b["end"]) - max(a["start"], b["start"]))
    union = max(a["end"], b["end"]) - min(a["start"], b["start"])
    return overlap / union if union > 0 else 0.0


def level3(args) -> dict:
    files = args.files
    if len(files) == 1 and pathlib.Path(files[0]).is_dir():
        directory = pathlib.Path(files[0])
        files = [str(p) for p in sorted(directory.iterdir())
                 if p.suffix.lower() in {".mkv", ".mp4", ".m4v", ".avi", ".ts", ".webm"}]
    else:
        directory = pathlib.Path(files[0]).parent
    if not files:
        sys.exit("no media files")

    labels = {}
    label_file = directory / "labels.json"
    if label_file.exists():
        labels = json.loads(label_file.read_text())

    ours = segments_ours(files, args.anime, not args.no_refine)
    theirs = segments_theirs(files, args.anime, not args.no_refine)

    # A comparison of half-read bytes measures the outage, not the detector.
    unreadable = [e["name"] for e in ours["episodes"] if e.get("unreadable")]
    if unreadable:
        sys.exit(f"unreadable on our side, nothing to compare: {', '.join(unreadable)}")
    # By NAME, not by position: one side skipping an episode the other kept
    # desynced every row after it, and each pair scored a different episode
    # against its neighbour.
    their_rows = {e["name"]: e for e in theirs["episodes"]}
    missing = [e["name"] for e in ours["episodes"] if e["name"] not in their_rows]
    if missing:
        sys.exit(f"intro-skipper produced no row for: {', '.join(missing)}")

    rows = []
    print(f"{'episode':<28} {'kind':<8} {'kahawai':^17} {'intro-skipper':^17} {'Δstart':>8} {'Δend':>8} {'IoU':>6}"
          + ("  truth" if labels else ""))
    for mine in ours["episodes"]:
        yours = their_rows[mine["name"]]
        truth = labels.get(mine["name"], {})
        for kind in ("recap", "intro", "credits"):
            a, b = mine.get(kind), yours.get(kind)
            row = {
                "episode": mine["name"],
                "kind": kind,
                "ours": a,
                "theirs": b,
                "delta_start": round(a["start"] - b["start"], 2) if a and b else None,
                "delta_end": round(a["end"] - b["end"], 2) if a and b else None,
                "iou": round(iou(a, b), 3) if iou(a, b) is not None else None,
            }
            if kind in truth:
                want = {"start": truth[kind][0], "end": truth[kind][1]}
                row["truth"] = want
                row["iou_ours_truth"] = round(iou(a, want), 3) if a else 0.0
                row["iou_theirs_truth"] = round(iou(b, want), 3) if b else 0.0
            rows.append(row)
            extra = ""
            if "truth" in row:
                extra = f"  ours {row['iou_ours_truth']:.2f} / theirs {row['iou_theirs_truth']:.2f}"
            print(f"{mine['name'][:28]:<28} {kind:<8} {span(a)} {span(b)} "
                  f"{row['delta_start'] if row['delta_start'] is not None else '-':>8} "
                  f"{row['delta_end'] if row['delta_end'] is not None else '-':>8} "
                  f"{row['iou'] if row['iou'] is not None else '-':>6}{extra}")

    both = [r for r in rows if r["ours"] and r["theirs"]]
    summary = {
        "episodes": len(ours["episodes"]),
        "found_by_both": len(both),
        "only_ours": sum(1 for r in rows if r["ours"] and not r["theirs"]),
        "only_theirs": sum(1 for r in rows if r["theirs"] and not r["ours"]),
        "neither": sum(1 for r in rows if not r["ours"] and not r["theirs"]),
        "median_iou": round(sorted(r["iou"] for r in both)[len(both) // 2], 3) if both else None,
        "within_1s": sum(1 for r in both if abs(r["delta_start"]) <= 1 and abs(r["delta_end"]) <= 1),
        "seconds_ours": round(ours["seconds"], 1),
        "seconds_theirs": round(theirs["seconds"], 1),
    }
    if labels:
        scored = [r for r in rows if "truth" in r]
        summary["truth_segments"] = len(scored)
        for who in ("ours", "theirs"):
            values = [r[f"iou_{who}_truth"] for r in scored]
            summary[f"median_iou_{who}_truth"] = round(sorted(values)[len(values) // 2], 3) if values else None
            summary[f"hits_{who}"] = sum(1 for v in values if v >= 0.5)

    print("\n" + json.dumps(summary, indent=2))
    return {"summary": summary, "rows": rows}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("level", choices=["l1", "l2", "l3"])
    parser.add_argument("files", nargs="+")
    parser.add_argument("--window", type=float, default=60.0, help="fingerprint window for l1/l2, seconds")
    parser.add_argument("--anime", action="store_true", help="credits by fingerprint before black frames")
    parser.add_argument("--no-refine", action="store_true",
                        help="l3: compare raw matches, with silence and keyframe snapping off on both sides")
    parser.add_argument("--json", type=pathlib.Path, help="also write the full result here")
    args = parser.parse_args()

    if not shutil.which("cargo"):
        sys.exit("cargo is not on PATH")
    # Release, always: half the measurement is how long each side takes, and a
    # debug build would make that number a fiction.
    subprocess.run(
        ["cargo", "build", "--release", "-p", "kahawai", "--bin", "kahawai"],
        cwd=ROOT, check=True,
    )
    subprocess.run(
        ["cargo", "build", "--release", "-p", "kahawai-intro", "--examples"],
        cwd=ROOT, check=True,
    )
    result = {"l1": level1, "l2": level2, "l3": level3}[args.level](args)

    if args.json:
        args.json.write_text(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
