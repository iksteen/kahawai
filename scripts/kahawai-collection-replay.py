#!/usr/bin/env python3
"""One-shot migration-56 -> collection-scoped migration-53 logical replay.

The exporter is read-only. It derives collection identity only from exact
physical-source projections, consolidates temporary library presentations, and
aborts on divergent durable state. The importer rewrites a staged database in
one transaction; it never targets the running hub implicitly.
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
import pathlib
import sqlite3
import sys
from typing import Any, Iterable

ITEM_STATE_TABLES = {
    "provider_metadata": "item_id",
    "provider_queries": "item_id",
    "rejected_matches": "item_id",
    "manual_match": "item_id",
    "anime_ids": "item_id",
    "enrichment_queue": "item_id",
    "item_relations": "from_item",
    "watch_state": "item_id",
}
GLOBAL_TABLES = [
    "satellites",
    "satellite_audit",
    "libraries",
    "library_collections",
    "users",
    "refresh_families",
    "user_libraries",
    "user_prefs",
    "watch_state_archive",
    "ed2k_aid",
    "provider_ranks",
    "settings",
    "transcoder_pace",
]
ITEM_FIELDS = [
    "kind",
    "title",
    "norm_title",
    "year",
    "season",
    "episode",
    "artist",
    "sort_title",
    "norm_artist",
    "episode_end",
]


class ReplayError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise ReplayError(message)


def ro(path: pathlib.Path) -> sqlite3.Connection:
    uri = f"file:{path.resolve()}?mode=ro"
    db = sqlite3.connect(uri, uri=True)
    db.row_factory = sqlite3.Row
    return db


def rw(path: pathlib.Path) -> sqlite3.Connection:
    db = sqlite3.connect(path)
    db.row_factory = sqlite3.Row
    return db


def rows(db: sqlite3.Connection, sql: str, args: Iterable[Any] = ()) -> list[dict[str, Any]]:
    return [dict(r) for r in db.execute(sql, tuple(args))]


def canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def db_version(db: sqlite3.Connection) -> int:
    value = db.execute("SELECT max(version) FROM _sqlx_migrations").fetchone()[0]
    return int(value or 0)


def integrity(db: sqlite3.Connection, label: str) -> None:
    quick = db.execute("PRAGMA quick_check").fetchone()[0]
    if quick != "ok":
        fail(f"{label}: quick_check={quick!r}")
    fk = db.execute("SELECT count(*) FROM pragma_foreign_key_check").fetchone()[0]
    if fk:
        fail(f"{label}: {fk} foreign-key violations")


def require_columns(db: sqlite3.Connection, table: str, required: set[str]) -> None:
    found = {r[1] for r in db.execute(f"PRAGMA table_info({table})")}
    missing = required - found
    if missing:
        fail(f"{table}: missing columns {sorted(missing)}")


class Dsu:
    def __init__(self) -> None:
        self.parent: dict[str, str] = {}

    def add(self, value: str) -> None:
        self.parent.setdefault(value, value)

    def find(self, value: str) -> str:
        self.add(value)
        parent = self.parent[value]
        if parent != value:
            self.parent[value] = self.find(parent)
        return self.parent[value]

    def union(self, left: str, right: str) -> None:
        a, b = self.find(left), self.find(right)
        if a != b:
            self.parent[max(a, b)] = min(a, b)


def source_item_rows(source: sqlite3.Connection) -> dict[tuple[str, str, str, str], list[str]]:
    grouped: dict[tuple[str, str, str, str], set[str]] = collections.defaultdict(set)
    for row in rows(
        source,
        "SELECT module_id,collection_id,root_token,source_path,item_id "
        "FROM library_item_sources",
    ):
        grouped[(row["module_id"], row["collection_id"], row["root_token"], row["source_path"])].add(
            row["item_id"]
        )
    for row in rows(
        source,
        "SELECT s.module_id,s.collection_id,s.root_token,s.source_path,s.item_id "
        "FROM item_sources s WHERE NOT EXISTS (SELECT 1 FROM library_item_sources p "
        "WHERE (p.module_id,p.collection_id,p.path_rel)=(s.module_id,s.collection_id,s.path_rel))",
    ):
        grouped[(row["module_id"], row["collection_id"], row["root_token"], row["source_path"])].add(
            row["item_id"]
        )
    return {key: sorted(value) for key, value in grouped.items()}


def source_parts(source: sqlite3.Connection) -> dict[tuple[str, str, str, str], int | None]:
    grouped: dict[tuple[str, str, str, str], set[int | None]] = collections.defaultdict(set)
    for row in rows(
        source,
        "SELECT module_id,collection_id,root_token,source_path,part FROM library_item_sources",
    ):
        grouped[(row["module_id"], row["collection_id"], row["root_token"], row["source_path"])].add(
            row["part"]
        )
    for row in rows(
        source,
        "SELECT s.module_id,s.collection_id,s.root_token,s.source_path,s.part "
        "FROM item_sources s WHERE NOT EXISTS (SELECT 1 FROM library_item_sources p "
        "WHERE (p.module_id,p.collection_id,p.path_rel)=(s.module_id,s.collection_id,s.path_rel))",
    ):
        grouped[(row["module_id"], row["collection_id"], row["root_token"], row["source_path"])].add(
            row["part"]
        )
    conflicts = {key: values for key, values in grouped.items() if len(values) != 1}
    if conflicts:
        key = sorted(conflicts)[0]
        fail(f"source {key} has conflicting multipart ranks {sorted(conflicts[key], key=lambda v: (v is not None, v))}")
    return {key: next(iter(values)) for key, values in grouped.items()}


def build_components(source: sqlite3.Connection) -> tuple[list[dict[str, Any]], dict[str, int], dict[tuple[str, str, str, str], int]]:
    item_rows = {r["id"]: r for r in rows(source, "SELECT * FROM items")}
    source_items = source_item_rows(source)
    dsu = Dsu()
    evidence: dict[str, set[tuple[str, str]]] = collections.defaultdict(set)

    for key, item_ids in source_items.items():
        module_id, collection_id, _, _ = key
        for item_id in item_ids:
            if item_id not in item_rows:
                fail(f"source projection names missing item {item_id}")
            dsu.add(item_id)
            evidence[item_id].add((module_id, collection_id))
        for item_id in item_ids[1:]:
            dsu.union(item_ids[0], item_id)
        parents = sorted({item_rows[item_id]["parent_id"] for item_id in item_ids if item_rows[item_id]["parent_id"]})
        for parent in parents:
            if parent not in item_rows:
                fail(f"item has missing parent {parent}")
            dsu.add(parent)
            evidence[parent].add((module_id, collection_id))
        for parent in parents[1:]:
            dsu.union(parents[0], parent)

    grouped: dict[str, list[str]] = collections.defaultdict(list)
    for item_id in dsu.parent:
        grouped[dsu.find(item_id)].append(item_id)
    if set(item_rows) != set(dsu.parent):
        missing = sorted(set(item_rows) - set(dsu.parent))
        fail(f"{len(missing)} items have no exact-source collection evidence: {missing[:20]}")

    component_of: dict[str, int] = {}
    components: list[dict[str, Any]] = []
    for number, member_ids in enumerate(sorted((sorted(v) for v in grouped.values()), key=lambda v: v[0]), 1):
        collections_seen: set[tuple[str, str]] = set()
        for member in member_ids:
            collections_seen.update(evidence[member])
        if len(collections_seen) != 1:
            fail(f"component {member_ids} has collection evidence {sorted(collections_seen)}")
        module_id, collection_id = next(iter(collections_seen))
        preferred = min(member_ids, key=lambda value: (value.count(":"), len(value), value))
        first = item_rows[preferred]
        expected = {field: first[field] for field in ITEM_FIELDS}
        for member in member_ids:
            actual = {field: item_rows[member][field] for field in ITEM_FIELDS}
            if actual != expected:
                fail(f"item presentation conflict in {member_ids}: {canonical(actual)} != {canonical(expected)}")
        component = {
            "component": number,
            "module_id": module_id,
            "collection_id": collection_id,
            "preferred_id": preferred,
            "members": member_ids,
            "item": expected,
            "parent_component": None,
        }
        components.append(component)
        for member in member_ids:
            component_of[member] = number

    by_number = {c["component"]: c for c in components}
    for component in components:
        parent_components = {
            component_of[item_rows[member]["parent_id"]]
            for member in component["members"]
            if item_rows[member]["parent_id"] is not None
        }
        if len(parent_components) > 1:
            fail(f"component {component['members']} has divergent parents {sorted(parent_components)}")
        if parent_components:
            parent = next(iter(parent_components))
            if (by_number[parent]["module_id"], by_number[parent]["collection_id"]) != (
                component["module_id"], component["collection_id"]
            ):
                fail(f"component {component['members']} has a parent in another collection")
            component["parent_component"] = parent

    source_component: dict[tuple[str, str, str, str], int] = {}
    for key, item_ids in source_items.items():
        found = {component_of[item_id] for item_id in item_ids}
        if len(found) != 1:
            fail(f"exact source {key} has conflicting item assignments {sorted(found)}")
        source_component[key] = next(iter(found))
    return components, component_of, source_component


def item_state(source: sqlite3.Connection, component: dict[str, Any], table: str, item_column: str) -> list[dict[str, Any]]:
    states: list[list[dict[str, Any]]] = []
    columns = [r[1] for r in source.execute(f"PRAGMA table_info({table})") if r[1] != item_column]
    select = ",".join(columns)
    order = ",".join(columns) if columns else "1"
    for member in component["members"]:
        states.append(rows(source, f"SELECT {select} FROM {table} WHERE {item_column}=? ORDER BY {order}", (member,)))
    baseline = canonical(states[0])
    for member, state in zip(component["members"][1:], states[1:]):
        if canonical(state) != baseline:
            fail(f"{table} conflict among {component['members']}; {member} diverged")
    return states[0]


def exact_roots(source: sqlite3.Connection) -> tuple[list[dict[str, Any]], dict[tuple[str, str, str], str]]:
    output: list[dict[str, Any]] = []
    lookup: dict[tuple[str, str, str], str] = {}
    for collection in rows(source, "SELECT * FROM collections ORDER BY module_id,collection_id"):
        try:
            roots = json.loads(collection["exact_roots_json"] or "[]")
        except json.JSONDecodeError as error:
            fail(f"invalid exact_roots_json for {collection['module_id']}/{collection['collection_id']}: {error}")
        for entry in roots:
            token, path = entry.get("token"), entry.get("path")
            if not token or not path:
                fail(f"invalid exact root entry {entry!r}")
            key = (collection["module_id"], collection["collection_id"], token)
            old = lookup.setdefault(key, path)
            if old != path:
                fail(f"root token {token} maps to both {old} and {path}")
            output.append({"module_id": key[0], "collection_id": key[1], "root_token": token, "normalized_path": path})
    return output, lookup


def create_bundle(path: pathlib.Path) -> sqlite3.Connection:
    if path.exists():
        fail(f"bundle already exists: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(path)
    db.executescript(
        """
        PRAGMA journal_mode=DELETE;
        CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE components(component INTEGER PRIMARY KEY,module_id TEXT NOT NULL,
          collection_id TEXT NOT NULL,preferred_id TEXT NOT NULL,parent_component INTEGER,item_json TEXT NOT NULL);
        CREATE TABLE component_members(component INTEGER NOT NULL,item_id TEXT NOT NULL UNIQUE,
          PRIMARY KEY(component,item_id));
        CREATE TABLE item_state(table_name TEXT NOT NULL,component INTEGER NOT NULL,row_json TEXT NOT NULL,
          PRIMARY KEY(table_name,component,row_json));
        CREATE TABLE global_rows(table_name TEXT NOT NULL,row_json TEXT NOT NULL,
          PRIMARY KEY(table_name,row_json));
        CREATE TABLE collections(module_id TEXT NOT NULL,collection_id TEXT NOT NULL,row_json TEXT NOT NULL,
          PRIMARY KEY(module_id,collection_id));
        CREATE TABLE roots(module_id TEXT NOT NULL,collection_id TEXT NOT NULL,root_token TEXT NOT NULL,
          normalized_path TEXT NOT NULL,PRIMARY KEY(module_id,collection_id,root_token));
        CREATE TABLE sources(source_key INTEGER PRIMARY KEY,module_id TEXT NOT NULL,collection_id TEXT NOT NULL,
          root_token TEXT NOT NULL,path_rel TEXT NOT NULL,component INTEGER,row_json TEXT NOT NULL,
          UNIQUE(module_id,collection_id,root_token,path_rel));
        CREATE TABLE tracks(track_key INTEGER PRIMARY KEY,original_id INTEGER NOT NULL,component INTEGER NOT NULL,
          source_key INTEGER,derived_track_key INTEGER,row_json TEXT NOT NULL);
        CREATE TABLE image_failures(source_key INTEGER NOT NULL,row_json TEXT NOT NULL,
          PRIMARY KEY(source_key,row_json));
        """
    )
    return db


def export_bundle(source_path: pathlib.Path, bundle_path: pathlib.Path) -> None:
    source = ro(source_path)
    source.execute("BEGIN")  # one consistent snapshot even if export was started too early
    if db_version(source) != 56:
        fail(f"source must be migration 56, found {db_version(source)}")
    integrity(source, "source")
    require_columns(source, "items", {"library_id", "media_key"})
    require_columns(source, "files", {"root_token", "source_path"})
    components, component_of, source_component = build_components(source)
    roots_out, root_lookup = exact_roots(source)

    bundle = create_bundle(bundle_path)
    bundle.execute("INSERT INTO meta VALUES('format','kahawai-collection-replay-v1')")
    bundle.execute("INSERT INTO meta VALUES('source_path',?)", (str(source_path.resolve()),))
    bundle.execute("INSERT INTO meta VALUES('source_sha256',?)", (hashlib.sha256(source_path.read_bytes()).hexdigest(),))

    for component in components:
        bundle.execute(
            "INSERT INTO components VALUES(?,?,?,?,?,?)",
            (
                component["component"], component["module_id"], component["collection_id"],
                component["preferred_id"], component["parent_component"], canonical(component["item"]),
            ),
        )
        bundle.executemany(
            "INSERT INTO component_members VALUES(?,?)",
            ((component["component"], member) for member in component["members"]),
        )
        for table, item_column in ITEM_STATE_TABLES.items():
            for row in item_state(source, component, table, item_column):
                bundle.execute("INSERT INTO item_state VALUES(?,?,?)", (table, component["component"], canonical(row)))

    for table in GLOBAL_TABLES:
        for row in rows(source, f"SELECT * FROM {table}"):
            bundle.execute("INSERT INTO global_rows VALUES(?,?)", (table, canonical(row)))
    for row in rows(source, "SELECT * FROM collections"):
        normalized = {key: row[key] for key in ["module_id", "collection_id", "media_type", "roots_json", "sync_version", "root_adoption_pending"]}
        bundle.execute("INSERT INTO collections VALUES(?,?,?)", (row["module_id"], row["collection_id"], canonical(normalized)))
    bundle.executemany(
        "INSERT INTO roots VALUES(:module_id,:collection_id,:root_token,:normalized_path)", roots_out,
    )

    source_key_of: dict[tuple[str, str, str, str], int] = {}
    parts = source_parts(source)
    for number, row in enumerate(rows(source, "SELECT * FROM files ORDER BY module_id,collection_id,root_token,source_path"), 1):
        key = (row["module_id"], row["collection_id"], row["root_token"], row["source_path"])
        if key[2] and (key[0], key[1], key[2]) not in root_lookup:
            fail(f"source {key} has no persisted token/path binding")
        component = source_component.get(key)
        if key not in parts:
            fail(f"source {key} has no multipart-rank projection")
        payload = {name: row[name] for name in ["size", "mtime_unix", "head_xxh3", "tail_xxh3", "oshash", "streams_json", "ed2k", "subs_extracted", "revision"]}
        payload["part"] = parts[key]
        bundle.execute("INSERT INTO sources VALUES(?,?,?,?,?,?,?)", (number, key[0], key[1], key[2], key[3], component, canonical(payload)))
        source_key_of[key] = number

    track_rows = {r["id"]: r for r in rows(source, "SELECT * FROM subtitle_tracks")}
    projected: dict[int, set[int]] = collections.defaultdict(set)
    for row in rows(source, "SELECT item_id,track_id FROM item_subtitle_tracks"):
        projected[row["track_id"]].add(component_of[row["item_id"]])
    track_components: dict[int, set[int]] = {}
    pending = set(track_rows)
    while pending:
        progressed = False
        for track_id in list(pending):
            row = track_rows[track_id]
            if row["module_id"] is not None:
                key = (row["module_id"], row["collection_id"], row["root_token"], row["source_path"])
                if key not in source_key_of:
                    fail(f"subtitle track {track_id} names unknown source {key}")
                comps = {source_component[key]}
            elif row["derived_from"] is not None:
                if row["derived_from"] not in track_components:
                    continue
                comps = set(track_components[row["derived_from"]])
            else:
                comps = set(projected.get(track_id, set())) or {component_of[row["item_id"]]}
            if not comps:
                fail(f"subtitle track {track_id} has no collection item")
            track_components[track_id] = comps
            pending.remove(track_id)
            progressed = True
        if not progressed:
            fail(f"subtitle derivation cycle or missing parent: {sorted(pending)[:20]}")

    track_key_of: dict[tuple[int, int], int] = {}
    next_track_key = 1
    for track_id in sorted(track_rows):
        for component in sorted(track_components[track_id]):
            track_key_of[(track_id, component)] = next_track_key
            next_track_key += 1
    for (track_id, component), track_key in sorted(track_key_of.items(), key=lambda pair: pair[1]):
        row = track_rows[track_id]
        source_key = None
        if row["module_id"] is not None:
            source_key = source_key_of[(row["module_id"], row["collection_id"], row["root_token"], row["source_path"])]
        derived = track_key_of.get((row["derived_from"], component)) if row["derived_from"] is not None else None
        payload = {name: row[name] for name in ["origin", "stream_index", "format", "language", "label", "provider", "machine", "created_by", "created_at"]}
        bundle.execute("INSERT INTO tracks VALUES(?,?,?,?,?,?)", (track_key, track_id, component, source_key, derived, canonical(payload)))

    for row in rows(source, "SELECT * FROM image_set_failures"):
        key = (row["module_id"], row["collection_id"], row["root_token"], row["source_path"])
        source_key = source_key_of.get(key)
        if source_key is None:
            fail(f"image failure names unknown source {key}")
        payload = {name: row[name] for name in ["sub_index", "mtime_unix", "error", "at"]}
        bundle.execute("INSERT INTO image_failures VALUES(?,?)", (source_key, canonical(payload)))

    counts = {
        "components": len(components), "members": len(component_of), "sources": len(source_key_of),
        "tracks": len(track_key_of), "roots": len(roots_out),
    }
    bundle.execute("INSERT INTO meta VALUES('counts',?)", (canonical(counts),))
    bundle.commit()
    integrity(bundle, "bundle")
    print(canonical(counts))


def table_columns(db: sqlite3.Connection, table: str) -> list[str]:
    return [r[1] for r in db.execute(f"PRAGMA table_info({table})")]


def upsert_row(db: sqlite3.Connection, table: str, row: dict[str, Any]) -> None:
    columns = list(row)
    pk = [r[1] for r in sorted(db.execute(f"PRAGMA table_info({table})"), key=lambda value: value[5]) if r[5]]
    placeholders = ",".join("?" for _ in columns)
    names = ",".join(columns)
    update = [column for column in columns if column not in pk]
    if pk and update:
        clause = " ON CONFLICT(" + ",".join(pk) + ") DO UPDATE SET " + ",".join(f"{c}=excluded.{c}" for c in update)
    elif pk:
        clause = " ON CONFLICT(" + ",".join(pk) + ") DO NOTHING"
    else:
        clause = ""
    db.execute(f"INSERT INTO {table}({names}) VALUES({placeholders}){clause}", tuple(row[c] for c in columns))


def bundle_rows(bundle: sqlite3.Connection, table: str) -> list[dict[str, Any]]:
    return [json.loads(r[0]) for r in bundle.execute("SELECT row_json FROM global_rows WHERE table_name=?", (table,))]


def import_bundle(target_path: pathlib.Path, bundle_path: pathlib.Path, allow_unrecorded_test_schema: bool) -> None:
    bundle = ro(bundle_path)
    if bundle.execute("SELECT value FROM meta WHERE key='format'").fetchone()[0] != "kahawai-collection-replay-v1":
        fail("unsupported bundle format")
    integrity(bundle, "bundle")
    target = rw(target_path)
    version = db_version(target)
    if version != 53 and not (allow_unrecorded_test_schema and version == 52):
        fail(f"target must be migration 53, found {version}")
    integrity(target, "target before import")
    require_columns(target, "items", {"module_id", "collection_id"})
    require_columns(target, "files", {"id", "root_id", "item_id"})
    if target.execute("SELECT count(*) FROM sqlite_schema WHERE name IN ('library_item_sources','media_sources')").fetchone()[0]:
        fail("target still contains library-presentation compatibility objects")

    components = {r["component"]: dict(r) for r in bundle.execute("SELECT * FROM components")}
    bundle_sources = [dict(r) for r in bundle.execute("SELECT * FROM sources ORDER BY source_key")]
    target_files = rows(
        target,
        "SELECT f.id,f.module_id,f.collection_id,f.path_rel,f.item_id,r.root_token "
        "FROM files f LEFT JOIN collection_roots r ON r.id=f.root_id",
    )
    exact_target: dict[tuple[str, str, str, str], dict[str, Any]] = {}
    legacy_target: dict[tuple[str, str, str], dict[str, Any]] = {}
    for row in target_files:
        if row["root_token"] is None:
            legacy_target[(row["module_id"], row["collection_id"], row["path_rel"])] = row
        else:
            exact_target[(row["module_id"], row["collection_id"], row["root_token"], row["path_rel"])] = row

    target_items = {r["id"]: r for r in rows(target, "SELECT id,parent_id FROM items")}
    candidates: dict[int, set[str]] = collections.defaultdict(set)
    source_target_id: dict[int, int] = {}
    claimed_target_files: set[int] = set()
    for source in bundle_sources:
        key = (source["module_id"], source["collection_id"], source["root_token"], source["path_rel"])
        old = exact_target.get(key) or legacy_target.get((key[0], key[1], key[3]))
        if old and old["id"] not in claimed_target_files:
            source_target_id[source["source_key"]] = old["id"]
            claimed_target_files.add(old["id"])
            if source["component"] is not None and old["item_id"] is not None:
                candidates[source["component"]].add(old["item_id"])
                parent_component = components[source["component"]]["parent_component"]
                parent_id = target_items.get(old["item_id"], {}).get("parent_id")
                if parent_component is not None and parent_id is not None:
                    candidates[parent_component].add(parent_id)
    for component, values in candidates.items():
        if len(values) > 1:
            fail(f"target maps component {component} to several item ids: {sorted(values)}")

    item_id_of: dict[int, str] = {}
    used: set[str] = set()
    for component in sorted(components):
        values = candidates.get(component, set())
        if values:
            chosen = next(iter(values))
            if chosen in used:
                fail(f"target item {chosen} maps to more than one component")
            item_id_of[component] = chosen
            used.add(chosen)
    for component in sorted(components):
        if component in item_id_of:
            continue
        row = components[component]
        proposed = row["preferred_id"]
        if proposed in used:
            proposed = f"{proposed}:collection:{row['module_id']}:{row['collection_id']}"
        suffix = 2
        base = proposed
        while proposed in used:
            proposed = f"{base}:{suffix}"
            suffix += 1
        item_id_of[component] = proposed
        used.add(proposed)

    target.execute("PRAGMA defer_foreign_keys=ON")
    target.execute("BEGIN IMMEDIATE")
    try:
        # Upsert parent objects first; authoritative relationship tables are replaced below.
        for table in ["satellites", "libraries", "users"]:
            for row in bundle_rows(bundle, table):
                upsert_row(target, table, row)
        for row in bundle.execute("SELECT row_json FROM collections"):
            upsert_row(target, "collections", json.loads(row[0]))

        for table in ["subtitle_tracks", "image_set_failures"] + list(ITEM_STATE_TABLES):
            target.execute(f"DELETE FROM {table}")
        target.execute("UPDATE files SET item_id=NULL,root_id=NULL")
        target.execute("DELETE FROM items")

        # Roots are authoritative. IDs are local implementation details.
        target.execute("DELETE FROM collection_roots")
        root_id_of: dict[tuple[str, str, str], int] = {}
        for row in bundle.execute("SELECT * FROM roots ORDER BY module_id,collection_id,root_token"):
            columns = set(table_columns(target, "collection_roots"))
            values = {
                "module_id": row["module_id"], "collection_id": row["collection_id"],
                "root_token": row["root_token"], "normalized_path": row["normalized_path"],
            }
            if "configured" in columns:
                values["configured"] = 1
            upsert_row(target, "collection_roots", values)
            root_id_of[(row["module_id"], row["collection_id"], row["root_token"])] = target.execute(
                "SELECT id FROM collection_roots WHERE module_id=? AND collection_id=? AND root_token=?",
                (row["module_id"], row["collection_id"], row["root_token"]),
            ).fetchone()[0]

        remaining = set(components)
        while remaining:
            progressed = False
            for component in sorted(list(remaining)):
                row = components[component]
                parent = row["parent_component"]
                if parent is not None and parent in remaining:
                    continue
                item = json.loads(row["item_json"])
                item.update({
                    "id": item_id_of[component], "module_id": row["module_id"],
                    "collection_id": row["collection_id"],
                    "parent_id": item_id_of[parent] if parent is not None else None,
                })
                upsert_row(target, "items", item)
                remaining.remove(component)
                progressed = True
            if not progressed:
                fail(f"component parent cycle: {sorted(remaining)[:20]}")

        wanted_file_ids: set[int] = set()
        source_id_of: dict[int, int] = {}
        for source in bundle_sources:
            payload = json.loads(source["row_json"])
            root_id = root_id_of.get((source["module_id"], source["collection_id"], source["root_token"]))
            if source["root_token"] and root_id is None:
                fail(f"bundle source has unknown root token: {dict(source)}")
            file_id = source_target_id.get(source["source_key"])
            values = {
                "module_id": source["module_id"], "collection_id": source["collection_id"],
                "root_id": root_id, "path_rel": source["path_rel"],
                "item_id": item_id_of[source["component"]] if source["component"] is not None else None,
                **payload,
            }
            if file_id is None:
                columns = list(values)
                target.execute(
                    f"INSERT INTO files({','.join(columns)}) VALUES({','.join('?' for _ in columns)})",
                    tuple(values[c] for c in columns),
                )
                file_id = target.execute("SELECT last_insert_rowid()").fetchone()[0]
            else:
                assignments = ",".join(f"{column}=?" for column in values)
                target.execute(f"UPDATE files SET {assignments} WHERE id=?", (*values.values(), file_id))
            wanted_file_ids.add(file_id)
            source_id_of[source["source_key"]] = file_id
        if wanted_file_ids:
            placeholders = ",".join("?" for _ in wanted_file_ids)
            target.execute(f"DELETE FROM files WHERE id NOT IN ({placeholders})", tuple(sorted(wanted_file_ids)))
        else:
            target.execute("DELETE FROM files")

        for row in bundle.execute("SELECT table_name,component,row_json FROM item_state ORDER BY table_name,component"):
            payload = json.loads(row["row_json"])
            item_column = ITEM_STATE_TABLES[row["table_name"]]
            payload[item_column] = item_id_of[row["component"]]
            upsert_row(target, row["table_name"], payload)

        track_id_of: dict[int, int] = {}
        used_track_ids: set[int] = set()
        next_track_id = max((r[0] for r in bundle.execute("SELECT original_id FROM tracks")), default=0) + 1
        for row in bundle.execute("SELECT * FROM tracks ORDER BY track_key"):
            proposed = row["original_id"]
            if proposed in used_track_ids:
                proposed = next_track_id
                next_track_id += 1
            used_track_ids.add(proposed)
            track_id_of[row["track_key"]] = proposed
        pending = {r["track_key"]: dict(r) for r in bundle.execute("SELECT * FROM tracks")}
        inserted_tracks: set[int] = set()
        while pending:
            progressed = False
            for track_key in sorted(list(pending)):
                row = pending[track_key]
                derived = row["derived_track_key"]
                if derived is not None and derived not in inserted_tracks:
                    continue
                payload = json.loads(row["row_json"])
                payload.update({
                    "id": track_id_of[track_key], "item_id": item_id_of[row["component"]],
                    "source_id": source_id_of[row["source_key"]] if row["source_key"] is not None else None,
                    "derived_from": track_id_of[derived] if derived is not None else None,
                    "payload_id": row["original_id"] if row["source_key"] is None else None,
                })
                upsert_row(target, "subtitle_tracks", payload)
                del pending[track_key]
                inserted_tracks.add(track_key)
                progressed = True
            if not progressed:
                fail(f"track derivation cycle: {sorted(pending)[:20]}")
        for row in bundle.execute("SELECT * FROM image_failures"):
            payload = json.loads(row["row_json"])
            payload["source_id"] = source_id_of[row["source_key"]]
            upsert_row(target, "image_set_failures", payload)

        # Replace authoritative global relationship/state tables.
        for table in [
            "refresh_families", "user_libraries", "user_prefs", "watch_state_archive",
            "library_collections", "satellite_audit", "ed2k_aid", "provider_ranks", "settings", "transcoder_pace",
        ]:
            target.execute(f"DELETE FROM {table}")
            for row in bundle_rows(bundle, table):
                upsert_row(target, table, row)

        # Remove collections deleted after the level-52 backup. Files, items and
        # library composition are exact now, so no current durable row can be
        # reached only through one of these stale parents.
        collection_keys = [
            (r["module_id"],r["collection_id"])
            for r in bundle.execute("SELECT module_id,collection_id FROM collections")
        ]
        if collection_keys:
            target.execute("CREATE TEMP TABLE replay_collections(module_id TEXT,collection_id TEXT,PRIMARY KEY(module_id,collection_id)) WITHOUT ROWID")
            target.executemany("INSERT INTO replay_collections VALUES(?,?)",collection_keys)
            target.execute("DELETE FROM collections WHERE NOT EXISTS(SELECT 1 FROM replay_collections r WHERE (r.module_id,r.collection_id)=(collections.module_id,collections.collection_id))")
            target.execute("DROP TABLE replay_collections")
        else:
            target.execute("DELETE FROM collections")

        # Remove parent rows absent from the frozen source after children are exact.
        for table in ["users", "libraries", "satellites"]:
            source_rows = bundle_rows(bundle, table)
            pk = [r[1] for r in sorted(target.execute(f"PRAGMA table_info({table})"), key=lambda value: value[5]) if r[5]]
            if len(pk) != 1:
                fail(f"expected one-column primary key for {table}")
            keep = [row[pk[0]] for row in source_rows]
            if keep:
                target.execute(f"DELETE FROM {table} WHERE {pk[0]} NOT IN ({','.join('?' for _ in keep)})", keep)
            else:
                target.execute(f"DELETE FROM {table}")
        target.commit()
    except Exception:
        target.rollback()
        raise

    integrity(target, "target after import")
    expected = json.loads(bundle.execute("SELECT value FROM meta WHERE key='counts'").fetchone()[0])
    actual = {
        "components": target.execute("SELECT count(*) FROM items").fetchone()[0],
        "sources": target.execute("SELECT count(*) FROM files").fetchone()[0],
        "tracks": target.execute("SELECT count(*) FROM subtitle_tracks").fetchone()[0],
        "roots": target.execute("SELECT count(*) FROM collection_roots").fetchone()[0],
    }
    for key in actual:
        if actual[key] != expected[key]:
            fail(f"post-import {key}: expected {expected[key]}, got {actual[key]}")
    print(canonical(actual))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    export = sub.add_parser("export", help="export and preflight a frozen migration-56 database")
    export.add_argument("source", type=pathlib.Path)
    export.add_argument("bundle", type=pathlib.Path)
    imp = sub.add_parser("import", help="transactionally replay a bundle into a staged migration-53 database")
    imp.add_argument("bundle", type=pathlib.Path)
    imp.add_argument("target", type=pathlib.Path)
    imp.add_argument("--allow-unrecorded-test-schema", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    bundle_was_absent = args.command != "export" or not args.bundle.exists()
    try:
        if args.command == "export":
            export_bundle(args.source, args.bundle)
        else:
            import_bundle(args.target, args.bundle, args.allow_unrecorded_test_schema)
    except (ReplayError, sqlite3.Error, OSError, KeyError) as error:
        if args.command == "export" and bundle_was_absent:
            args.bundle.unlink(missing_ok=True)
        print(f"collection replay refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
