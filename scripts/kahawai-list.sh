#!/usr/bin/env bash
# List mediadb libraries, items, episodes and tracks.
#
#   kahawai-list.sh [-a host:port] [-l library] [-L|-p|-n|-r|-A artist|-i parent] <username> <password> [filter]
#
#   -L            list accessible libraries and their IDs
#   -l library    restrict browsing or a home feed to this library
#   -p / -n       continue watching / up next
#   -r            album artists (requires -l)
#   -A artist     an artist's albums, oldest first (requires -l)
#   -i parent     episodes or tracks of a library item (requires -l)
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password "-"  prompt for it
# All pages are read. Item rows include both library and stable item IDs.
set -euo pipefail
API="${KAHAWAI_API:-localhost:8420}"
MODE=items LIBRARY="" PARENT="" ARTIST=""
while getopts "a:l:LpnrA:i:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        l) LIBRARY="$OPTARG" ;;
        L) MODE=libraries ;;
        p) MODE=continue-watching ;;
        n) MODE=up-next ;;
        r) MODE=artists ;;
        A) MODE=albums; ARTIST="$OPTARG" ;;
        i) MODE=children; PARENT="$OPTARG" ;;
        h) sed -n '2,/^set /{ /^#/s/^# \{0,1\}//p; }' "$0"; exit 0 ;;
        *) exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ $# -ge 2 ] || { echo 'username and password (or - to prompt) are required' >&2; exit 2; }
case "$MODE" in artists|albums|children) [ -n "$LIBRARY" ] || { echo '-l library is required' >&2; exit 2; } ;; esac
USERNAME=$1 PASSWORD=$2
if [ "$PASSWORD" = - ]; then read -rsp "Password for $USERNAME: " PASSWORD; echo >&2; fi
exec python3 - "$API" "$USERNAME" "$PASSWORD" "$MODE" "$LIBRARY" "$ARTIST" "$PARENT" "${3:-}" <<'PY'
import json, sys, urllib.request, urllib.parse, urllib.error
address, username, password, mode, library, artist, parent, needle = sys.argv[1:]
base = 'http://' + address
token = None
quote = lambda s: urllib.parse.quote(s, safe='')
def request(path, body=None):
    headers = {'Content-Type': 'application/json'}
    if token: headers['Authorization'] = 'Bearer ' + token
    req = urllib.request.Request(base + path, headers=headers, data=None if body is None else json.dumps(body).encode())
    try:
        with urllib.request.urlopen(req, timeout=30) as response: return json.load(response)
    except urllib.error.HTTPError as e: sys.exit(f'HTTP {e.code}: {e.read().decode()}')
token = request('/api/v1/auth/token', {'client':'api', 'username':username, 'password':password})['access_token']
root = '/api/v1/catalogue/libraries'
libraries = request(root) if not library or mode == 'libraries' else [{'id':library}]
if mode == 'libraries':
    for lib in libraries:
        if needle.casefold() in lib['name'].casefold(): print(lib['id'], lib['media_type'], lib['name'], sep='  ')
    sys.exit(0)
feeds = mode in ('continue-watching', 'up-next')
shown = 0
for lib in ([{'id':library}] if feeds else libraries):
    lid = lib['id']
    path = f'{root}/{quote(lid)}'
    params = {}
    if feeds:
        path = '/api/v1/catalogue/' + mode
        if library: params['library'] = library
    elif mode == 'children': path += f'/items/{quote(parent)}/children'
    else:
        path += '/artists' if mode == 'artists' else '/items'
        if needle: params['q'] = needle
        if mode == 'albums': params.update(artist=artist, sort='year')
    key = 'artists' if mode == 'artists' else 'children' if mode == 'children' else 'items'
    offset = 0
    while True:
        answer = request(path + '?' + urllib.parse.urlencode(dict(params, offset=offset, limit=200)))
        rows = answer[key]
        for row in rows:
            if mode == 'artists':
                print(lid, row['key'], row['name'], f"[{row['album_count']} albums]", sep='  ')
                shown += 1
                continue
            title = row['title']
            if (feeds or mode == 'children') and needle.casefold() not in title.casefold(): continue
            position = (row.get('child') or row).get('position', {})
            if position.get('kind') == 'episode':
                number = f"E{position['episode']:02d}"
                if position.get('season') is not None: number = f"S{position['season']:02d}" + number
                title = f"{row.get('parent_title') or ''} {number} {title}".strip()
            elif position.get('kind') == 'track':
                title = f"Disc {position.get('disc') or '?'} Track {position['track']} {title}"
            watch = answer.get('watch', {}).get(row['id'], row)
            mark = ' [seen]' if watch.get('played') else ''
            if not mark and watch.get('resume_position_ms'):
                secs = watch['resume_position_ms'] // 1000
                mark = f' [resume {secs//60}:{secs%60:02d}]'
            copies = f" [{len(row['copy_ids'])} copies]" if 'copy_ids' in row else f" [{row['source_count']} sources]"
            year = f" ({row['year']})" if row.get('year') else ''
            print(row.get('library_id', lid), row['id'], title + year + copies + mark, sep='  ')
            shown += 1
        offset += len(rows)
        if offset >= answer['total']: break
        if not rows: sys.exit('Catalogue page was empty before its reported total')
print(f'-- {shown} {mode}', file=sys.stderr)
PY
