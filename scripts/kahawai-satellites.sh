#!/usr/bin/env bash
# Fleet overview, including AIO's read-only built-in transcoder.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ${1:-} == check ]]; then
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test built_in_transcoder
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test auth_api built_in_transcoder
  exit
fi
[[ ${1:-status} == status ]] || { echo 'usage: kahawai-satellites.sh status | check' >&2; exit 2; }
: "${KAHAWAI_TOKEN:?Set KAHAWAI_TOKEN to an administrator access token}"
exec python3 - <<'PY'
import json, os, urllib.request
base = os.environ.get('KAHAWAI_URL', 'http://' + os.environ.get('KAHAWAI_API', '127.0.0.1:8420')).rstrip('/')
request = urllib.request.Request(base + '/admin/v1/satellites')
request.add_header('Authorization', 'Bearer ' + os.environ['KAHAWAI_TOKEN'])
with urllib.request.urlopen(request) as response:
    satellites = json.load(response)['satellites']
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
