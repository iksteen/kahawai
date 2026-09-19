#!/usr/bin/env python3
"""Check revision-bound embedded subtitles through an actual remote mediahost.
Usage: KAHAWAI_BIN=target/release/kahawai python3 scripts/kahawai-remote-subtitles-check.py HOST [SSH_KEY]
Requires ffmpeg locally and ~/kahawai-mediahost with staged GStreamer remotely.
Creates its own temporary hub, mediahost and generated media; never opens the
installation's databases or changes its media. Tests both revisions at one path.
"""
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request

host = sys.argv[1]
key = Path(sys.argv[2] if len(sys.argv) > 2 else '~/.ssh/id_rsa_agent').expanduser()
ssh = ['ssh', '-i', str(key), '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15']
scp = ['scp', '-q', '-i', str(key), '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes']
binary = Path(os.environ.get('KAHAWAI_BIN', 'target/release/kahawai')).resolve()
work = Path(tempfile.mkdtemp(prefix='kahawai-remote-subs-'))
children = []
remote = None
token = None


def run(args, **kw):
    return subprocess.run(args, check=True, **kw)


def remote_run(command):
    return run(ssh + [host, command], capture_output=True, text=True).stdout


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def until(test, label, seconds=180):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = test()
        if result:
            return result
        time.sleep(.5)
    raise AssertionError('Timed out: ' + label)


def api(method, path, body=None, port=None, raw=False):
    base = f'http://127.0.0.1:{port or api_port}'
    headers = {'Content-Type': 'application/json', 'Origin': base}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    req = urllib.request.Request(base + path, headers=headers, method=method,
                                 data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(req, timeout=180) as response:
        payload = response.read()
        return payload if raw else (json.loads(payload) if payload else None)


def generate(marker):
    # A tiny authored PGS display set avoids external media dependencies.
    pixels = run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                  f'color=black:s=640x80,drawtext=text={marker} IMAGE:fontcolor=white:fontsize=38:x=20:y=20',
                  '-frames:v', '1', '-pix_fmt', 'gray', '-f', 'rawvideo', '-'], capture_output=True).stdout
    rle = bytearray()
    for y in range(80):
        row = [int(p > 128) for p in pixels[y*640:(y+1)*640]]
        x = 0
        while x < 640:
            if row[x]:
                rle.append(1)
                x += 1
            else:
                end = x + 1
                while end < min(x + 63, 640) and not row[end]:
                    end += 1
                rle.extend([0, end-x])
                x = end
        rle.extend([0, 0])
    pcs = struct.pack('>HHBHBBBBHBBHH', 640, 360, 0x10, 1, 0x80, 0, 0, 1, 7, 0, 0, 0, 200)
    ods = b'\0\7\0\xc0' + (len(rle)+4).to_bytes(3, 'big') + struct.pack('>HH', 640, 80) + rle
    def segment(kind, data, pts=9000):
        return b'PG' + struct.pack('>IIBH', pts, 0, kind, len(data)) + data
    sup = work / 'image.sup'
    sup.write_bytes(segment(0x16, pcs) + segment(0x14, bytes([0,0,1,235,128,128,255])) + segment(0x15, ods) + segment(0x80, b'') + segment(0x16, pcs[:7] + bytes([0,0,0,0]), 450000) + segment(0x80, b'', 450000))
    srt = work / 'text.srt'
    srt.write_text(f'1\n00:00:00,100 --> 00:00:05,000\n{marker} TEXT\n')
    ass = work / 'styled.ass'
    ass.write_text('[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,DejaVu Sans,30,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n' + f'Dialogue: 0,0:00:00.10,0:00:05.00,Default,,0,0,0,,{marker} ASS\n')
    film = work / (marker + '.mkv')
    run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'color=black:s=640x360:r=5', '-f', 'lavfi', '-i', 'anullsrc=r=48000:cl=stereo', '-i', str(srt), '-i', str(ass), '-i', str(sup), '-map', '0:v', '-map', '1:a', '-map', '2:s', '-map', '3:s', '-map', '4:s', '-t', '8', '-c:v', 'libx264', '-c:a', 'aac', '-c:s', 'copy', '-metadata:s:s', 'language=eng', str(film)])
    return film


def start_hub():
    log = (work / 'hub.log').open('ab')
    child = subprocess.Popen([str(binary), '--config', str(work/'hub.toml'), 'hub'], stdout=log, stderr=log)
    log.close()
    children.append(child)
    def ready():
        assert child.poll() is None, (work/'hub.log').read_text()[-6000:]
        try:
            return api('GET', '/api/v1/bootstrap')
        except OSError:
            return False
    until(ready, 'hub startup')
    return child


try:
    films = [generate(marker) for marker in ['ORIGINAL', 'REPLACED']]
    api_port, setup_port, satellite_port = free_port(), free_port(), free_port()
    (work/'hub.toml').write_text(f'[hub]\nbind="127.0.0.1:{api_port}"\nsetup_bind="127.0.0.1:{setup_port}"\nsatellite_bind="127.0.0.1:{satellite_port}"\ndata_dir={json.dumps(str(work/"hub"))}\nhostnames=["localhost","127.0.0.1"]\n')
    hub = start_hub()
    api('POST', '/api/v1/setup', {'username':'fixture','password':'fixture-password'}, port=setup_port)
    token = api('POST', '/api/v1/auth/token', {'client':'api','username':'fixture','password':'fixture-password'})['access_token']
    remote = remote_run('mktemp -d /tmp/kahawai-remote-subs-XXXXXXXX').strip()
    assert re.fullmatch(r'/tmp/kahawai-remote-subs-[A-Za-z0-9]+', remote), remote
    remote_port = int(remote_run("python3 -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\",0)); print(s.getsockname()[1])'"))
    tunnel = subprocess.Popen(ssh + ['-o','ExitOnForwardFailure=yes','-N','-R',f'{remote_port}:127.0.0.1:{satellite_port}',host])
    children.append(tunnel)
    remote_run(f'mkdir {remote}/media')
    (work/'mediahost.toml').write_text(f'[mediahost]\nhub="localhost:{remote_port}"\nname="remote-subtitle-fixture"\nstate_dir="{remote}/state"\ndetect_segments=false\nrescan_minutes=0\n[[mediahost.collections]]\nname="movies"\nmedia_type="movies"\nroots=["{remote}/media"]\n')
    run(scp + [str(work/'mediahost.toml'), f'{host}:{remote}/mediahost.toml'])
    run(scp + [str(films[0]), f'{host}:{remote}/media/Fixture.mkv'])
    remote_run(f'GST_PLUGIN_PATH="$HOME/.local/lib/kahawai-gst/plugins" LD_LIBRARY_PATH="$HOME/.local/lib/kahawai-gst/lib" nohup "$HOME/kahawai-mediahost" --config {remote}/mediahost.toml >{remote}/mediahost.log 2>&1 </dev/null & echo $! >{remote}/pid')
    code = until(lambda: re.search(r'Enrollment code:\s*([^\n]+)', remote_run(f'cat {remote}/mediahost.log')), 'enrollment').group(1).strip()
    api('POST', '/admin/v1/enrollments/approve', {'code':code})
    def collection():
        return next((c for c in api('GET','/admin/v1/catalogue/collections') if c['file_count']==1 and not c['snapshot']), None)
    col = until(collection, 'remote catalogue import')
    library = api('POST','/admin/v1/catalogue/libraries',{'name':'Remote fixture','media_type':'movies','collection_ids':[col['id']]})['id']
    item = until(lambda: api('GET',f'/api/v1/catalogue/libraries/{library}/items')['items'], 'library item')[0]['id']
    item_path = f'/api/v1/catalogue/libraries/{library}/items/{item}'
    with sqlite3.connect(work/'hub'/'hub.db') as db:
        db.execute("INSERT INTO user_prefs(user_id,scope,key,value) SELECT id,'','ass_order','overlay,flatten,burn' FROM users")
    versions = []
    rasters = []
    for index, marker in enumerate(['ORIGINAL','REPLACED']):
        if index:
            run(scp + [str(films[index]), f'{host}:{remote}/replacement.mkv'])
            remote_run(f'mv {remote}/replacement.mkv {remote}/media/Fixture.mkv')
            until(lambda: api('QUERY',item_path,{'mode':'direct'})['subtitle_source']['source_version'] != versions[-1], 'replacement ingestion')
            # Restart only our disposable hub to exercise persisted cache lookup
            # and start a fresh idle OCR pass without its ten-minute interval.
            hub.terminate()
            hub.wait(timeout=15)
            hub = start_hub()
            until(lambda: any(c['connected'] for c in api('GET','/admin/v1/catalogue/collections')), 'remote reconnect')
        versions.append(api('QUERY',item_path,{'mode':'direct'})['subtitle_source']['source_version'])
        session = api('POST','/api/v1/playback/sessions',{'library_id':library,'item_id':item,'mode':'direct'})
        try:
            for fmt, suffix, expected in [('text','vtt','TEXT'),('ass','ass','ASS')]:
                track = next((t for t in session['subtitle_listing'] if t['format']==fmt and t['origin']=='embedded'), None)
                assert track, session['subtitle_listing']
                body = api('GET',f"/api/v1/playback/sessions/{session['session_id']}/subtitles/{track['id']}.{suffix}",raw=True)
                assert f'{marker} {expected}'.encode() in body, body
        finally:
            api('DELETE',f"/api/v1/playback/sessions/{session['session_id']}")
        def ocr_ready():
            files = list((work/'hub').rglob('*-ocr.json'))
            return next((f for f in files if f'{marker} IMAGE' in f.read_text()), None)
        until(ocr_ready, marker + ' remote image extraction and OCR', 240)
        session = api('POST','/api/v1/playback/sessions',{
            'library_id':library, 'item_id':item, 'mode':'direct',
            'profile':{'target_duration':{'mode':'ignore'}, 'graphics_overlay':True}})
        try:
            for origin, suffix in [('ocr','vtt'), ('raster','jsonl')]:
                track = next((t for t in session['subtitle_listing'] if t['origin']==origin), None)
                assert track, session['subtitle_listing']
                body = api('GET',f"/api/v1/playback/sessions/{session['session_id']}/subtitles/{track['id']}.{suffix}",raw=True)
                if origin == 'ocr':
                    assert f'{marker} IMAGE'.encode() in body, body
                else:
                    assert body, 'empty ASS overlay'
                    rasters.append(body)
        finally:
            api('DELETE',f"/api/v1/playback/sessions/{session['session_id']}")
        print('PASS:', marker, 'remote embedded text, ASS and image/OCR', flush=True)
    assert versions[0] != versions[1]
    assert rasters[0] != rasters[1], 'ASS overlay reused the old revision'
    print('PASS: replacing one remote media path did not reuse old subtitle artifacts', flush=True)
finally:
    if remote:
        remote_run(f'if test -f {remote}/pid; then kill "$(cat {remote}/pid)" 2>/dev/null || true; fi')
    for child in reversed(children):
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
    if os.environ.get('KAHAWAI_KEEP_FIXTURE'):
        print('Fixtures retained:', work, remote)
    else:
        if remote:
            remote_run(f'rm -rf -- {shlex.quote(remote)}')
        shutil.rmtree(work)
