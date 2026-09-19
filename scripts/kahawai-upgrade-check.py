#!/usr/bin/env python3
"""Rehearse the legacy upgrade and both restore paths using disposable profiles.
Usage: kahawai-upgrade-check.py OLD_KAHAWAI NEW_KAHAWAI
Neither binary is ever pointed at an existing installation.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

old, new = [Path(p).resolve() for p in sys.argv[1:]]
work = Path(tempfile.mkdtemp(prefix='kahawai-upgrade-check-'))
children = []


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def config(name):
    data = work / name
    api, setup, satellite = port(), port(), port()
    path = work / (name + '.toml')
    path.write_text(f'[hub]\ndata_dir={json.dumps(str(data))}\nbind="127.0.0.1:{api}"\nsetup_bind="127.0.0.1:{setup}"\nsatellite_bind="127.0.0.1:{satellite}"\n')
    return path, data, api, setup


def request(port, method, path, body=None, token=None):
    base = f'http://127.0.0.1:{port}'
    headers = {'Content-Type': 'application/json', 'Origin': base}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    req = urllib.request.Request(base + path, method=method, headers=headers,
                                 data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(req, timeout=5) as response:
        payload = response.read()
        return json.loads(payload) if payload else None


def start(binary, cfg):
    path, data, api, setup = cfg
    log = work / (data.name + '.log')
    with log.open('wb') as out:
        child = subprocess.Popen([str(binary), '--config', str(path), 'hub'], stdin=subprocess.DEVNULL, stdout=out, stderr=out)
    children.append(child)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        assert child.poll() is None, log.read_text()[-8000:]
        try:
            request(api, 'GET', '/api/v1/bootstrap')
            return child
        except (OSError, urllib.error.URLError):
            time.sleep(.1)
    raise AssertionError('hub startup timed out: ' + log.read_text()[-8000:])


def stop(child):
    child.terminate()
    child.wait(timeout=15)


def cli(binary, cfg, command, snapshot):
    subprocess.run([str(binary), '--config', str(cfg[0]), 'hub', command, str(snapshot)], check=True)


def login(cfg):
    return request(cfg[2], 'POST', '/api/v1/auth/token', {'client': 'api', 'username': 'upgrade-fixture', 'password': 'upgrade-fixture-password'})['access_token']


try:
    legacy = config('legacy')
    running = start(old, legacy)
    request(legacy[3], 'POST', '/api/v1/setup', {'username': 'upgrade-fixture', 'password': 'upgrade-fixture-password'})
    login(legacy)
    with sqlite3.connect(legacy[1] / 'hub.db') as db:
        assert db.execute('SELECT max(version) FROM _sqlx_migrations').fetchone() == (86,)
        user = db.execute('SELECT id FROM users').fetchone()[0]
        db.executescript("INSERT INTO libraries(id,name,media_type) VALUES('legacy-library','Old movies','movies');\nINSERT INTO library_items(id,kind,title,norm_title,sort_title,added_id) VALUES('legacy-item','movie','Old film','old film','old film','legacy-item');")
        db.execute('INSERT INTO user_item_state(user_id,item_id,position_ms,played,play_count) VALUES(?,?,3000,1,4)', (user, 'legacy-item'))
        db.execute("INSERT INTO settings(key,value) VALUES('upgrade-fixture','preserved')")
    stop(running)
    pre = work / 'pre-upgrade'
    cli(old, legacy, 'backup', pre)
    assert json.loads((pre / 'kahawai-backup.json').read_text())['format'] == 3
    upgraded = config('upgraded')
    shutil.copytree(legacy[1], upgraded[1])
    running = start(new, upgraded)
    access = login(upgraded)
    with sqlite3.connect(upgraded[1] / 'hub.db') as db:
        assert db.execute('SELECT max(version) FROM _sqlx_migrations').fetchone() == (89,)
        assert db.execute("SELECT count(*) FROM sqlite_master WHERE name IN ('library_items','user_item_state')").fetchone() == (0,)
        assert db.execute("SELECT value FROM settings WHERE key='upgrade-fixture'").fetchone() == ('preserved',)
        db.execute('INSERT INTO catalogue_watch_state(user_id,item_id,parent_id,position_ms,played,updated_at) VALUES(?,?,?,1234,0,42)', (user, 'new-item', 'new-item'))
    with sqlite3.connect(upgraded[1] / 'mediadb.db') as db:
        assert db.execute('SELECT max(version) FROM _sqlx_migrations').fetchone() == (5,)
        assert db.execute('SELECT count(*) FROM libraries').fetchone() == (0,)
    library = request(upgraded[2], 'POST', '/admin/v1/catalogue/libraries', {'name': 'New movies', 'media_type': 'movies', 'collection_ids': []}, access)['id']
    for file in pre.rglob('*'):
        relative = file.relative_to(pre)
        if file.is_file() and (relative.parts[0] == 'pki' or file.name.endswith('.secret') or file.name == 'credentials.key'):
            assert (upgraded[1] / relative).read_bytes() == file.read_bytes(), relative
    post = work / 'post-upgrade'
    cli(new, upgraded, 'backup', post)
    assert json.loads((post / 'kahawai-backup.json').read_text())['format'] == 4
    stop(running)
    restored = config('restored-new')
    cli(new, restored, 'restore', post)
    running = start(new, restored)
    access = login(restored)
    assert request(restored[2], 'GET', '/api/v1/catalogue/libraries', token=access)[0]['id'] == library
    with sqlite3.connect(restored[1] / 'hub.db') as db:
        assert db.execute('SELECT position_ms FROM catalogue_watch_state WHERE item_id=?', ('new-item',)).fetchone() == (1234,)
    stop(running)
    rollback = config('rollback-old')
    cli(old, rollback, 'restore', pre)
    running = start(old, rollback)
    login(rollback)
    with sqlite3.connect(rollback[1] / 'hub.db') as db:
        assert db.execute('SELECT max(version) FROM _sqlx_migrations').fetchone() == (86,)
        assert db.execute('SELECT position_ms,played,play_count FROM user_item_state WHERE item_id=?', ('legacy-item',)).fetchone() == (3000, 1, 4)
        assert db.execute('SELECT id FROM libraries').fetchone() == ('legacy-library',)
    assert not (rollback[1] / 'mediadb.db').exists()
    print('PASS: master-era backup, live upgrade, two-database restore and old-binary rollback')
finally:
    for child in children:
        if child.poll() is None:
            stop(child)
    if os.environ.get('KAHAWAI_KEEP_FIXTURE'):
        print('Fixture retained at', work)
    else:
        shutil.rmtree(work)
