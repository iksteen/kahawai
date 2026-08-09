#!/usr/bin/env bash
# Exercise refresh-family rotation, replay, concurrency and API logout.
#
#   kahawai-auth-cycle.sh [-a host:port] <username> [password]
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password      omitted or "-" prompts without echo
#
# Tokens stay inside the Python process and are never printed.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
while getopts "a:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -8; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 1 ] || { echo "usage: $(basename "$0") [-a host:port] <username> [password]" >&2; exit 2; }
USERNAME=$1
PASSWORD="${2:--}"
if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

KAHAWAI_AUTH_PASSWORD="$PASSWORD" python3 - "$API" "$USERNAME" <<'PY'
import concurrent.futures
import json
import os
import sys
import threading
import urllib.error
import urllib.request

base = "http://%s" % sys.argv[1]
username = sys.argv[2]
password = os.environ.pop("KAHAWAI_AUTH_PASSWORD")


def call(method, path, body=None, bearer=None):
    headers = {"content-type": "application/json"}
    if bearer:
        headers["authorization"] = "Bearer %s" % bearer
    request = urllib.request.Request(
        base + path,
        method=method,
        headers=headers,
        data=None if body is None else json.dumps(body).encode(),
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            raw = response.read()
            return response.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as error:
        error.read()
        return error.code, None


def expect(want, result, what):
    status, body = result
    if status != want:
        raise SystemExit("%s: expected HTTP %d, got %d" % (what, want, status))
    return body


def login():
    return expect(
        200,
        call("POST", "/api/v1/auth/token", {"username": username, "password": password}),
        "login",
    )


def refresh(token):
    return call("POST", "/api/v1/auth/refresh", {"refresh_token": token})


# Consumed-token replay revokes the rotated token but not another login.
first = login()
rotated = expect(200, refresh(first["refresh_token"]), "initial rotation")
expect(401, refresh(first["refresh_token"]), "consumed-token replay")
expect(401, refresh(rotated["refresh_token"]), "replayed family remains revoked")

# Logout is authenticated and revokes one current family.
logout_pair = login()
expect(
    204,
    call(
        "POST",
        "/api/v1/auth/logout",
        {"refresh_token": logout_pair["refresh_token"]},
        logout_pair["access_token"],
    ),
    "logout",
)
expect(401, refresh(logout_pair["refresh_token"]), "refresh after logout")

# Two simultaneous uses have one winner. The loser is replay, so the
# winner's replacement is intentionally unusable too.
contested = login()
barrier = threading.Barrier(2)


def race():
    barrier.wait()
    return refresh(contested["refresh_token"])


with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
    results = list(executor.map(lambda _: race(), range(2)))
if sorted(status for status, _ in results) != [200, 401]:
    raise SystemExit("concurrent refresh: expected one HTTP 200 and one 401, got %r"
                     % [status for status, _ in results])
winner = next(body for status, body in results if status == 200)
expect(401, refresh(winner["refresh_token"]), "concurrent replay family revocation")

print("auth refresh-family cycle passed")
PY
