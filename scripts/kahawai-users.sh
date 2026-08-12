#!/usr/bin/env bash
# Manage kahawai accounts and their library access (HUB-10).
#
#   kahawai-users.sh [-a host:port] <admin> <password> <command> [args]
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password "-"  prompt for it instead of passing on the command line
#
# Commands:
#   list                          accounts, and what each may see
#   create <user> <pass> [admin]  new account (open until you say otherwise)
#   delete <user>                 remove it, its watch state and its sessions
#   promote <user>                make it an admin
#   demote <user>                 back to an ordinary account, bound by grants
#   open <user>                   every library, including ones made later
#   close <user>                  nothing at all
#   grant <user> <library>        add one library (by name or id)
#   revoke <user> <library>       take one away
#
# An admin is never bound by grants; grant/revoke on one is recorded and
# takes effect only if the admin flag comes off.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"

while getopts "a:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -18; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] || { grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -18 >&2; exit 2; }
ADMIN=$1 PASSWORD=$2 CMD=$3
shift 3

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $ADMIN: " PASSWORD; echo >&2
fi

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$ADMIN" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["access_token"])') \
    || { echo "login failed" >&2; exit 1; }

# One python block rather than a pipeline per verb: grant and revoke are
# read-modify-write over the whole access set (the endpoint takes the set,
# not a delta), and splicing that through curl twice is where the race
# lives.
API="$API" TOKEN="$TOKEN" python3 - "$CMD" "$@" <<'PY'
import json, os, sys, urllib.error, urllib.request

BASE = "http://%s" % os.environ["API"]
AUTH = {"Authorization": "Bearer %s" % os.environ["TOKEN"],
        "content-type": "application/json"}


def call(method, path, body=None):
    req = urllib.request.Request(
        BASE + path, method=method, headers=AUTH,
        data=None if body is None else json.dumps(body).encode())
    try:
        with urllib.request.urlopen(req) as r:
            raw = r.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        sys.exit("%s %s: %s %s" % (method, path, e.code, e.read().decode().strip()))


def users():
    return call("GET", "/admin/v1/users")["users"]


def find(name):
    for u in users():
        if u["username"] == name:
            return u
    sys.exit("no such user: %s" % name)


def libraries():
    return call("GET", "/admin/v1/libraries")["libraries"]


def library_id(needle):
    libs = libraries()
    for l in libs:
        if l["id"] == needle or l["name"].lower() == needle.lower():
            return l["id"]
    sys.exit("no such library: %s (have: %s)"
             % (needle, ", ".join(l["name"] for l in libs)))


def set_access(user, all_libraries, libs):
    call("PUT", "/admin/v1/users/%s/libraries" % user["id"],
         {"all_libraries": all_libraries, "libraries": libs})


cmd, args = sys.argv[1], sys.argv[2:]

if cmd == "list":
    names = {l["id"]: l["name"] for l in libraries()}
    for u in users():
        if u["is_admin"]:
            access = "every library (admin)"
        elif u["all_libraries"]:
            access = "every library"
        elif u["libraries"]:
            access = ", ".join(sorted(names.get(i, i) for i in u["libraries"]))
        else:
            access = "NOTHING"
        print("%-20s %-6s %s" % (u["username"], "admin" if u["is_admin"] else "", access))

elif cmd == "create":
    if len(args) < 2:
        sys.exit("usage: create <user> <pass> [admin]")
    admin = len(args) > 2 and args[2] == "admin"
    call("POST", "/admin/v1/users",
         {"username": args[0], "password": args[1], "admin": admin})
    print("created %s%s — open to every library until you close it"
          % (args[0], " (admin)" if admin else ""))

elif cmd == "delete":
    if not args:
        sys.exit("usage: delete <user>")
    r = call("DELETE", "/admin/v1/users/%s" % find(args[0])["id"])
    print("deleted %s (%d session(s) ended)" % (args[0], r["sessions_ended"]))

elif cmd in ("promote", "demote"):
    if not args:
        sys.exit("usage: %s <user>" % cmd)
    u = find(args[0])
    # The hub refuses to strip the rights of the account you are signed in
    # as, and refuses to demote the last admin. Both come back as 409 with
    # the reason, so there is nothing to re-check here.
    call("PUT", "/admin/v1/users/%s/admin" % u["id"], {"admin": cmd == "promote"})
    print("%s is now %s" % (args[0], "an admin" if cmd == "promote" else "an ordinary account"))

elif cmd in ("open", "close"):
    if not args:
        sys.exit("usage: %s <user>" % cmd)
    u = find(args[0])
    set_access(u, cmd == "open", [] if cmd == "close" else u["libraries"])
    print("%s: %s" % (args[0], "every library" if cmd == "open" else "no access"))

elif cmd in ("grant", "revoke"):
    if len(args) < 2:
        sys.exit("usage: %s <user> <library>" % cmd)
    u, lib = find(args[0]), library_id(args[1])
    libs = set(u["libraries"])
    libs.add(lib) if cmd == "grant" else libs.discard(lib)
    set_access(u, False, sorted(libs))
    names = {l["id"]: l["name"] for l in libraries()}
    print("%s: %s" % (args[0], ", ".join(sorted(names.get(i, i) for i in libs)) or "NOTHING"))

else:
    sys.exit("unknown command: %s (try -h)" % cmd)
PY
