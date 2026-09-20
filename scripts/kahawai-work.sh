#!/usr/bin/env bash
# Background work companion: every queue the hub and its mediahosts run,
# in one shape, and the rerun that releases a parked hub queue.
#
#   kahawai-work.sh status                  every queue, one line each
#   kahawai-work.sh rerun <area> <queue>    e.g. rerun subtitles ocr
#   kahawai-work.sh check                   the tests behind it
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ${1:-} == check ]]; then
  cargo test -p kahawai-mediadb --test subtitle_jobs
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test work_api
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --lib -- queue:: subtitles::work
  exit
fi
: "${KAHAWAI_TOKEN:?Set KAHAWAI_TOKEN to an administrator access token}"
exec python3 - "$@" <<'PY'
import json, os, sys, urllib.request, urllib.error
base=os.environ.get("KAHAWAI_URL","http://"+os.environ.get("KAHAWAI_API","127.0.0.1:8420")).rstrip("/")
command=sys.argv[1] if len(sys.argv)>1 else "status"
method="GET"; body=None
if command=="status": path="/admin/v1/work"
elif command=="rerun" and len(sys.argv)==4:
    method="POST"; path="/admin/v1/work/rerun"
    body=json.dumps({"area":sys.argv[2],"queue":sys.argv[3]}).encode()
else:
    sys.exit("usage: kahawai-work.sh status | rerun <area> <queue> | check")
request=urllib.request.Request(base+path,data=body,method=method)
request.add_header("Authorization","Bearer "+os.environ["KAHAWAI_TOKEN"])
if body: request.add_header("Content-Type","application/json")
try:
    with urllib.request.urlopen(request) as response: answer=json.load(response)
except urllib.error.HTTPError as error:
    sys.exit(f"{error.code}: {error.read().decode(errors='replace')}")
if command!="status":
    print(json.dumps(answer)); sys.exit()
for q in answer["queues"]:
    where=f" {q['host']}/{q['collection']}" if q.get("host") else ""
    counts=" ".join(f"{k}={q[k]}" for k in ("pending","running","retry","blocked","done") if q[k])
    due=f" due={q['next_due']}" if q.get("next_due") else ""
    error=f" error={q['error']!r}" if q.get("error") else ""
    print(f"{q['area']}/{q['queue']}{where}: {counts or 'idle'}{due}{error}")
PY
