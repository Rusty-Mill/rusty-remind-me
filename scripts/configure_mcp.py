#!/usr/bin/env python3
"""
configure_mcp.py — Configures Claude Desktop, Antigravity, Cursor, Codex, and custom LLM agents
to use rusty_remind_me as their long-term memory backend.
"""

import json
import os
import subprocess
import sys
from pathlib import Path


def find_workspace_root(start: Path) -> Path:
    """The Cargo workspace root at or above `start`: where `Cargo.lock` and
    `target/` live. Since the move into the rusty_mill monorepo that is the
    monorepo root, not this product's own directory."""
    for candidate in (start, *start.parents):
        if (candidate / "Cargo.lock").exists():
            return candidate
    sys.exit(f"Error: no Cargo.lock found at or above {start}")


def find_executable() -> str:
    product_root = Path(__file__).resolve().parent.parent
    root = find_workspace_root(product_root)
    exe_name = "rusty-remind-me.exe" if os.name == "nt" else "rusty-remind-me"
    release_exe = root / "target" / "release" / exe_name
    debug_exe = root / "target" / "debug" / exe_name

    if release_exe.exists():
        return str(release_exe)
    if debug_exe.exists():
        return str(debug_exe)

    print("Building rusty-remind-me binary...")
    # `-p` rather than a bare build: at the monorepo root a bare build would
    # compile every Rusty Mill crate, not just this one.
    subprocess.run(["cargo", "build", "--release", "-p", "rusty-remind-me"], cwd=root, check=False)
    if release_exe.exists():
        return str(release_exe)

    sys.exit(f"Error: Could not locate compiled binary at {release_exe}")


def update_mcp_config(config_path: Path, exe_path: str, db_path: str, target_name: str) -> None:
    config_path.parent.mkdir(parents=True, exist_ok=True)

    data = {"mcpServers": {}}
    if config_path.exists():
        try:
            with open(config_path, "r", encoding="utf-8") as f:
                content = f.read().strip()
                if content:
                    parsed = json.loads(content)
                    if isinstance(parsed, dict):
                        data = parsed
                        if "mcpServers" not in data:
                            data["mcpServers"] = {}
        except Exception as e:
            print(f"Warning: Could not parse {config_path} ({e}). Creating backup...")
            backup_path = config_path.with_suffix(".json.bak")
            config_path.rename(backup_path)
            data = {"mcpServers": {}}

    server_entry = {
        "command": exe_path,
        "args": ["server"],
        "env": {
            "REMIND_ME_DB_PATH": db_path
        }
    }

    if "mcpServers" not in data:
        data["mcpServers"] = {}

    data["mcpServers"]["rusty-remind-me"] = server_entry

    with open(config_path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)

    print(f"✔ Configured {target_name}: {config_path}")


def main():
    home = Path.home()
    appdata = Path(os.environ.get("APPDATA", home / "AppData" / "Roaming")) if os.name == "nt" else home / ".config"
    db_path = str(home / ".remind_me" / "remind_me.db")

    exe_path = find_executable()
    print(f"Using executable: {exe_path}")
    print(f"Using database:   {db_path}")

    # Targets
    targets = [
        (appdata / "Claude" / "claude_desktop_config.json", "Claude Desktop"),
        (home / ".gemini" / "antigravity" / "mcp_config.json", "Antigravity"),
        (home / ".cursor" / "mcp.json", "Cursor"),
        (home / ".mcp" / "config.json", "Codex / Generic MCP Client"),
    ]

    for path, name in targets:
        update_mcp_config(path, exe_path, db_path, name)

    print("\n🎉 Setup complete! Restart your client application to activate rusty-remind-me.")


if __name__ == "__main__":
    main()
