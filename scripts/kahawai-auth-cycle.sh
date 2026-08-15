#!/usr/bin/env bash
# Exercise API bearer auth, browser cookies, Origin checks and refresh families.
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
import base64
import concurrent.futures
import http.cookiejar
import json
import os
import sys
import threading
import urllib.error
import urllib.request

base = "http://%s" % sys.argv[1]
username = sys.argv[2]
password = os.environ.pop("KAHAWAI_AUTH_PASSWORD")


def call(method, path, body=None, bearer=None, origin=None, opener=None):
    headers = {"content-type": "application/json"}
    if bearer:
        headers["authorization"] = "Bearer %s" % bearer
    if origin is not None:
        headers["origin"] = origin
    request = urllib.request.Request(
        base + path,
        method=method,
        headers=headers,
        data=None if body is None else json.dumps(body).encode(),
    )
    open_request = opener.open if opener else urllib.request.urlopen
    try:
        with open_request(request, timeout=15) as response:
            raw = response.read()
            return (
                response.status,
                json.loads(raw) if raw else None,
                response.headers.get_all("Set-Cookie") or [],
            )
    except urllib.error.HTTPError as error:
        raw = error.read()
        return (
            error.code,
            json.loads(raw) if raw and error.headers.get_content_type() == "application/json" else None,
            error.headers.get_all("Set-Cookie") or [],
        )


def expect(want, result, what):
    status, body, cookies = result
    if status != want:
        raise SystemExit("%s: expected HTTP %d, got %d" % (what, want, status))
    return body, cookies


def assert_access_shape(token):
    try:
        header64, payload64, _signature = token.split(".")
        decode = lambda part: json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
        header, payload = decode(header64), decode(payload64)
    except (ValueError, KeyError, json.JSONDecodeError) as error:
        raise SystemExit("access token is not a JWT: %s" % error)
    expected = {
        "alg": (header.get("alg"), "HS256"),
        "iss": (payload.get("iss"), "urn:kahawai:hub"),
        "aud": (payload.get("aud"), "urn:kahawai:api"),
        "token_type": (payload.get("token_type"), "access"),
    }
    wrong = ["%s=%r" % (name, actual) for name, (actual, want) in expected.items() if actual != want]
    if wrong:
        raise SystemExit("access token has wrong authentication boundary: " + ", ".join(wrong))


def assert_api_pair(pair, cookies, what):
    if set(pair) != {"access_token", "refresh_token", "expires_in"}:
        raise SystemExit("%s: wrong API response shape %r" % (what, sorted(pair)))
    assert_access_shape(pair["access_token"])
    if not pair["refresh_token"]:
        raise SystemExit("%s: empty refresh token" % what)
    if cookies:
        raise SystemExit("%s: API mode emitted auth cookies" % what)


def api_login():
    pair, cookies = expect(
        200,
        call(
            "POST",
            "/api/v1/auth/token",
            {"client": "api", "username": username, "password": password},
        ),
        "API login",
    )
    assert_api_pair(pair, cookies, "API login")
    return pair


def api_refresh(token):
    result = call(
        "POST",
        "/api/v1/auth/refresh",
        {"client": "api", "refresh_token": token},
    )
    if result[0] == 200:
        assert_api_pair(result[1], result[2], "API refresh")
    return result


def assert_cookie(lines, name, path, max_age):
    matches = [line for line in lines if line.startswith(name + "=")]
    if len(matches) != 1:
        raise SystemExit("expected one %s cookie, got %r" % (name, lines))
    parts = [part.strip() for part in matches[0].split(";")]
    if not parts[0].split("=", 1)[1]:
        raise SystemExit("%s cookie has no value" % name)
    expected = {"Path=" + path, "Max-Age=" + str(max_age), "HttpOnly", "SameSite=Strict"}
    if set(parts[1:]) != expected:
        raise SystemExit("%s cookie attributes: expected %r, got %r" % (name, expected, set(parts[1:])))


def assert_browser_response(body, cookies, what):
    if set(body) != {"access_token", "expires_in"}:
        raise SystemExit("%s: browser response exposed the wrong fields %r" % (what, sorted(body)))
    assert_access_shape(body["access_token"])
    assert_cookie(cookies, "kahawai_refresh", "/api/v1/auth", 2592000)
    assert_cookie(cookies, "kahawai_media", "/api/v1", 900)


# API bearer families: replay, logout isolation and concurrency.
first = api_login()
rotated, _ = expect(200, api_refresh(first["refresh_token"]), "initial rotation")
expect(401, api_refresh(first["refresh_token"]), "consumed-token replay")
expect(401, api_refresh(rotated["refresh_token"]), "replayed family remains revoked")

logout_pair = api_login()
_body, cookies = expect(
    204,
    call(
        "POST",
        "/api/v1/auth/logout",
        {"client": "api", "refresh_token": logout_pair["refresh_token"]},
        logout_pair["access_token"],
    ),
    "API logout",
)
if cookies:
    raise SystemExit("API logout emitted auth cookies")
expect(401, api_refresh(logout_pair["refresh_token"]), "refresh after API logout")

contested = api_login()
barrier = threading.Barrier(2)


def race():
    barrier.wait()
    return api_refresh(contested["refresh_token"])


with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
    results = list(executor.map(lambda _: race(), range(2)))
if sorted(result[0] for result in results) != [200, 401]:
    raise SystemExit("concurrent refresh: expected one HTTP 200 and one 401, got %r"
                     % [result[0] for result in results])
winner = next(body for status, body, _cookies in results if status == 200)
expect(401, api_refresh(winner["refresh_token"]), "concurrent replay family revocation")

# Browser mode: server-only cookies, canonical Origin and narrow cookie reads.
jar = http.cookiejar.CookieJar()
browser = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
browser_body, browser_cookies = expect(
    200,
    call(
        "POST",
        "/api/v1/auth/token",
        {"client": "browser", "username": username, "password": password},
        origin=base,
        opener=browser,
    ),
    "browser login",
)
assert_browser_response(browser_body, browser_cookies, "browser login")

expect(200, call("HEAD", "/api/v1/events", opener=browser), "media-cookie event stream")
expect(401, call("GET", "/api/v1/items", opener=browser), "cookie catalogue rejection")
expect(
    401,
    call("POST", "/api/v1/playback/sessions", {}, opener=browser),
    "cookie mutation rejection",
)
expect(
    401,
    call("HEAD", "/api/v1/events", bearer="invalid", opener=browser),
    "invalid bearer precedence",
)

for bad_origin, label in [
    (None, "absent Origin"),
    ("null", "null Origin"),
    ("https://foreign.invalid", "foreign Origin"),
]:
    _body, cookies = expect(
        403,
        call(
            "POST",
            "/api/v1/auth/refresh",
            {"client": "browser"},
            origin=bad_origin,
            opener=browser,
        ),
        label,
    )
    if cookies:
        raise SystemExit("%s rotated or cleared cookies" % label)

browser_body, browser_cookies = expect(
    200,
    call(
        "POST",
        "/api/v1/auth/refresh",
        {"client": "browser"},
        origin=base,
        opener=browser,
    ),
    "browser refresh",
)
assert_browser_response(browser_body, browser_cookies, "browser refresh")

_body, cleared = expect(
    204,
    call(
        "POST",
        "/api/v1/auth/logout",
        {"client": "browser"},
        bearer=browser_body["access_token"],
        origin=base,
        opener=browser,
    ),
    "browser logout",
)
for name, path in [("kahawai_refresh", "/api/v1/auth"), ("kahawai_media", "/api/v1")]:
    matches = [line for line in cleared if line.startswith(name + "=;")]
    if len(matches) != 1 or "Path=" + path not in matches[0] or "Max-Age=0" not in matches[0]:
        raise SystemExit("browser logout did not clear %s with its original path" % name)

print("API and browser authentication cycle passed")
PY
