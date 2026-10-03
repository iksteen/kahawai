#!/usr/bin/env bash
# Fleet overview and video-executor draining, including AIO's built-in transcoder.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ${1:-} == check ]]; then
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test built_in_transcoder
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test auth_api built_in_transcoder
  exit
fi
: "${KAHAWAI_TOKEN:?Set KAHAWAI_TOKEN to an administrator access token}"
exec python3 - "$@" <<'PY'
import json, os, sys, urllib.error, urllib.parse, urllib.request
base = os.environ.get('KAHAWAI_URL', 'http://' + os.environ.get('KAHAWAI_API', '127.0.0.1:8420')).rstrip('/')
command = sys.argv[1] if len(sys.argv) > 1 else 'status'
if command in ('disable', 'enable') and len(sys.argv) == 3:
    module = sys.argv[2]
    path = '/admin/v1/satellites/' + urllib.parse.quote(module, safe='') + '/disabled'
    request = urllib.request.Request(base + path, method='POST', data=json.dumps({'disabled': command == 'disable'}).encode())
    request.add_header('Content-Type', 'application/json')
elif command == 'status' and len(sys.argv) <= 2:
    request = urllib.request.Request(base + '/admin/v1/satellites')
else:
    sys.exit('usage: kahawai-satellites.sh status | enable <module> | disable <module> | check')
request.add_header('Authorization', 'Bearer ' + os.environ['KAHAWAI_TOKEN'])
try:
    with urllib.request.urlopen(request, timeout=30) as response:
        if command != 'status':
            print(f'{module}: {"disabled" if command == "disable" else "enabled"}')
            sys.exit()
        satellites = json.load(response)['satellites']
except urllib.error.HTTPError as error:
    sys.exit(f'{error.code}: {error.read().decode(errors="replace")}')
for row in satellites:
    if row['cert_fingerprint'] == 'in-process' and row['module_type'] != 'transcoder':
        continue
    kind = 'built-in' if row['cert_fingerprint'] == 'in-process' else 'enrolled'
    status = 'disabled' if row['disabled'] else 'online' if row['connected'] else 'offline'
    print(f"{row['module_id']}: {row['name']} ({row['module_type']}, {kind}, {status})")
    for encoder in (row.get('capabilities') or {}).get('encoders', []):
        speeds = ' / '.join(f'{s:.1f}x' if s else 'unmeasured' for s in (encoder['speed_1080'], encoder['speed_2160']))
        print(f"  {encoder['codec']} {encoder['element']}: {speeds} (1080p / 2160p)")
    for pace in row['pace']:
        print(f"  {pace['class']}: {pace['multiple']:.1f}x observed")
PY
