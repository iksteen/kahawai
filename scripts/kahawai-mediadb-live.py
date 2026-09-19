#!/usr/bin/env python3
"""Isolated real-process ingestion check. Owns every fixture, listener and child PID."""
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

binary = Path(sys.argv[1]).resolve()
assert binary.is_file(), f"missing binary: {binary}"
assert shutil.which("ffmpeg"), "ffmpeg is required to generate isolated media fixtures"
work = Path(tempfile.mkdtemp(prefix="kahawai-mediadb-live-"))
children = []
logs = []
token = None


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


api_port, setup_port, sat_port = port(), port(), port()
assert len({api_port, setup_port, sat_port}) == 3
url = f"http://127.0.0.1:{api_port}"


def api(method, path, body=None, base=url):
    headers = {"Content-Type": "application/json", "Origin": base}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(base + path, headers=headers, method=method,
                                 data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(req, timeout=5) as reply:
        raw = reply.read()
        return json.loads(raw) if raw else None


def until(check, description, seconds=60):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, urllib.error.URLError, sqlite3.Error) as error:
            last = error
        time.sleep(0.1)
    raise AssertionError(f"timeout: {description}; last error: {last}")


def item_log(item):
    req = urllib.request.Request(url + f"/admin/v1/items/{item}/log",
                                 headers={"Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=10) as response:
        assert "attachment" in response.headers["Content-Disposition"]
        return response.read().decode()


def start(service, config):
    log = work / f"{service}-{len(children)}.log"
    with log.open("wb") as out:
        child = subprocess.Popen([str(binary), "--config", str(config), service], stdin=subprocess.DEVNULL, stdout=out, stderr=out)
    children.append(child)
    logs.append(log)
    return child, log


def stop(child, crash=False):
    # Test-owned PIDs only. wait() proves termination before any replacement.
    if child.poll() is None:
        child.send_signal(signal.SIGKILL if crash else signal.SIGTERM)
    child.wait(timeout=15)
    assert child.returncode in (0, -signal.SIGTERM, -signal.SIGKILL), child.returncode


def query(name, sql):
    with sqlite3.connect(f"file:{work / 'hub' / name}?mode=ro", uri=True) as db:
        return db.execute(sql).fetchall()


def cli(script, *args, ok=True):
    executable = Path(__file__).resolve().parent / script
    result = subprocess.run([str(executable), *args], cwd=work, text=True, capture_output=True,
                            timeout=45, env={**os.environ, "KAHAWAI_API":f"127.0.0.1:{api_port}",
                                             "KAHAWAI_URL":url, "KAHAWAI_TOKEN":token})
    assert (result.returncode == 0) == ok, (script, args, result.returncode, result.stdout, result.stderr)
    return result


def check_cli(library, item, child_checks):
    auth = ["fixture", "fixture-password"]
    cli("kahawai-users.sh", *auth, "create", "cli-viewer", "cli-password")
    def viewer():
        return next(u for u in api("GET", "/admin/v1/users")['users'] if u['username'] == 'cli-viewer')
    assert viewer()['all_libraries'] is True
    assert 'cli-viewer' in cli("kahawai-users.sh", *auth, "list").stdout
    cli("kahawai-users.sh", *auth, "close", "cli-viewer")
    assert viewer()['all_libraries'] is False and viewer()['libraries'] == []
    name = next(l['name'] for l in api("GET", "/api/v1/catalogue/libraries") if l['id'] == library)
    cli("kahawai-users.sh", *auth, "grant", "cli-viewer", name)
    assert viewer()['libraries'] == [library]
    assert library in cli("kahawai-list.sh", "-L", "cli-viewer", "cli-password").stdout
    assert name in cli("kahawai-users.sh", *auth, "list").stdout
    cli("kahawai-users.sh", *auth, "revoke", "cli-viewer", library)
    assert viewer()['libraries'] == []
    assert library not in cli("kahawai-list.sh", "-L", "cli-viewer", "cli-password").stdout
    cli("kahawai-users.sh", *auth, "open", "cli-viewer")
    assert viewer()['all_libraries'] is True
    for command, expected in (("promote", True), ("demote", False)):
        cli("kahawai-users.sh", *auth, command, "cli-viewer")
        assert viewer()['is_admin'] is expected
    cli("kahawai-users.sh", *auth, "delete", "cli-viewer")
    assert all(u['username'] != 'cli-viewer' for u in api("GET", "/admin/v1/users")['users'])
    assert library in cli("kahawai-list.sh", "-L", *auth).stdout
    assert item in cli("kahawai-list.sh", "-l", library, *auth).stdout
    assert item in cli("kahawai-list.sh", *auth, "Dark").stdout
    assert item not in cli("kahawai-list.sh", "-l", library, *auth, "No such title").stdout
    for path, ids in child_checks:
        lid, parent = path.split('/')[5], path.split('/')[7]
        output = cli("kahawai-list.sh", "-l", lid, "-i", parent, *auth).stdout
        assert all(child in output for child in ids), output
        for child in ids:
            result = json.loads(cli("kahawai-query.sh", "-l", lid, "-j", *auth, child).stdout)
            assert result['id'] == child and result['sources'], result
    music_path, _ = child_checks[1]
    music = music_path.split('/')[5]
    artists = json.loads(cli("kahawai-mediadb.sh", "api", "artists", music).stdout)['artists']
    assert artists
    assert artists[0]['key'] in cli("kahawai-list.sh", "-l", music, "-r", *auth).stdout
    assert music_path.split('/')[7] in cli("kahawai-list.sh", "-l", music, "-A", artists[0]['key'], *auth).stdout
    show_library = child_checks[0][0].split('/')[5]
    assert child_checks[0][1][1] in cli("kahawai-list.sh", "-p", "-l", show_library, *auth).stdout
    cli("kahawai-list.sh", "-n", *auth)
    detail = json.loads(cli("kahawai-library.sh", "copies", library, item).stdout)
    entry = detail['sources'][0]['media_entry_id']
    preview = json.loads(cli("kahawai-query.sh", "-l", library, "-e", entry, "-j", *auth, item).stdout)
    assert preview['subtitle_source']['media_entry_id'] == entry, preview
    assert 'would play:' in cli("kahawai-query.sh", "-l", library, "-e", entry, *auth, item).stdout
    copy = detail['copy_ids'][0]
    cli("kahawai-enrich.sh", "detail", copy)
    cli("kahawai-enrich.sh", "items", "--library", library)
    # A stale write must fail without changing the fixture's assignment.
    (work / 'stale.json').write_text(json.dumps({'revision':-1, 'action':'clear'}))
    stale = cli("kahawai-library.sh", "match", copy, 'stale.json', ok=False)
    assert '409' in stale.stderr, stale
    temporary = json.loads(cli("kahawai-mediadb.sh", "api", "create-library", "CLI fixture", "movies").stdout)
    members = next(row['collection_ids'] for row in api("GET", "/api/v1/catalogue/libraries") if row['id'] == library)
    cli("kahawai-mediadb.sh", "api", "set-collections", temporary['id'], *members)
    assert api("GET", f"/api/v1/catalogue/libraries/{temporary['id']}/items")['items'][0]['id'] == item
    cli("kahawai-mediadb.sh", "api", "set-collections", temporary['id'])
    assert api("GET", f"/api/v1/catalogue/libraries/{temporary['id']}/items")['total'] == 0
    cli("kahawai-mediadb.sh", "api", "delete-library", temporary['id'])
    cli("kahawai-mediadb.sh", "api", "rescan", library)
    cli("kahawai-mediadb.sh", "api", "rescan", library, "--deep")
    cli("kahawai-mediadb.sh", "api", "segments")
    cli("kahawai-watched.sh", "-l", library, *auth, item)
    assert api("GET", f"/api/v1/catalogue/libraries/{library}/items/{item}")['played'] is True
    cli("kahawai-watched.sh", "-u", "-l", library, *auth, item)
    assert api("GET", f"/api/v1/catalogue/libraries/{library}/items/{item}")['played'] is False
    # A single-part source should fail the parts diagnostic after starting a
    # valid mediadb session, then clean that session up.
    result = cli("kahawai-parts.sh", "-l", library, "-e", entry, *auth, item, ok=False)
    assert 'not a multi-part source' in result.stderr, result
    print('CLI: users/access, libraries, browse, artists, children, feeds, query, corrections and rescan verified', flush=True)


def check_segment_admin(host):
    def reported():
        result = api("GET", "/admin/v1/segments")
        rows = [r for r in result["collections"] if r["mediahost_id"] == host]
        return rows if len(rows) == 2 and all(r["enabled"] is False for r in rows) else None
    rows = until(reported, "mediahost segment status reaches admin API")
    assert all(r["connected"] and r["pending_sources"] == 0 for r in rows), rows
    assert {r["name"] for r in rows} == {"series", "anime"}
    assert api("POST", "/admin/v1/segments") == {"asked":0,"unavailable":0}
    print("PASS: live segment status reports disabled detection without fake completion")


def check_rescans(library, count=2):
    def status():
        with sqlite3.connect(f"file:{work / 'mediahost' / 'catalog.db'}?mode=ro", uri=True) as db:
            return db.execute("SELECT scan_generation,scanning,scanned,skipped,failed FROM catalog_collections WHERE id='movies'").fetchone()
    for deep in (False, True, False):
        until(lambda: status()[1] == 0, "scan settled")
        before = status()
        answer = api("POST", f"/admin/v1/catalogue/libraries/{library}/refresh?deep={str(deep).lower()}")
        assert answer == {"asked": 1, "offline": 0, "unsupported": 0}, answer
        def completed():
            row = status()
            return row if row[0] > before[0] and row[1] == 0 else None
        row = until(completed, "deep rescan completed" if deep else "rescan completed")
        assert row[4] == 0, row
        assert row[2] == (count if deep else 0), row
        assert row[3] == (0 if deep else count), row
    print("PASS: normal skips, deep re-probes, next normal skips again")


def collections():
    return api("GET", "/admin/v1/catalogue/collections")


def ready_collection(host=None, count=2):
    return next((c for c in collections() if c["remote_id"] == "movies" and c["file_count"] == count and not c["snapshot"] and c["connected"]
                 and (host is None or c["mediahost_id"] == host)), None)


try:
    roots = [work / "source-a", work / "source-b"]
    for root in roots:
        root.mkdir()
    film = roots[0] / "Dark.City.1998.mkv"
    chapter_metadata = work / "chapters.txt"
    chapter_metadata.write_text(";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=30000\ntitle=Opening\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=30000\nEND=180000\ntitle=Feature\n")
    subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=64x64:r=5", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-i", str(chapter_metadata), "-map_metadata", "2", "-t", "180", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "5", "-c:a", "aac", str(film)], check=True)
    poster = roots[0] / "cover.jpg"
    subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=0x2e5f58:s=100x150", "-frames:v", "1", str(poster)], check=True)
    shutil.copyfile(poster, roots[1] / poster.name)
    other = roots[1] / film.name
    shutil.copyfile(film, other)
    for root in roots:
        (root / "Dark.City.1998.en.srt").write_text("1\n00:00:00,000 --> 00:00:05,000\nCatalogue playback subtitle\n")
        (root / "Dark.City.1998.nfo").write_text("<movie><title>Dark City</title><year>1998</year><plot>Metadata from the remote mediahost.</plot></movie>")
    (roots[0] / "Dark.City.1998.ja.ass").write_text('[Script Info]\nScriptType: v4.00+\nPlayResX: 64\nPlayResY: 64\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,DejaVu Sans,10,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,1,0,2,1,1,1,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:03:00.00,Default,,0,0,0,,Source ASS\n')
    hub_text = f'''[hub]
bind = "127.0.0.1:{api_port}"
setup_bind = "127.0.0.1:{setup_port}"
satellite_bind = "127.0.0.1:{sat_port}"
data_dir = {json.dumps(str(work / 'hub'))}
hostnames = ["localhost", "127.0.0.1"]
'''
    media_text = f'''name = "ingestion-fixture"
state_dir = {json.dumps(str(work / 'mediahost'))}
detect_segments = false
rescan_minutes = 0
[[mediahost.collections]]
name = "movies"
media_type = "movies"
roots = {json.dumps([str(r) for r in roots])}
'''
    series_root = work / "series"
    show_dir = series_root / "Example (2001)"
    show_dir.mkdir(parents=True)
    for name in ["Example.S01E01-E02", "Example.S01E03"]:
        shutil.copyfile(film, show_dir / (name + ".mkv"))
    (show_dir / "Example.S01E01-E02.nfo").write_text("<episodedetails><title>First episode</title><season>1</season><episode>1</episode><plot>Episode fixture.</plot></episodedetails>")
    music_root = work / "music"
    album_dir = music_root / "Artist" / "Album (2001)"
    album_dir.mkdir(parents=True)
    for number, title in [(1, "First song"), (2, "Second song")]:
        subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo",
                        "-t", "6", "-metadata", "album=Album", "-metadata", "album_artist=Artist", "-metadata", "artist=Artist",
                        "-metadata", "date=2001", "-metadata", f"track={number}", "-metadata", "disc=1",
                        "-metadata", f"title={title}", str(album_dir / f"{number:02d} - {title}.flac")], check=True)
    shutil.copyfile(poster, album_dir / "cover.jpg")
    anime_root = work / "anime"
    anime_root.mkdir()
    subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-i", str(film), "-map", "0", "-map_chapters", "-1", "-c", "copy", str(anime_root / "My.Neighbour.Totoro.1988.mkv")], check=True)
    shutil.copyfile(film, anime_root / "Example.S01E01.mkv")
    for name, kind, root in [("series", "series", series_root), ("music", "music", music_root), ("anime", "anime", anime_root)]:
        media_text += f'\n[[mediahost.collections]]\nname = "{name}"\nmedia_type = "{kind}"\nroots = [{json.dumps(str(root))}]\n'
    hub_cfg = work / "hub.toml"
    hub_cfg.write_text(hub_text)
    mh_cfg = work / "mediahost.toml"
    mh_cfg.write_text(f'[mediahost]\nhub = "localhost:{sat_port}"\n' + media_text)
    hub, _ = start("hub", hub_cfg)
    until(lambda: api("GET", "/api/v1/bootstrap", base=f"http://127.0.0.1:{setup_port}"), "setup listener")
    api("POST", "/api/v1/setup", {"username": "fixture", "password": "fixture-password"}, base=f"http://127.0.0.1:{setup_port}")
    token = api("POST", "/api/v1/auth/token", {"client": "api", "username": "fixture", "password": "fixture-password"})["access_token"]
    mediahost, mh_log = start("mediahost", mh_cfg)
    code = until(lambda: re.search(r"Enrollment code:\s*([^\n]+)", mh_log.read_text()), "mediahost enrollment code").group(1).strip()
    until(lambda: api("GET", "/admin/v1/enrollments")["pending"], "pending enrollment")
    api("POST", "/admin/v1/enrollments/approve", {"code": code})
    col = until(ready_collection, "remote mediahost import")
    until(lambda: len([c for c in collections() if c["file_count"] == 2 and not c["snapshot"]]) == 4, "episode, track and anime import")
    anime_source = next(c for c in collections() if c["remote_id"] == "anime")
    anime_library = api("POST", "/admin/v1/catalogue/libraries", {"name": "Fixture anime", "media_type": "anime", "collection_ids": [anime_source["id"]]})
    anime_path = f"/api/v1/catalogue/libraries/{anime_library['id']}/items"
    anime_items = api("GET", anime_path)["items"]
    assert sorted(i["kind"] for i in anime_items) == ["movie", "series"], anime_items
    movie = next(i for i in anime_items if i["kind"] == "movie")
    # A fixed provider answer in this disposable fixture avoids depending on
    # upstream search availability; the real API and browser consume the IDs.
    with sqlite3.connect(work / "hub" / "mediadb.db") as fixture_db:
        fixture_db.execute("INSERT INTO provider_records(id,provider,namespace,external_id,language,media_type,title,year,description_json) VALUES('community-fixture','tmdb','movie','1234','en','movies',?,?, '{}')", (movie["title"], movie["year"]))
        fixture_db.execute("INSERT OR REPLACE INTO metadata_assignments(item_id,record_id,manual,strength) VALUES(?,'community-fixture',1,100)", (movie["representative_id"],))
    assert api("GET", f"{anime_path}/{movie['id']}")["tmdb_id"] == 1234
    assert api("POST", f"{anime_path}/{movie['id']}", {"mode": "direct"})["negotiated"]["mode"] == "direct"
    child_checks = []
    for kind in ["series", "music"]:
        source = next(c for c in collections() if c["remote_id"] == kind)
        lib = api("POST", "/admin/v1/catalogue/libraries", {"name": f"Fixture {kind}", "media_type": kind, "collection_ids": [source["id"]]})
        path = f"/api/v1/catalogue/libraries/{lib['id']}/items"
        parent = api("GET", path)["items"][0]["id"]
        child_path = f"{path}/{parent}/children"
        result = api("GET", child_path)
        assert result["total"] == (3 if kind == "series" else 2), result
        ids = [c["id"] for c in result["children"]]
        assert all(i.startswith("child1:") for i in ids)
        assert all(api("GET", f"{path}/{i}")["sources"] for i in ids)
        child_checks.append((child_path, ids))

    # Real playback progress through the mediahost, with no legacy catalogue rows.
    show_path, episode_ids = child_checks[0]
    show_library = show_path.split('/')[5]
    session = api("POST", "/api/v1/playback/sessions", {"library_id":show_library,"item_id":episode_ids[1],"mode":"direct"})
    request = urllib.request.Request(url + session["stream_url"], headers={"Authorization":"Bearer " + token,"Range":"bytes=0-31"})
    with urllib.request.urlopen(request,timeout=10) as response:
        assert response.status == 206
        assert response.read() == film.read_bytes()[:32]
    api("POST",f"/api/v1/playback/sessions/{session['session_id']}/progress",{"position_ms":120000})
    api("DELETE",f"/api/v1/playback/sessions/{session['session_id']}")
    diagnostic_session = session['session_id']
    assert diagnostic_session in item_log(episode_ids[1])
    assert diagnostic_session in item_log(episode_ids[1].split(':')[1])
    assert api("GET", "/api/v1/catalogue/continue-watching")["items"][0]["id"] == episode_ids[1]
    if os.environ.get("KAHAWAI_MEDIADB_UI_CHECK") == "1":
        subprocess.run(["npm", "exec", "--", "playwright", "test", "--config", "test/browser/mediadb.config.ts"],
                       cwd="web", check=True,
                       env={**os.environ, "KAHAWAI_MEDIADB_UI_URL": url, "KAHAWAI_MEDIADB_UI_TOKEN": token})
    check_segment_admin(col["mediahost_id"])
    library = api("POST", "/admin/v1/catalogue/libraries", {"name": "Fixture movies", "media_type": "movies", "collection_ids": [col["id"]]})
    check_rescans(library["id"])
    items_path = f"/api/v1/catalogue/libraries/{library['id']}/items"
    page = api("GET", items_path)
    assert len(page["items"]) == 1 and len(page["items"][0]["copy_ids"]) == 2, page
    item = page["items"][0]["id"]
    copies = page["items"][0]["copy_ids"]
    assert query("hub.db", "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'") == [(0,)]
    assert query("mediadb.db", "SELECT COUNT(*) FROM files") == [(8,)]
    assert query("mediadb.db", "SELECT MAX(version) FROM _sqlx_migrations")[0][0] == 4
    until(lambda: all(api("GET", f"/admin/v1/enrich/items/{copy}")["input"]["selected"] for copy in copies), "local NFO enrichment despite unavailable remote providers")
    until(lambda: query("mediadb.db", f"SELECT count(*) FROM enrichment_jobs j JOIN collection_items i ON i.id=j.item_id WHERE i.collection_id='{col['id']}' AND j.provider='local' AND j.state='done'")[0][0] == len(copies),
          "local enrichment settled after library membership changes")
    # A local identity can make remote searches unnecessary, so those jobs may
    # already be done. The durable provider block proves the failure occurred.
    assert query("mediadb.db", "SELECT COUNT(*) FROM enrichment_providers WHERE provider IN ('tmdb','tvdb') AND due_at>0")[0][0] > 0
    assert api("GET", f"/admin/v1/enrich/items/{copies[0]}")["metadata"]["description"]["overview"] == "Metadata from the remote mediahost."
    check_cli(library['id'], item, child_checks)
    if os.environ.get('KAHAWAI_MEDIADB_CLI_CHECK') == '1':
        sys.exit(0)
    watch_paths = [f"{items_path}/{item}"] + [path.rsplit('/', 2)[0] + '/' + ids[0] for path, ids in child_checks]
    for path in watch_paths:
        api("PUT", path + "/watched", {"played": True})
        assert api("GET", path)["played"] is True
    # Seed only this disposable fixture; no external provider quota is spent.
    # The provider boundary is covered by the injected-provider router test.
    subtitle_path = f"{items_path}/{item}"
    preview = api("POST", subtitle_path, {"mode": "direct"})
    subtitle_source = preview["subtitle_source"]
    subtitle_owner = query("hub.db", "SELECT id FROM users WHERE is_admin=1")[0][0]
    payload = json.dumps({"cues": [{"start_ms": 0, "end_ms": 10000, "text": "Source-bound live subtitle"}], "ass": None})
    with sqlite3.connect(work / "hub" / "mediadb.db") as db:
        downloaded_id = db.execute("INSERT INTO downloaded_subtitles(media_entry_id,source_version,provider,provider_file_id,format,language,label,created_by,payload) VALUES(?,?,'fixture','live','srt','eng','Live source',?,?)",
                                  (subtitle_source["media_entry_id"], subtitle_source["source_version"], subtitle_owner, payload)).lastrowid
    before = hub.pid
    stop(hub, crash=True)
    hub, _ = start("hub", hub_cfg)
    assert hub.pid != before
    until(lambda: ready_collection(col["mediahost_id"]), "reconnect after hub crash")
    for path, ids in child_checks:
        assert [c["id"] for c in api("GET", path)["children"]] == ids
    for path in watch_paths:
        assert api("GET", path)["played"] is True, path
        assert "play_count" not in api("GET", path)
    preview = api("POST", subtitle_path, {"mode": "direct", "media_entry_id": subtitle_source["media_entry_id"]})
    assert any(t["id"] == -downloaded_id for t in preview["negotiated"]["subtitles"]), preview
    playback = api("POST", "/api/v1/playback/sessions", {"library_id":library["id"], "item_id":item, "mode":"direct", "media_entry_id":subtitle_source["media_entry_id"]})
    subtitle_url = f"/api/v1/playback/sessions/{playback['session_id']}/subtitles/{-downloaded_id}.vtt"
    assert api("DELETE", f"{subtitle_path}/subtitles/{-downloaded_id}", subtitle_source)["removed"]
    request = urllib.request.Request(url + subtitle_url, headers={"Authorization":"Bearer " + token})
    with urllib.request.urlopen(request, timeout=10) as response:
        assert response.status == 200 and b"Source-bound live subtitle" in response.read()
    api("DELETE", f"/api/v1/playback/sessions/{playback['session_id']}")
    restored = api("GET", items_path)["items"]
    assert restored[0]["id"] == item and restored[0]["copy_ids"] == copies, restored
    before = mediahost.pid
    stop(mediahost)
    mediahost, replay_log = start("mediahost", mh_cfg)
    assert mediahost.pid != before
    until(lambda: ready_collection(col["mediahost_id"]), "mediahost replay")
    until(lambda: "filesystem watches installed" in replay_log.read_text(), "mediahost watches ready")
    assert api("GET", items_path)["items"][0]["id"] == item
    # Only generated test fixtures are modified.
    other.unlink()
    until(lambda: ready_collection(col["mediahost_id"], count=1), "file tombstone")
    assert len(api("GET", items_path)["items"][0]["copy_ids"]) == 1
    api("DELETE", f"/admin/v1/satellites/{col['mediahost_id']}")
    assert collections() == []
    assert api("GET", items_path)["items"] == []
    assert query("mediadb.db", f"SELECT COUNT(*) FROM library_items WHERE id='{item}'") == [(1,)]
    assert query("mediadb.db", "SELECT COUNT(*) FROM collection_items") == [(0,)]
    assert query("hub.db", "SELECT COUNT(*) FROM catalogue_watch_state WHERE played=1") == [(3,)]
    stop(mediahost)
    stop(hub)
    # Same Store boundary with the actual in-process mediahost engine.
    aio_cfg = work / "aio.toml"
    aio_cfg.write_text(hub_text + '\n[all_in_one]\ntranscoder = false\n[mediahost]\n' + media_text)
    aio, _ = start("all-in-one", aio_cfg)
    local = until(lambda: ready_collection("local", count=1), "all-in-one import")
    api("PUT", f"/admin/v1/catalogue/libraries/{library['id']}/collections", {"collection_ids": [local["id"]]})
    check_rescans(library["id"], count=1)
    check_segment_admin("local")

    # Item diagnostics outlive source deletion and the hub's scratch purge.
    assert diagnostic_session in item_log(episode_ids[1])
    assert local["remote_id"] == "movies"
    local_library = api("POST", "/admin/v1/catalogue/libraries", {"name": "Local fixture", "media_type": "movies", "collection_ids": [local["id"]]})
    local_items = api("GET", f"/api/v1/catalogue/libraries/{local_library['id']}/items")["items"]
    assert len(local_items) == 1 and local_items[0]["id"] == item, local_items
    assert local_items[0]["played"] is True, "watch state must survive replacement of the entire mediahost"
    assert query("hub.db", "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'") == [(0,)]
    session=api("POST","/api/v1/playback/sessions",{"library_id":local_library["id"],"item_id":item,"mode":"direct"})
    request=urllib.request.Request(url+session["stream_url"],headers={"Authorization":"Bearer "+token,"Range":"bytes=0-31"})
    with urllib.request.urlopen(request,timeout=10) as response:
        assert response.status==206 and response.read()==film.read_bytes()[:32]
    api("DELETE",f"/api/v1/playback/sessions/{session['session_id']}")
    print("PASS: real remote + in-process mediahosts, coalescing, durable restart/replay, tombstones, deletion and archival")
except BaseException as error:
    if not isinstance(error, SystemExit) or error.code:
        for log in logs:
            print(f"--- {log.name} ---\n{log.read_text()[-12000:]}", file=sys.stderr)
    raise
finally:
    for child in reversed(children):
        if child.poll() is None:
            stop(child)
    shutil.rmtree(work)
