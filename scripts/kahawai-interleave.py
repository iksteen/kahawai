#!/usr/bin/env python3
"""Rank local media by sustained audio/video byte separation; not a stall verdict.

Runs ffprobe without decoding. Reads whole files, one at a time; keeps per-second
track summaries in memory, not packet payloads. Media is opened read-only.
The actual GStreamer read order, network latency and client buffer are not modeled.
"""
import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

MIB = 1024 * 1024
# Match read_ahead.rs. Thresholds rank layouts, not predicted playback outcomes.
BUDGETS = {"transcoder": 2 * MIB, "hub": 16 * MIB}
EXTENSIONS = {".mp4", ".m4v", ".mov", ".mkv", ".webm", ".avi", ".ts", ".m2ts"}


def summarize(buckets, video, audio, min_run):
    """Compare disjoint byte envelopes for packets in the same media second.

    Using distance between envelopes (rather than total span) avoids flagging
    ordinary high-bitrate interleaving. Require consecutive affected seconds so
    isolated probes or one unusually positioned packet do not dominate ranking.
    """
    pairs = []
    for track in audio:
        rows = []
        for second, streams in sorted(buckets.items()):
            if video not in streams or track not in streams:
                continue
            v, a = streams[video], streams[track]
            gap = max(0, a[0] - v[1], v[0] - a[1])
            rows.append((second, gap))
        if not rows:
            continue
        entry = {"video_stream": video, "audio_stream": track,
                 "compared_seconds": len(rows), "max_gap_mib": round(max(g for _, g in rows) / MIB, 3)}
        for label, capacity in BUDGETS.items():
            count = run = longest = 0
            previous = None
            examples = []
            for second, gap in rows:
                if gap > capacity:
                    count += 1
                    run = run + 1 if previous == second - 1 else 1
                    longest = max(longest, run)
                    if run == min_run and len(examples) < 3:
                        examples.append({"start_seconds": second - run + 1, "gap_mib": round(gap / MIB, 3)})
                    previous = second
                else:
                    run = 0
                    previous = None
            entry[label] = {"affected_seconds": count, "fraction": round(count / len(rows), 4),
                            "longest_run_seconds": longest, "examples": examples}
        pairs.append(entry)
    # Prefer persistent separation exceeding the hub budget, then the smaller
    # transcoder budget. These scores are deliberately not stall probabilities.
    pairs.sort(key=lambda p: (p['hub']['longest_run_seconds'], p['transcoder']['longest_run_seconds'], p['max_gap_mib']), reverse=True)
    return pairs


def scan(path, ffprobe, timeout, min_run):
    # ffprobe primary docs: "-show_packets" shows each packet; show_entries
    # selects fields; compact writer emits one section per line. See
    # https://ffmpeg.org/ffprobe.html#Main-options and #compact_002c-csv
    # (checked 2026-09-23). No read_intervals: this examines the whole timeline.
    meta = subprocess.run([ffprobe, '-v', 'error', '-show_streams', '-of', 'json', str(path)],
                          capture_output=True, text=True, timeout=timeout, check=True)
    streams = json.loads(meta.stdout)['streams']
    videos = [s['index'] for s in streams if s.get('codec_type') == 'video' and not s.get('disposition', {}).get('attached_pic')]
    audio = [s['index'] for s in streams if s.get('codec_type') == 'audio']
    if not videos or not audio:
        return {"path": str(path), "status": "not_audio_video"}
    video = videos[0]
    selected = {video, *audio}
    buckets = {}
    missing = packets = backwards = 0
    last_pos = None
    expired = threading.Event()
    # stderr goes to a temporary file to prevent pipe deadlock on corrupt input.
    with tempfile.TemporaryFile(mode='w+') as errors:
        proc = subprocess.Popen([ffprobe, '-v', 'error', '-show_packets', '-show_entries',
            'packet=stream_index,dts_time,pts_time,pos,size', '-of', 'compact=p=0:nk=0', str(path)],
            stdout=subprocess.PIPE, stderr=errors, text=True)
        def expire():
            expired.set()
            proc.kill()
        timer = threading.Timer(timeout, expire)
        timer.start()
        try:
            for line in proc.stdout:
                fields = dict(part.split('=', 1) for part in line.strip().split('|') if '=' in part)
                try:
                    track = int(fields.get('stream_index', '-1'))
                    if track not in selected:
                        continue
                    packets += 1
                    stamp = fields.get('dts_time', 'N/A')
                    stamp = fields.get('pts_time', 'N/A') if stamp == 'N/A' else stamp
                    second = math.floor(float(stamp))
                    pos, size = int(fields['pos']), int(fields['size'])
                    if pos < 0 or size <= 0:
                        raise ValueError('unavailable offset')
                except (KeyError, ValueError, OverflowError):
                    missing += 1
                    continue
                if last_pos is not None and pos < last_pos:
                    backwards += 1
                last_pos = pos
                bounds = buckets.setdefault(second, {}).setdefault(track, [pos, pos + size])
                bounds[0], bounds[1] = min(bounds[0], pos), max(bounds[1], pos + size)
            code = proc.wait()
        finally:
            timer.cancel()
            if proc.poll() is None:
                proc.kill()
            proc.wait()
            proc.stdout.close()
        errors.seek(0)
        error = errors.read(2000).strip()
    if expired.is_set():
        raise TimeoutError(f'packet probe exceeded {timeout}s')
    if code:
        raise RuntimeError(f'ffprobe exited {code}: {error}')
    pairs = summarize(buckets, video, audio, min_run)
    by_index = {s['index']: s for s in streams}
    for pair in pairs:
        track = pair['audio_stream']
        pair['audio_track_zero_based'] = audio.index(track)
        pair['audio_codec'] = by_index[track].get('codec_name')
        pair['audio_language'] = by_index[track].get('tags', {}).get('language')
    candidate = any(p[label]['longest_run_seconds'] >= min_run for p in pairs for label in BUDGETS)
    return {"path": str(path), "status": "candidate" if candidate else "no_sustained_gap" if pairs else "inconclusive",
            "coverage": "incomplete" if missing or error else "complete", "packets": packets,
            "packets_without_position_or_time": missing, "ffprobe_warning": error or None,
            "backward_packet_steps_ffprobe_order": backwards, "pairs": pairs}


def files(paths):
    seen = set()
    for root in paths:
        root = Path(root).resolve()
        if not root.exists():
            raise FileNotFoundError(root)
        if root.is_file():
            entries = [root]
        else:
            def walk():
                for parent, dirs, names in os.walk(root):
                    dirs.sort()
                    for name in sorted(names):
                        if Path(name).suffix.lower() in EXTENSIONS:
                            yield Path(parent) / name
            entries = walk()
        for path in entries:
            resolved = path.resolve()
            if resolved not in seen:
                seen.add(resolved)
                yield resolved


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('paths', nargs='+', type=Path)
    parser.add_argument('--ffprobe', default='ffprobe')
    parser.add_argument('--timeout', type=float, default=120, help='per ffprobe invocation, seconds')
    parser.add_argument('--min-run', type=int, default=5, help='consecutive media seconds above a budget to flag (default: 5)')
    parser.add_argument('--limit', type=int, default=0, help='maximum files; 0 means all')
    parser.add_argument('--stop-after', type=int, default=0, help='stop after this many candidate files')
    parser.add_argument('--top', type=int, default=10)
    args = parser.parse_args()
    if args.timeout <= 0 or args.min_run < 1 or min(args.limit, args.stop_after, args.top) < 0:
        parser.error('timeout/min-run must be positive; counts must be nonnegative')
    ranked = []
    failures = count = candidates = 0
    for path in files(args.paths):
        if args.limit and count >= args.limit:
            break
        count += 1
        print(f'[{count}] {path}', file=sys.stderr, flush=True)
        try:
            result = scan(path, args.ffprobe, args.timeout, args.min_run)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError, TimeoutError, RuntimeError) as error:
            failures += 1
            result = {"path": str(path), "status": "error", "error": str(error)}
        print(json.dumps(result), flush=True)
        if result['status'] == 'candidate':
            candidates += 1
            ranked.append(result)
            ranked.sort(key=lambda r: (r['pairs'][0]['hub']['longest_run_seconds'], r['pairs'][0]['transcoder']['longest_run_seconds'], r['pairs'][0]['max_gap_mib']), reverse=True)
            del ranked[args.top:]
        if args.stop_after and candidates >= args.stop_after:
            break
    print(json.dumps({"summary": {"scanned": count, "candidates": candidates, "errors": failures,
        "top": ranked, "min_run_seconds": args.min_run, "budgets_bytes": BUDGETS, "meaning": "Layout-risk shortlist, not a GStreamer stall prediction. Test the reported audio stream and media position through the hub."}}), flush=True)
    return 1 if failures else 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
