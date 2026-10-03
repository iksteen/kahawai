#!/usr/bin/env python3
"""Check a real worker's HDR -> eight-bit SDR pipeline without a hub.

Pass a short PQ HDR clip and the candidate binary. Source files are read
only; outputs/logs remain in a private temporary directory for inspection.
Run under the deployment's GStreamer environment, without GL overrides.

Example: scripts/kahawai-tonemap-check.py --binary target/release/kahawai-transcoder \
    --source /tmp/hdr-fixture.mkv --expected-encoder nvh264enc
"""

import argparse
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import tempfile
import threading


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def probe(ffprobe, path):
    result = subprocess.run(
        [ffprobe, '-v', 'error', '-select_streams', 'v:0', '-show_streams',
         '-of', 'json', str(path)], check=True, capture_output=True, text=True,
        timeout=30,
    )
    return json.loads(result.stdout)['streams'][0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--expected-encoder', required=True)
    parser.add_argument('--config', type=Path)
    parser.add_argument('--ffprobe', default='ffprobe')
    parser.add_argument('--ffmpeg', default='ffmpeg')
    parser.add_argument('--timeout', type=int, default=180)
    parser.add_argument('--sink', choices=['hlssink2', 'hlssink3'])
    parser.add_argument('--allow-untagged-sdr', action='store_true',
                        help='permit encoders omitting color metadata; HDR tags still fail')
    args = parser.parse_args()
    source = args.source.resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    original = probe(args.ffprobe, source)
    require(original.get('color_transfer') == 'smpte2084', 'source must be PQ HDR')
    root = Path(tempfile.mkdtemp(prefix='kahawai-tonemap-'))
    print('Artifacts: ' + str(root), flush=True)
    endpoint = root / 'source.sock'
    errors = []
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(endpoint))
    server.listen(1)
    server.settimeout(args.timeout)

    def feed():
        try:
            conn, _ = server.accept()
            with conn, source.open('rb') as media:
                while True:
                    request = b''
                    while len(request) < 16:
                        part = conn.recv(16 - len(request))
                        if not part:
                            return
                        request += part
                    offset, length = struct.unpack('<QQ', request)
                    require(length <= 16 * 1024 * 1024, 'unexpected worker read size')
                    media.seek(offset)
                    data = media.read(length)
                    conn.sendall(struct.pack('<Q', len(data)) + data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # worker has ended; its exit status is checked separately
        except Exception as error:
            errors.append(str(error))
        finally:
            server.close()

    thread = threading.Thread(target=feed, daemon=True)
    thread.start()
    command = [str(binary)]
    if args.config:
        command += ['--config', str(args.config.resolve(strict=True))]
    command += ['remux-worker', str(endpoint), str(root), str(source.stat().st_size),
                '--video', 'encode', '--audio', 'off', '--video-codec', 'h264',
                '--video-kbps', '6000', '--max-bit-depth', '8', '--tone-map']
    if args.sink:
        command += ['--sink', args.sink]
    log_path = root / 'worker.log'
    with log_path.open('wb') as log:
        result = subprocess.run(command, stdout=log, stderr=log, timeout=args.timeout,
                                env={**os.environ, 'GST_DEBUG_NO_COLOR': '1'})
    thread.join(timeout=2)
    endpoint.unlink(missing_ok=True)
    require(result.returncode == 0, 'worker failed; inspect ' + str(log_path))
    require(not errors, 'source channel failed: ' + str(errors))
    log = re.sub(r'\x1b\[[0-9;]*m', '', log_path.read_text(errors='replace'))
    require(re.search(r'video encoder selected.*encoder="' +
                      re.escape(args.expected_encoder) + '"', log),
            'worker did not use expected encoder; inspect ' + str(log_path))
    require('encoded bit depth verified' in log, 'worker did not verify output depth')
    require('tone-map output selected' in log, 'tone map was not applied')
    if args.expected_encoder.startswith(('nv', 'va', 'vtenc')):
        require('format=(string)NV12' in log, 'hardware tone-map output must be NV12')
    segments = sorted(root.glob('segment*.ts'))
    require(len(segments) >= 2, 'need at least two HLS segments from the input clip')
    colorimetry = set()
    for segment in segments:
        video = probe(args.ffprobe, segment)
        require(video.get('codec_name') == 'h264' and video.get('pix_fmt') == 'yuv420p',
                'output must be eight-bit H.264: ' + str(video))
        transfer, matrix = video.get('color_transfer'), video.get('color_space')
        tagged = transfer == 'bt709' and matrix == 'bt709'
        untagged = transfer in (None, 'unknown') and matrix in (None, 'unknown')
        require(tagged or (args.allow_untagged_sdr and untagged),
                'output must be tagged SDR BT.709: ' + str(video))
        colorimetry.add('bt709' if tagged else 'unspecified')
        subprocess.run([args.ffmpeg, '-v', 'error', '-xerror', '-i', str(segment),
                        '-map', '0:v:0', '-f', 'null', '-'], check=True,
                       timeout=args.timeout, capture_output=True)
    print(json.dumps({'encoder': args.expected_encoder, 'segments': len(segments),
                      'pixel_format': 'yuv420p', 'colorimetry': sorted(colorimetry),
                      'artifacts': str(root)}))


if __name__ == '__main__':
    main()
