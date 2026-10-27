#!/usr/bin/env python3
"""Install the cloud session capture hook (ADR-0029) into another repository.

Cloud Claude Code sessions load hooks only from the checked-out repository's
`.claude/settings.json`, so each repository whose sessions should reach
remind-me needs its own copy. This copies `.claude/hooks/remind-me-capture.py`
from this monorepo into TARGET and registers it on Stop and PreCompact,
keeping everything else in TARGET's settings. Re-running it updates the copy
and adds nothing twice; `--check` reports drift without writing.

Usage: install_cloud_capture.py [--check] TARGET
"""
from __future__ import annotations

import argparse
import copy
import json
import pathlib
import sys
from typing import Any

HOOK_NAME = 'remind-me-capture.py'
SOURCE = pathlib.Path(__file__).resolve().parents[4] / '.claude' / 'hooks' / HOOK_NAME
COMMAND = f'python3 "$CLAUDE_PROJECT_DIR"/.claude/hooks/{HOOK_NAME}'
EVENTS = ('Stop', 'PreCompact')
TIMEOUT_SECONDS = 30


class InstallError(Exception):
    """TARGET cannot be installed into as it stands."""


def registered(entries: list[Any]) -> bool:
    """Whether any matcher group in one event's list already runs the hook."""
    return any(
        isinstance(hook, dict) and hook.get('command') == COMMAND
        for group in entries if isinstance(group, dict)
        for hook in group.get('hooks', [])
    )


def merged(settings: dict[str, Any]) -> dict[str, Any]:
    """`settings` with the hook on every event in EVENTS, other keys untouched."""
    hooks = settings.setdefault('hooks', {})
    if not isinstance(hooks, dict):
        raise InstallError('settings.json "hooks" is not an object')
    for event in EVENTS:
        entries = hooks.setdefault(event, [])
        if not isinstance(entries, list):
            raise InstallError(f'settings.json hooks.{event} is not a list')
        if not registered(entries):
            entries.append({'hooks': [
                {'type': 'command', 'command': COMMAND, 'timeout': TIMEOUT_SECONDS},
            ]})
    return settings


def read_settings(path: pathlib.Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        settings = json.loads(path.read_text(encoding='utf-8'))
    except json.JSONDecodeError as e:
        raise InstallError(f'{path} is not valid JSON: {e}') from e
    if not isinstance(settings, dict):
        raise InstallError(f'{path} is not a JSON object')
    return settings


def plan(target: pathlib.Path) -> dict[pathlib.Path, str]:
    """The files TARGET needs written, by path, with their new contents."""
    if not (target / '.git').exists():
        raise InstallError(f'{target} is not a git repository root')
    hook = target / '.claude' / 'hooks' / HOOK_NAME
    settings_path = target / '.claude' / 'settings.json'
    source = SOURCE.read_text(encoding='utf-8')
    current = read_settings(settings_path)
    wanted = merged(copy.deepcopy(current))
    changes: dict[pathlib.Path, str] = {}
    if not hook.exists() or hook.read_text(encoding='utf-8') != source:
        changes[hook] = source
    if wanted != current:
        changes[settings_path] = json.dumps(wanted, indent=2) + '\n'
    return changes


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('target', type=pathlib.Path, help='root of the repository to install into')
    parser.add_argument('--check', action='store_true', help='report drift, write nothing')
    args = parser.parse_args(argv)
    target = args.target.resolve()
    try:
        changes = plan(target)
    except InstallError as e:
        print(f'error: {e}', file=sys.stderr)
        return 2
    if not changes:
        print(f'{target}: capture hook is current')
        return 0
    for path in changes:
        print(f'{"stale" if args.check else "wrote"}: {path.relative_to(target)}')
    if args.check:
        return 1
    for path, text in changes.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')
    print('Commit .claude/ in that repository; capture still needs the environment settings.')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
