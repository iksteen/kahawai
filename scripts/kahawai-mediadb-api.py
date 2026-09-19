#!/usr/bin/env python3
"""Mediadb API companion. Set KAHAWAI_TOKEN and KAHAWAI_API (host:port) or KAHAWAI_URL."""
import argparse
import getpass
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request


def request(method, path, body=None):
    headers = {"Content-Type": "application/json"}
    if token := os.environ.get("KAHAWAI_TOKEN"):
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(os.environ.get("KAHAWAI_URL", "http://" + os.environ.get("KAHAWAI_API", "127.0.0.1:8420")).rstrip("/") + path,
                                 data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            raw = response.read()
            return (json.loads(raw) if "application/json" in response.headers.get("Content-Type", "") else raw.decode()) if raw else None
    except urllib.error.HTTPError as error:
        sys.exit(f"HTTP {error.code}: {error.read().decode()}")


parser = argparse.ArgumentParser(description=__doc__)
sub = parser.add_subparsers(dest="command", required=True)
sub.add_parser("collections")
sub.add_parser("libraries")
match_command = sub.add_parser("match", help="apply enrichment correction JSON to a collection copy")
match_command.add_argument("copy")
match_command.add_argument("body_file")
artists = sub.add_parser("artists")
artists.add_argument("library")
artists.add_argument("--offset", type=int, default=0)
artists.add_argument("--limit", type=int, default=200)
artists.add_argument("--query", default="")
sub.add_parser("segments", help="show pending skip-point and loudness analysis per video collection")
sub.add_parser("item-log").add_argument("item")
login = sub.add_parser("login")
login.add_argument("username")
create = sub.add_parser("create-library")
create.add_argument("name")
create.add_argument("media_type", choices=["movies", "series", "anime", "music"])
create.add_argument("collections", nargs="*")
change = sub.add_parser("set-collections")
change.add_argument("library")
change.add_argument("collections", nargs="*")
sub.add_parser("delete-library").add_argument("library")
rescan = sub.add_parser("rescan")
rescan.add_argument("library")
rescan.add_argument("--deep", action="store_true")
items = sub.add_parser("items")
items.add_argument("library")
items.add_argument("--query", default="")
items.add_argument("--artist")
items.add_argument("--sort", default="title")
items.add_argument("--offset", type=int, default=0)
items.add_argument("--limit", type=int, default=200)
item = sub.add_parser("item")
item.add_argument("library")
item.add_argument("item")
children = sub.add_parser("children")
children.add_argument("library")
children.add_argument("item")
children.add_argument("--offset", type=int, default=0)
children.add_argument("--limit", type=int, default=200)
children.add_argument("--season")
children.add_argument("--disc")
mark = sub.add_parser("watched")
mark.add_argument("library")
mark.add_argument("item")
mark.add_argument("--unwatched", action="store_true")
mark.add_argument("--season")
for name in ("continue-watching", "up-next"):
    feed = sub.add_parser(name)
    feed.add_argument("--library")
    feed.add_argument("--offset", type=int, default=0)
    feed.add_argument("--limit", type=int, default=12)
playback = sub.add_parser("playback")
playback.add_argument("library")
playback.add_argument("item")
playback.add_argument("--entry")
playback.add_argument("--profile", help="CapabilityProfile JSON file")
playback.add_argument("--mode", choices=["direct", "remux", "transcode"])
start = sub.add_parser("start")
start.add_argument("library")
start.add_argument("item")
start.add_argument("--entry")
start.add_argument("--mode", choices=["direct", "remux", "transcode"])
start.add_argument("--position", type=int)
progress = sub.add_parser("progress")
progress.add_argument("session")
progress.add_argument("position", type=int)
sub.add_parser("stop").add_argument("session")
for name in ("subtitle-search", "subtitle-download", "subtitle-delete"):
    command = sub.add_parser(name)
    command.add_argument("library")
    command.add_argument("item")
    command.add_argument("entry")
    command.add_argument("version", help="source_version from playback's subtitle_source")
    if name == "subtitle-search":
        command.add_argument("--language", action="append", default=[])
    elif name == "subtitle-download":
        command.add_argument("file_id")
        command.add_argument("--language")
    else:
        command.add_argument("track", type=int)
args = parser.parse_args()
quote = lambda value: urllib.parse.quote(value, safe="")
base = "/api/v1/catalogue/libraries"
admin = "/admin/v1/catalogue/libraries"
match args.command:
    case "match":
        with open(args.body_file) as source:
            body = json.load(source)
        if "revision" not in body or "action" not in body:
            sys.exit("match requires revision and action; use enrichment detail to read the current revision")
        answer = request("POST", f"/admin/v1/enrich/items/{quote(args.copy)}/match", body)
    case "artists":
        answer = request("GET", f"{base}/{quote(args.library)}/artists?" + urllib.parse.urlencode({"offset":args.offset,"limit":args.limit,"q":args.query}))
    case "segments":
        answer = request("GET", "/admin/v1/segments")
    case "subtitle-search" | "subtitle-download" | "subtitle-delete":
        path = f"{base}/{quote(args.library)}/items/{quote(args.item)}/subtitles"
        source = {"media_entry_id": args.entry, "source_version": args.version}
        if args.command == "subtitle-search":
            answer = request("POST", path + "/search", {"source": source, "languages": args.language})
        elif args.command == "subtitle-download":
            answer = request("POST", path + "/download", {"source": source, "file_id": args.file_id, "language": args.language})
        else:
            answer = request("DELETE", path + f"/{args.track}", source)
    case "playback":
        body = {"mode": args.mode, "media_entry_id": args.entry}
        if args.profile:
            with open(args.profile) as profile:
                body["profile"] = json.load(profile)
        answer = request("QUERY", f"{base}/{quote(args.library)}/items/{quote(args.item)}", body)
    case "start":
        body = {"library_id": args.library, "item_id": args.item, "media_entry_id": args.entry, "mode": args.mode, "resume": args.position is None}
        if args.position is not None:
            body["start_ms"] = args.position
        answer = request("POST", "/api/v1/playback/sessions", body)
    case "item-log":
        print(request("GET", f"/admin/v1/items/{quote(args.item)}/log"), end="")
        sys.exit(0)
    case "progress":
        answer = request("POST", f"/api/v1/playback/sessions/{quote(args.session)}/progress", {"position_ms": args.position})
    case "stop":
        answer = request("DELETE", f"/api/v1/playback/sessions/{quote(args.session)}")
    case "continue-watching" | "up-next":
        params = {k: getattr(args, k) for k in ("library", "offset", "limit") if getattr(args, k) is not None}
        answer = request("GET", f"/api/v1/catalogue/{args.command}?" + urllib.parse.urlencode(params))
    case "watched":
        body = {"played": not args.unwatched}
        if args.season is not None:
            body["season"] = args.season
        answer = request("PUT", f"{base}/{quote(args.library)}/items/{quote(args.item)}/watched", body)
    case "login":
        answer = request("POST", "/api/v1/auth/token", {"client": "api", "username": args.username, "password": getpass.getpass()})
    case "collections":
        answer = request("GET", "/admin/v1/catalogue/collections")
    case "libraries":
        answer = request("GET", base)
    case "create-library":
        answer = request("POST", admin, {"name": args.name, "media_type": args.media_type, "collection_ids": args.collections})
    case "set-collections":
        answer = request("PUT", f"{admin}/{quote(args.library)}/collections", {"collection_ids": args.collections})
    case "rescan":
        answer = request("POST", f"{admin}/{quote(args.library)}/refresh?deep={str(args.deep).lower()}")
    case "delete-library":
        answer = request("DELETE", f"{admin}/{quote(args.library)}")
    case "items":
        answer = request("GET", f"{base}/{quote(args.library)}/items?" + urllib.parse.urlencode({k:v for k,v in {"offset":args.offset,"limit":args.limit,"q":args.query,"sort":args.sort,"artist":args.artist}.items() if v is not None}))
    case "children":
        params = {k: getattr(args, k) for k in ("offset", "limit", "season", "disc") if getattr(args, k) is not None}
        answer = request("GET", f"{base}/{quote(args.library)}/items/{quote(args.item)}/children?" + urllib.parse.urlencode(params))
    case "item":
        answer = request("GET", f"{base}/{quote(args.library)}/items/{quote(args.item)}")
print(json.dumps(answer, indent=2))
