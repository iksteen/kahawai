#!/usr/bin/env python3
"""Exercise automatic subtitle deletion recovery through a disposable local AIO.
KAHAWAI_BIN=target/release/kahawai python3 scripts/kahawai-subtitle-cache-check.py
Requires ffmpeg. Owns only generated fixtures, temporary databases and its process.
--upgrade-from DATA_DIR checks a disposable snapshot of installed OCR caches,
without starting services or recognizing subtitles. No installed data is modified.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
import sys

if len(sys.argv) == 3 and sys.argv[1] == '--upgrade-from':
    source = Path(sys.argv[2]).expanduser().resolve()
    # Keep immutable hard links on the source filesystem. The transition only
    # renames cache paths; it never edits an existing artifact's contents.
    repo = Path(__file__).resolve().parent.parent
    target = repo/'target'
    target.mkdir(exist_ok=True)
    snapshot = Path(tempfile.mkdtemp(prefix='subtitle-upgrade-', dir=target))
    try:
        (snapshot/'.subtitle-upgrade-fixture').touch()
        with sqlite3.connect((source/'mediadb.db').as_uri()+'?mode=ro', uri=True) as original:
            with sqlite3.connect(snapshot/'mediadb.db') as dest:
                original.backup(dest)
        # Fail on the first unsupported hard link, rather than letting
        # copytree accumulate one error for every cache entry.
        def link(src, dst):
            try:
                os.link(src, dst)
            except OSError as error:
                raise RuntimeError('cache snapshot requires source and target on the same filesystem') from error
        shutil.copytree(source/'subtitles', snapshot/'subtitles', copy_function=link)
        env = dict(os.environ, KAHAWAI_SKIP_WEB_BUILD='1', KAHAWAI_SUBTITLE_UPGRADE_FIXTURE=str(snapshot))
        subprocess.run(['cargo','test','-p','kahawai-hub','--lib',
                        'subtitles::recovery::tests::installed_cache_upgrade_fixture',
                        '--','--ignored','--exact','--nocapture'], env=env, cwd=repo, check=True)
        print('PASS: installed OCR cache upgrade checked in an isolated snapshot; services remain stopped')
    finally:
        shutil.rmtree(snapshot)
    sys.exit()
elif len(sys.argv) != 1:
    sys.exit('usage: kahawai-subtitle-cache-check.py [--upgrade-from DATA_DIR]')

binary = Path(os.environ.get('KAHAWAI_BIN', 'target/release/kahawai')).resolve()
work = Path(tempfile.mkdtemp(prefix='kahawai-subtitle-cache-'))
child = None

def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

api_port, setup_port, satellite_port = port(), port(), port()

def until(check, label):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        assert child.poll() is None, (work/'aio.log').read_text()[-5000:]
        if check():
            return
        time.sleep(.2)
    raise AssertionError(label + ': ' + (work/'aio.log').read_text()[-5000:])

def start():
    global child
    with (work/'aio.log').open('ab') as log:
        child = subprocess.Popen([str(binary), '--config', str(work/'aio.toml'), 'all-in-one'], stdout=log, stderr=log)


def cache_ready():
    paths = list((work/'hub'/'subtitles'/'extracted-v4').rglob('*-e0.json'))
    return paths if paths and any('CACHE FIXTURE' in p.read_text() for p in paths) else False


def outstanding():
    with sqlite3.connect((work/'hub'/'mediadb.db').as_uri()+'?mode=ro', uri=True) as db:
        return db.execute('select count(*) from subtitle_jobs').fetchone()[0]

try:
    media = work/'media'; media.mkdir()
    (work/'text.srt').write_text('1\n00:00:00,100 --> 00:00:01,500\nCACHE FIXTURE\n')
    subprocess.run(['ffmpeg','-v','error','-f','lavfi','-i','color=black:s=320x180:r=5','-i',str(work/'text.srt'),
                    '-map','0:v','-map','1:s','-t','2','-c:v','libx264','-c:s','srt',str(media/'Fixture.mkv')],check=True)
    (work/'aio.toml').write_text(f'[hub]\nbind="127.0.0.1:{api_port}"\nsetup_bind="127.0.0.1:{setup_port}"\nsatellite_bind="127.0.0.1:{satellite_port}"\ndata_dir={json.dumps(str(work/"hub"))}\nhostnames=["localhost","127.0.0.1"]\n[mediahost]\nstate_dir={json.dumps(str(work/"mediahost"))}\ndetect_segments=false\nrescan_minutes=0\n[[mediahost.collections]]\nname="movies"\nmedia_type="movies"\nroots=[{json.dumps(str(media))}]\n')
    start()
    def ready():
        try:
            with urllib.request.urlopen(f'http://127.0.0.1:{api_port}/api/v1/bootstrap',timeout=2):
                return True
        except OSError:
            return False
    until(ready, 'AIO startup')
    request = urllib.request.Request(f'http://127.0.0.1:{setup_port}/api/v1/setup',
        data=json.dumps({'username':'fixture','password':'fixture-password'}).encode(),
        headers={'Content-Type':'application/json','Origin':f'http://127.0.0.1:{setup_port}'}, method='POST')
    with urllib.request.urlopen(request, timeout=10):
        pass
    until(cache_ready, 'initial background extraction')
    until(lambda: outstanding()==0, 'completed extraction has no queue row')
    with sqlite3.connect((work/'hub'/'mediadb.db').as_uri()+'?mode=ro',uri=True) as db:
        assert db.execute('select max(version) from _sqlx_migrations').fetchone()[0] == 9
    path = cache_ready()[0]; path.unlink()
    until(cache_ready, 'live deletion recovery without playback')
    until(lambda: outstanding()==0, 'live recovery completion')
    print('PASS: cached track recreated by background extraction without playback',flush=True)
    child.terminate(); child.wait(timeout=15)
    cache_ready()[0].unlink()
    start(); until(ready, 'AIO restart'); until(cache_ready, 'deletion while stopped recovered')
    until(lambda: outstanding()==0, 'restart recovery completion')
    print('PASS: startup repaired a missing artifact; completed queue rows are absent',flush=True)
    shutil.rmtree(work/'hub'/'subtitles'/'extracted-v4')
    until(cache_ready, 'whole extraction cache deletion recovery')
    until(lambda: outstanding()==0, 'bulk recovery completion')
    print('PASS: whole extraction cache recreated without playback',flush=True)
    print('PASS: migration 9 installed; no playback sessions were created',flush=True)
finally:
    if child and child.poll() is None:
        child.terminate(); child.wait(timeout=15)
    if os.environ.get('KAHAWAI_KEEP_FIXTURE'):
        print('Fixtures retained:', work)
    else:
        shutil.rmtree(work)
