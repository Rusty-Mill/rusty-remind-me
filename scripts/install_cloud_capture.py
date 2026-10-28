#!/usr/bin/env python3
"""Install the cloud session capture hook (ADR-0029) in a user's Claude settings.

A cloud session that checks out several repositories starts above them, in
`/home/user`, so no repository's `.claude/settings.json` is loaded and a hook
committed there never runs. Installing at user level covers every session of
an environment: its setup script runs this once per container. It copies
`remind-me-capture.py` into `~/.claude/hooks/` and registers it on Stop and
PreCompact in `~/.claude/settings.json`, keeping everything else there.
Re-running it updates the copy and repairs the registration without adding it
twice; `--check` reports drift without writing.

HOME is refused untouched when a path the installer would use is a symlink or
not a regular file, or when its settings are not the shape Claude Code reads.
These checks run before any access and again before publishing; they do not
guard against directories changed concurrently. Files are staged beside their
destination with recovery copies of the originals and replaced atomically; if
the second replacement fails, the first is put back.

The hook lives in the rusty_mill repository, not here, so its path is always
passed with --source; the installer never guesses one.

Usage: install_cloud_capture.py [--check] [--home DIR] --source HOOK
"""
from __future__ import annotations

import argparse
import copy
import json
import os
import pathlib
import stat
import sys
import tempfile
from dataclasses import dataclass
from typing import Any

HOOK_NAME = 'remind-me-capture.py'
COMMAND = f'python3 "$HOME"/.claude/hooks/{HOOK_NAME}'
EVENTS = ('Stop', 'PreCompact')
TIMEOUT_SECONDS = 30
HOOK_ENTRY = {'type': 'command', 'command': COMMAND, 'timeout': TIMEOUT_SECONDS}
CANONICAL_GROUP = {'hooks': [HOOK_ENTRY]}
NEW_FILE_MODE = 0o644


class InstallError(Exception):
    """HOME cannot be installed into as it stands."""


@dataclass(frozen=True)
class Change:
    """One file to publish: its new bytes and the mode it keeps or takes."""
    path: pathlib.Path
    data: bytes
    mode: int


def ours(hook: Any) -> bool:
    return isinstance(hook, dict) and hook.get('command') == COMMAND


def validate(settings: dict[str, Any]) -> None:
    """Refuse settings whose hooks Claude Code could not read as written.

    Errors name the location and field only, never a value: handlers can
    carry credentials in a command or header.
    """
    if 'hooks' not in settings:
        return
    hooks = settings['hooks']
    if not isinstance(hooks, dict):
        raise InstallError('settings.json "hooks" is not an object')
    for event in EVENTS:
        groups = hooks.get(event, [])
        if not isinstance(groups, list):
            raise InstallError(f'settings.json hooks.{event} is not a list')
        for i, group in enumerate(groups):
            where = f'settings.json hooks.{event}[{i}]'
            if not isinstance(group, dict) or not isinstance(group.get('hooks'), list):
                raise InstallError(f'{where} has no "hooks" list')
            for j, hook in enumerate(group['hooks']):
                field = handler_fault(hook)
                if field is not None:
                    raise InstallError(f'{where}.hooks[{j}] has a missing or invalid "{field}"')


def is_str(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def is_str_list(value: Any) -> bool:
    return isinstance(value, list) and all(isinstance(v, str) for v in value)


def is_timeout(value: Any) -> bool:
    return not isinstance(value, bool) and isinstance(value, (int, float)) and value > 0


# Hook handler fields, per https://code.claude.com/docs/en/hooks#hook-handler-fields:
# the fields each type requires, then the type each optional field must have.
REQUIRED: dict[str, tuple[str, ...]] = {
    'command': ('command',),
    'http': ('url',),
    'mcp_tool': ('server', 'tool'),
    'prompt': ('prompt',),
    'agent': ('prompt',),
}
OPTIONAL: dict[str, Any] = {
    'timeout': is_timeout,
    'if': is_str,
    'statusMessage': lambda v: isinstance(v, str),
    'once': lambda v: isinstance(v, bool),
    'args': is_str_list,
    'async': lambda v: isinstance(v, bool),
    'asyncRewake': lambda v: isinstance(v, bool),
    'shell': lambda v: v in ('bash', 'powershell'),
    'onFailure': lambda v: v in ('continue', 'block'),
    'headers': lambda v: isinstance(v, dict) and all(isinstance(x, str) for x in v.values()),
    'allowedEnvVars': is_str_list,
    'input': lambda v: isinstance(v, dict),
    'model': is_str,
}


def handler_fault(hook: Any) -> str | None:
    """The first field that makes `hook` an invalid handler, or None if it is valid."""
    if not isinstance(hook, dict) or not isinstance(hook.get('type'), str) or hook['type'] not in REQUIRED:
        return 'type'
    for field in REQUIRED[hook['type']]:
        if not is_str(hook.get(field)):
            return field
    for field, ok in OPTIONAL.items():
        if field in hook and not ok(hook[field]):
            return field
    return None


def registered(groups: list[dict[str, Any]]) -> bool:
    """Exactly one registration of the hook, and it is the canonical group."""
    found = [(group, hook) for group in groups for hook in group['hooks'] if ours(hook)]
    return len(found) == 1 and found[0][0] == CANONICAL_GROUP


def merged(settings: dict[str, Any]) -> dict[str, Any]:
    """`settings` with the hook registered exactly once on every event in EVENTS.

    A registration that differs (another matcher, type or timeout, or a
    duplicate) is removed and replaced; other hooks keep their place.
    """
    validate(settings)
    hooks = settings.setdefault('hooks', {})
    for event in EVENTS:
        groups = hooks.setdefault(event, [])
        if registered(groups):
            continue
        kept = []
        for group in groups:
            others = [hook for hook in group['hooks'] if not ours(hook)]
            if others or not group['hooks']:
                kept.append({**group, 'hooks': others})
        hooks[event] = kept + [copy.deepcopy(CANONICAL_GROUP)]
    return settings


def checked(path: pathlib.Path) -> pathlib.Path:
    """`path`, if it is not a symlink and is a regular file or directory when present."""
    if path.is_symlink():
        raise InstallError(f'{path} is a symlink')
    if path.exists() and not (path.is_file() or path.is_dir()):
        raise InstallError(f'{path} is not a regular file or directory')
    return path


def paths(home: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
    """The hook and settings paths under HOME, every component checked."""
    if not home.is_dir():
        raise InstallError(f'{home} is not a directory')
    claude = checked(home / '.claude')
    hooks_dir = checked(claude / 'hooks')
    hook, settings = checked(hooks_dir / HOOK_NAME), checked(claude / 'settings.json')
    for path in (hook, settings):
        if path.exists() and not path.is_file():
            raise InstallError(f'{path} is not a regular file')
    return hook, settings


def read_settings(path: pathlib.Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        settings = json.loads(path.read_text(encoding='utf-8'))
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        raise InstallError(f'{path} is not valid JSON: {e}') from e
    if not isinstance(settings, dict):
        raise InstallError(f'{path} is not a JSON object')
    return settings


def mode_of(path: pathlib.Path, default: int) -> int:
    return stat.S_IMODE(path.stat().st_mode) if path.exists() else default


def plan(home: pathlib.Path, source_path: pathlib.Path) -> list[Change]:
    """The files HOME needs written, in publication order."""
    hook, settings_path = paths(home)
    if not source_path.is_file():
        raise InstallError(f'{source_path} is not a file')
    source = source_path.read_bytes()
    current = read_settings(settings_path)
    wanted = merged(copy.deepcopy(current))
    changes = []
    if not hook.exists() or hook.read_bytes() != source:
        changes.append(Change(hook, source, mode_of(hook, mode_of(source_path, NEW_FILE_MODE))))
    if wanted != current:
        text = json.dumps(wanted, indent=2) + '\n'
        changes.append(Change(settings_path, text.encode('utf-8'), mode_of(settings_path, NEW_FILE_MODE)))
    return changes


def stage(change: Change) -> pathlib.Path:
    """Write `change` to a temporary file beside its destination."""
    fd, name = tempfile.mkstemp(dir=change.path.parent, prefix=f'.{change.path.name}.')
    staged = pathlib.Path(name)
    try:
        with os.fdopen(fd, 'wb') as f:
            f.write(change.data)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(staged, change.mode)
    except BaseException:
        staged.unlink(missing_ok=True)
        raise
    return staged


def publish(home: pathlib.Path, changes: list[Change]) -> None:
    """Replace every file atomically; if one fails, put back those already replaced.

    A recovery copy of each file being replaced is written beside it before
    anything is replaced, so rolling back needs no new write. If a rollback
    step still fails, the error names the recovery copy left in place.
    """
    for change in changes:
        change.path.parent.mkdir(parents=True, exist_ok=True)
    paths(home)  # nothing became a symlink while directories were made
    backups: dict[pathlib.Path, pathlib.Path | None] = {}
    staged: dict[pathlib.Path, pathlib.Path] = {}
    try:
        for change in changes:
            original = change.path.read_bytes() if change.path.exists() else None
            backups[change.path] = None if original is None else stage(
                Change(change.path, original, change.mode))
            staged[change.path] = stage(change)
    except BaseException:
        discard([*staged.values(), *filter(None, backups.values())])
        raise
    done: list[pathlib.Path] = []
    try:
        for change in changes:
            os.replace(staged[change.path], change.path)
            del staged[change.path]
            done.append(change.path)
    except BaseException as failure:
        stranded = rollback(done, backups)
        discard([*staged.values(), *(b for p, b in backups.items() if b and p not in done)])
        if stranded:
            raise InstallError(f'{failure}; rollback incomplete: {"; ".join(stranded)}') from failure
        raise
    discard(list(filter(None, backups.values())))


def rollback(done: list[pathlib.Path], backups: dict[pathlib.Path, pathlib.Path | None]) -> list[str]:
    """Put back every replaced file, newest first; describe each step that failed."""
    stranded = []
    for path in reversed(done):
        backup = backups[path]
        try:
            if backup is None:
                path.unlink(missing_ok=True)
            else:
                os.replace(backup, path)
        except OSError as e:
            stranded.append(f'{path} could not be removed ({e})' if backup is None
                            else f'{path} not restored ({e}); its original is kept at {backup}')
    return stranded


def discard(temporaries: list[pathlib.Path]) -> None:
    """Remove staged or recovery files that are no longer needed, warning on any left behind."""
    for path in temporaries:
        try:
            path.unlink(missing_ok=True)
        except OSError as e:
            print(f'warning: could not remove {path}: {e}', file=sys.stderr)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--home', type=pathlib.Path, default=pathlib.Path.home(),
                        help='home directory whose .claude/ to install into (default: $HOME)')
    parser.add_argument('--source', type=pathlib.Path, required=True,
                        help='the hook to install: rusty_mill\'s .claude/hooks/remind-me-capture.py')
    parser.add_argument('--check', action='store_true', help='report drift, write nothing')
    args = parser.parse_args(argv)
    home = args.home.resolve()
    try:
        changes = plan(home, args.source)
        if not changes:
            print(f'{home}: capture hook is current')
            return 0
        if not args.check:
            publish(home, changes)
    except (InstallError, OSError) as e:
        print(f'error: {e}', file=sys.stderr)
        return 2
    for change in changes:
        print(f'{"stale" if args.check else "wrote"}: {change.path.relative_to(home)}')
    if args.check:
        return 1
    print('Capture also needs REMIND_ME_CLOUD_CAPTURE, REMIND_ME_URL and REMIND_ME_TOKEN.')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
