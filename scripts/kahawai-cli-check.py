#!/usr/bin/env python3
"""CLI pagination and HTTP failure regression; real-hub checks live in kahawai-mediadb-live.py."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import threading
import urllib.parse

scripts = Path(__file__).resolve().parent
requests = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass

    def reply(self, body, status=200):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append((self.path, body))
        if self.path == '/api/v1/auth/token':
            self.reply({'access_token':'fixture-token'})
        elif self.path == '/api/v1/playback/sessions':
            self.reply({'code':'unplayable', 'message':'fixture has no playback source'}, 400)
        else:
            self.reply({'message':'retired route'}, 404)

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        query = urllib.parse.parse_qs(url.query)
        requests.append((url.path, query))
        if url.path == '/api/v1/catalogue/libraries':
            self.reply([{'id':'library', 'name':'Library', 'media_type':'movies'}])
        elif url.path == '/admin/v1/users':
            self.reply({'users':[{'id':'viewer', 'username':'viewer', 'is_admin':False,
                                  'all_libraries':False, 'libraries':[], 'grants_version':7}]})
        elif url.path == '/api/v1/catalogue/libraries/library/items':
            offset = int(query['offset'][0])
            self.reply({'items':[{'id':f'item-{i}', 'title':f'Film {i}', 'copy_ids':['copy'], 'year':2000}
                                 for i in range(offset, min(offset+200, 201))], 'total':201})
        else:
            self.reply({'message':'unknown library'}, 404)

    def do_PUT(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append((self.path, body))
        self.reply({'code':'stale_write', 'message':'library grants changed; reload and try again'}, 409)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
env = {**os.environ, 'KAHAWAI_API':f'127.0.0.1:{server.server_port}'}
try:
    result = subprocess.run([str(scripts/'kahawai-users.sh'), 'user', 'password', 'list'],
                            env=env, text=True, capture_output=True, timeout=20)
    assert result.returncode == 0 and 'viewer' in result.stdout, result
    for command, args, expected in (
        ('grant', ['Library'], {'all_libraries':False, 'libraries':['library']}),
        ('revoke', ['library'], {'all_libraries':False, 'libraries':[]}),
        ('open', [], {'all_libraries':True, 'libraries':[]}),
        ('close', [], {'all_libraries':False, 'libraries':[]}),
    ):
        requests.clear()
        result = subprocess.run([str(scripts/'kahawai-users.sh'), 'user', 'password', command, 'viewer', *args],
                                env=env, text=True, capture_output=True, timeout=20)
        assert result.returncode != 0 and '409' in result.stderr, result
        writes = [body for path, body in requests if path == '/admin/v1/users/viewer/libraries']
        assert writes == [{**expected, 'grants_version':7}], writes
    result = subprocess.run([str(scripts/'kahawai-list.sh'), 'user', 'password'], env=env, text=True, capture_output=True, timeout=20)
    assert result.returncode == 0, result.stderr
    assert 'item-200 ' in result.stdout, result.stdout
    assert len(result.stdout.splitlines()) == 201
    assert any(params.get('offset') == ['200'] for path, params in requests)
    result = subprocess.run([str(scripts/'kahawai-list.sh'), '-l', 'missing', 'user', 'password'], env=env, text=True, capture_output=True, timeout=20)
    assert result.returncode != 0 and 'HTTP 404' in result.stderr, result
    # These diagnostics should reach session creation with mediadb identity,
    # even when the source itself cannot play. No real load is generated.
    for script in ('kahawai-parts.sh', 'kahawai-avsync.sh', 'kahawai-latency.sh'):
        requests.clear()
        args = ['-l','library','-e','entry']
        if script == 'kahawai-latency.sh': args += ['-n','1','-c','1']
        result = subprocess.run([str(scripts/script), *args, 'user', 'password', 'child1:parent:e:1:2'],
                                env=env, text=True, capture_output=True, timeout=20)
        if script != 'kahawai-latency.sh':
            assert result.returncode != 0, (script, result)
        bodies = [body for path, body in requests if path == '/api/v1/playback/sessions']
        assert bodies, (script, result.stderr)
        assert all(body['library_id'] == 'library' and body['item_id'] == 'child1:parent:e:1:2'
                   and body['media_entry_id'] == 'entry' and body['resume'] is False for body in bodies), bodies
    print('OK: CLI grants/conflicts, pagination, HTTP failures and diagnostic session identities')
finally:
    server.shutdown()
    server.server_close()
    thread.join()
