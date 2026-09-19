#!/usr/bin/env bash
# Administrative enrichment companion. Never resets provider caches or pins.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ${1:-} == check ]]; then
  cargo test -p kahawai-mediadb --test enrichment
  KAHAWAI_SKIP_WEB_BUILD=1 cargo test -p kahawai-hub --test mediadb_ingestion
  exit
fi
: "${KAHAWAI_TOKEN:?Set KAHAWAI_TOKEN to an administrator access token}"
exec python3 - "$@" <<'PY'
import argparse, json, os, sys, urllib.request, urllib.parse, urllib.error
base=os.environ.get("KAHAWAI_URL","http://"+os.environ.get("KAHAWAI_API","127.0.0.1:8420")).rstrip("/")
command=sys.argv[1] if len(sys.argv)>1 else "status"
method="GET"; body=None
if command=="status": path="/admin/v1/enrich/progress"
elif command=="run": method="POST"; path="/admin/v1/enrich"
elif command=="items":
    parser=argparse.ArgumentParser(description="Review mediadb collection copies")
    for field in ("library","collection","q"): parser.add_argument("--"+field)
    parser.add_argument("--offset",type=int,default=0)
    parser.add_argument("--limit",type=int,default=200)
    options=vars(parser.parse_args(sys.argv[2:]))
    path="/admin/v1/enrich/items?"+urllib.parse.urlencode({k:v for k,v in options.items() if v is not None})
elif command=="detail" and len(sys.argv)==3: path=f"/admin/v1/enrich/items/{urllib.parse.quote(sys.argv[2],safe='')}"
elif command=="identities" and len(sys.argv)==4:
    path=f"/admin/v1/enrich/items/{urllib.parse.quote(sys.argv[2],safe='')}/identities?"+urllib.parse.urlencode({"q":sys.argv[3]})
elif command=="assign" and len(sys.argv)==5:
    path=f"/admin/v1/enrich/items/{urllib.parse.quote(sys.argv[2],safe='')}/match"; method="POST"
    body={"revision":int(sys.argv[3]),"action":"assign","library_item_id":sys.argv[4]}
elif command=="search" and len(sys.argv)==6:
    path=f"/admin/v1/enrich/items/{urllib.parse.quote(sys.argv[2],safe='')}/candidates"; method="POST"
    body={"revision":int(sys.argv[3]),"provider":sys.argv[4],"query":sys.argv[5]}
elif command in ("retry","clear","pick","confirm","reject","restore","supplement") and len(sys.argv)>=4:
    path=f"/admin/v1/enrich/items/{urllib.parse.quote(sys.argv[2],safe='')}/match"; method="POST"
    body={"revision":int(sys.argv[3]),"action":command,"record_id":sys.argv[4] if len(sys.argv)>4 else None}
else: raise SystemExit("usage: kahawai-enrich.sh status|run|items|detail ITEM|ACTION ITEM REVISION [RECORD]|search ITEM REVISION PROVIDER TITLE|identities ITEM TITLE|assign ITEM REVISION LIBRARY_ITEM|check")
request=urllib.request.Request(base+path,method=method,data=None if body is None else json.dumps(body).encode(),headers={"Authorization":"Bearer "+os.environ["KAHAWAI_TOKEN"],"Content-Type":"application/json"})
try:
    with urllib.request.urlopen(request,timeout=130) as response: print(json.dumps(json.load(response),indent=2))
except urllib.error.HTTPError as error:
    raise SystemExit(f"HTTP {error.code}: {error.read().decode()}")
PY
