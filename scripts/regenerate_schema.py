#!/usr/bin/env python3
"""Regenerate crates/remind_me_core/src/db/schema_*.sql from a `remind_me` checkout.

`schema_tables.sql`, `schema_indexes.sql` and `schema_triggers.sql` are
generated verbatim from the reference's own schema code — never hand-written.
`db/migrations.rs`'s module docstring explains why: an earlier version of this
port hand-transcribed the reference's migration ladder, three of the nineteen
steps were written from *this* crate's pre-existing tables rather than from the
reference, and the divergence went unnoticed because the parity check only
compared table names. Generating removes the transcription step that produces
that class of error.

ADR-0007 established the method and this script is that method, made
repeatable: import `remind_me_mcp.db` standalone, call `_ensure_schema` against
a fresh in-memory connection (which ends by calling `_migrate_schema`, so the
result is the reference's *current* schema, not its v1 base), and dump
`sqlite_master`.

Two adjustments are applied to the dump, matching this repo's existing
convention exactly:

* `IF NOT EXISTS` is re-added. SQLite strips it before storing a statement, but
  `migrations.rs` executes these files against databases that may already hold
  some of the objects.
* FTS5's own shadow tables (`memories_fts_data`, `_idx`, `_docsize`, ...) and
  `sqlite_sequence` are excluded. They are created implicitly by the virtual
  table and by AUTOINCREMENT; emitting them would make the file fail to load.

Usage:

    python3 scripts/regenerate_schema.py --reference /path/to/remind_me

The reference checkout is *not* imported for anything but its DDL, so its
runtime dependencies (`httpx`, `numpy` via `embeddings`) are stubbed rather
than installed — the stubs are never called, only imported.
"""
from __future__ import annotations

import argparse
import sqlite3
import sys
import types
from pathlib import Path

# Objects SQLite maintains itself. Emitting their DDL would either fail to
# execute or duplicate what the virtual table already creates.
_SHADOW_SUFFIXES = ("_data", "_idx", "_docsize", "_content", "_config")
_FTS_TABLES = ("memories_fts", "wiki_fts")

_HEADER = """-- GENERATED from remind_me's schema. Do not hand-edit.
-- Regenerate with: python3 scripts/regenerate_schema.py --reference <path>
"""


def _install_stubs(reference: Path) -> None:
    """Stand in for the reference's runtime-only dependencies.

    `remind_me_mcp.db` imports `httpx` at module scope and pulls in
    `remind_me_mcp.embeddings`, which imports `numpy` purely to be importable.
    Neither participates in DDL. Stubbing beats installing here because it
    keeps the generated schema a function of the reference's *source*, not of
    whichever dependency versions happen to resolve today.

    The package root needs the same treatment for a different reason:
    `remind_me_mcp/__init__.py` imports the entire MCP tool surface, dragging
    in pydantic, the MCP SDK and the rest of the runtime. A namespace stub
    carrying only `__path__` lets `remind_me_mcp.db` resolve as a submodule
    without that `__init__` body ever executing — which is what "import db.py
    standalone" in ADR-0007 means in practice.
    """
    for name in ("httpx", "numpy"):
        if name not in sys.modules:
            sys.modules[name] = types.ModuleType(name)

    package = types.ModuleType("remind_me_mcp")
    package.__path__ = [str(reference / "remind_me_mcp")]  # type: ignore[attr-defined]
    sys.modules["remind_me_mcp"] = package

    embeddings = types.ModuleType("remind_me_mcp.embeddings")

    def _unavailable(*_args: object, **_kwargs: object) -> None:
        raise RuntimeError(
            "embeddings is stubbed for schema generation and must not be called"
        )

    embeddings._get_embedder = _unavailable  # type: ignore[attr-defined]
    embeddings.chunk_text = _unavailable  # type: ignore[attr-defined]
    sys.modules["remind_me_mcp.embeddings"] = embeddings


def _is_shadow(name: str) -> bool:
    return any(
        name == f"{fts}{suffix}" for fts in _FTS_TABLES for suffix in _SHADOW_SUFFIXES
    )


def _reinstate_if_not_exists(sql: str, kind: str) -> str:
    """Put back the `IF NOT EXISTS` SQLite strips when it stores a statement."""
    prefixes = {
        "table": ("CREATE TABLE ", "CREATE VIRTUAL TABLE "),
        "index": ("CREATE UNIQUE INDEX ", "CREATE INDEX "),
        "trigger": ("CREATE TRIGGER ",),
    }[kind]
    for prefix in prefixes:
        if sql.startswith(prefix):
            if sql.startswith(f"{prefix}IF NOT EXISTS "):
                return sql
            return sql.replace(prefix, f"{prefix}IF NOT EXISTS ", 1)
    return sql


def _dump(conn: sqlite3.Connection, kind: str) -> str:
    rows = conn.execute(
        "SELECT name, sql FROM sqlite_master WHERE type = ? AND sql IS NOT NULL"
        " ORDER BY name",
        (kind,),
    ).fetchall()

    statements = [
        _reinstate_if_not_exists(sql, kind)
        for name, sql in rows
        if not _is_shadow(name) and name != "sqlite_sequence"
    ]
    return _HEADER + "\n" + "\n\n".join(f"{s};" for s in statements) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--reference",
        required=True,
        type=Path,
        help="Path to a remind_me checkout (the directory containing remind_me_mcp/)",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parent.parent
        / "crates"
        / "remind_me_core"
        / "src"
        / "db",
        help="Directory to write schema_*.sql into",
    )
    args = parser.parse_args()

    reference = args.reference.resolve()
    if not (reference / "remind_me_mcp" / "db.py").is_file():
        print(f"error: no remind_me_mcp/db.py under {reference}", file=sys.stderr)
        return 1

    sys.path.insert(0, str(reference))
    _install_stubs(reference)

    from remind_me_mcp import db as reference_db  # noqa: PLC0415 — after stubs

    conn = sqlite3.connect(":memory:")
    reference_db._ensure_schema(conn)

    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version != reference_db._SCHEMA_VERSION:
        # A mismatch means _ensure_schema did not run the ladder to completion,
        # so the dump would be of some intermediate version while claiming to
        # be the target. Refuse rather than emit a mislabelled schema.
        print(
            f"error: generated schema stamped v{version}, reference targets "
            f"v{reference_db._SCHEMA_VERSION}",
            file=sys.stderr,
        )
        return 1

    for kind, filename in (
        ("table", "schema_tables.sql"),
        ("index", "schema_indexes.sql"),
        ("trigger", "schema_triggers.sql"),
    ):
        path = args.out / filename
        path.write_text(_dump(conn, kind), encoding="utf-8")
        print(f"wrote {path}")

    print(f"schema version: {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
