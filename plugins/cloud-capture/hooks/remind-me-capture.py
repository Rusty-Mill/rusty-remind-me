#!/usr/bin/env python3
"""Stop / PreCompact hook: capture a cloud session's transcript in remind-me (ADR-0029, ADR-0036).

A cloud session (claude.ai Projects, Claude Duet) runs in a fresh container with
no rusty-remind-me binary and a transcript that dies with the container. This
sends the transcript to the remote remind-me server's MCP endpoint
(`remind_me_capture_transcript`), which keeps one capture per session, replaced
in place on every call.

Where the URL and token come from, first match wins:
  config.json            beside this plugin's .claude-plugin/ folder:
                         {"url": "https://…/mcp", "token": "<capture-only token>"}.
                         Never committed: the plugin is filled in and uploaded
                         privately (see the plugin's README).
  REMIND_ME_URL, REMIND_ME_TOKEN with REMIND_ME_CLOUD_CAPTURE=1
                         the environment variables an environment setup sets.
Either way it runs only when CLAUDE_CODE_REMOTE=true (set by Claude Code in a
cloud session), so a local CLI session stays with the main plugin's hook.

The token should be the server's capture-only token (REMIND_ME_REMOTE_CAPTURE_TOKEN):
it can open a session and call remind_me_capture_transcript, nothing else, and
the server marks what it writes with the client name remind-me-capture-token.

What leaves the container is scrubbed first: thinking blocks are dropped, the
values of fields named like secrets are withheld (JSON carried in a string is
scrubbed field by field), and every string is passed through the secret shapes
the server redacts (ADR-0026), plus quoted key-value pairs, bearer values and
the token itself. The server redacts again
at write. The endpoint must be https (http only for loopback), redirects are
never followed, and responses are read up to a cap.

Stop is skipped when re-entrant and throttled to once a minute after a
successful capture; PreCompact always runs. Two overlapping runs may both
send; the server replaces the capture in place, so the later one wins.
SessionEnd is not used, because a reclaimed container may never fire it. The
whole run shares one deadline inside the hook's 30 s timeout; a socket timeout
bounds each read, so a slow trickle can run to that 30 s hard stop. Failures are
reported on stderr as a fixed category, never server text or header values,
and the hook exits 0.
"""
import http.client
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

# remind_me_core::transcript::MAX_TRANSCRIPT_BYTES, counted once JSON-escaped.
MAX_TRANSCRIPT_BYTES = 3 * 1024 * 1024
MAX_RESPONSE_BYTES = 1024 * 1024
THROTTLE_SECONDS = 60
BUDGET_SECONDS = 25
GIT_SECONDS = 2
PROTOCOL_VERSION = '2025-06-18'
# Cloudflare's browser integrity check refuses urllib's default agent (error 1010).
USER_AGENT = 'remind-me-capture/1'
# Versions whose initialize, tools/call and Streamable HTTP framing this client speaks.
SUPPORTED_VERSIONS = {PROTOCOL_VERSION, '2025-03-26'}
OPT_IN = 'REMIND_ME_CLOUD_CAPTURE'
# The plugin's own config.json: hooks/ sits one level below the plugin root.
CONFIG = pathlib.Path(__file__).resolve().parents[1] / 'config.json'
LOOPBACK = {'127.0.0.1', '::1', 'localhost'}
THINKING = {'thinking', 'redacted_thinking'}


class CaptureError(Exception):
    """A failure named by a fixed category: never a server's text or a header value."""


class Deadline:
    def __init__(self, seconds, clock=time.monotonic):
        self.clock, self.end = clock, clock() + seconds

    def remaining(self):
        left = self.end - self.clock()
        if left <= 0:
            raise CaptureError('timeout')
        return left


def settings(env, config=CONFIG):
    """(url, token) to capture with, or None when capture is off here."""
    if env.get('CLAUDE_CODE_REMOTE') != 'true':
        return None
    from_file = read_config(config)
    if from_file:
        return from_file
    if env.get(OPT_IN) == '1' and env.get('REMIND_ME_URL') and env.get('REMIND_ME_TOKEN'):
        return env['REMIND_ME_URL'], env['REMIND_ME_TOKEN']
    return None


def read_config(path):
    """(url, token) from `path`, None when there is no such file. A file that is
    there but unreadable or incomplete is an error, never a silent fallback."""
    try:
        text = pathlib.Path(path).read_text(encoding='utf-8')
    except FileNotFoundError:
        return None
    except OSError:
        raise CaptureError('unreadable config.json') from None
    try:
        data = json.loads(text)
    except ValueError:
        raise CaptureError('invalid config.json') from None
    url = data.get('url') if isinstance(data, dict) else None
    token = data.get('token') if isinstance(data, dict) else None
    if not (isinstance(url, str) and url and isinstance(token, str) and token):
        raise CaptureError('invalid config.json')
    return url, token


def endpoint(url):
    """`url` if it is a credential-free https URL (http for loopback only)."""
    try:
        parts = urllib.parse.urlsplit(url)
        host = parts.hostname
    except ValueError:
        raise CaptureError('invalid REMIND_ME_URL') from None
    secure = parts.scheme == 'https' or (parts.scheme == 'http' and host in LOOPBACK)
    if (not secure or not host or '@' in parts.netloc or parts.fragment
            or re.search(r'[\x00-\x20\x7f]', url)):
        raise CaptureError('invalid REMIND_ME_URL')
    return url


def bearer(token):
    """`token` if it can be sent as a header without being echoed in an error."""
    if not re.fullmatch(r'[\x21-\x7e]+', token):
        raise CaptureError('invalid REMIND_ME_TOKEN')
    return token


SECRET_WORDS = (r'api_key|api-key|apikey|secret|token|password|passwd|authorization'
                r'|credential|access_key|private_key|cookie')
SECRET_KEY = re.compile(r'(?i)(?:' + SECRET_WORDS + r')')
# Usage counts named like secrets: `input_tokens`, `cache_read_input_tokens`, `max_tokens`.
USAGE_COUNTER = re.compile(r'[a-z0-9_]+_tokens')

# The shapes remind_me_core::redact scrubs, as (kind, pattern, group kept before
# the secret). Bearer values are added here: a transcript can quote a header.
RULES = [
    ('private_key', re.compile(r'-----BEGIN [A-Z ]*PRIVATE KEY-----.*?'
                               r'(?:-----END [A-Z ]*PRIVATE KEY-----|\Z)', re.S), None),
    ('jwt', re.compile(r'(?<![A-Za-z0-9])eyJ[A-Za-z0-9_-]{7,}\.eyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]*'), None),
    ('url_credentials', re.compile(r'(://[^\s/@:]*:)(?!\[REDACTED:)[^\s/@]+(?=@)'), 1),
    ('aws_access_key', re.compile(r'(?<![A-Za-z0-9])AKIA[0-9A-Z]{16}(?![A-Za-z0-9])'), None),
    ('github_token', re.compile(r'(?<![A-Za-z0-9])gh[pousr]_[A-Za-z0-9]{36,}'), None),
    ('slack_token', re.compile(r'(?<![A-Za-z0-9])xox[baprs]-[A-Za-z0-9_-]{10,}'), None),
    ('bearer', re.compile(r'(?i)(\bbearer[ \t]+)(?!\[REDACTED:)[A-Za-z0-9._~+/=-]{8,}'), 1),
    # `key: "value"`, `"key": "value"` and their escaped forms inside serialized
    # JSON: the whole quoted value, however short.
    ('secret', re.compile(r'(?i)((?:' + SECRET_WORDS + r')[\w-]*\\?["\']?[ \t]*[:=][ \t]*\\?(["\']))'
                          r'(?!\[REDACTED:)(?:(?!\\?\2).)+'), 1),
    # `key=value`, `key: value`: an unquoted value of eight or more characters.
    ('secret', re.compile(r'(?i)((?:' + SECRET_WORDS + r')[\w-]*["\']?[ \t]*[:=][ \t]*)'
                          r'(?![\\"\']|\[REDACTED:)\S{8,}'), 1),
]


def redact(text, token=None):
    """`text` with every secret shape, and `token` itself, replaced by `[REDACTED:<kind>]`."""
    if token:
        text = text.replace(token, '[REDACTED:token]')
    for kind, pattern, keep in RULES:
        mark = f'[REDACTED:{kind}]'
        text = pattern.sub((lambda m, mark=mark, keep=keep: m.group(keep) + mark) if keep else mark, text)
    return text


def scrub(value, token=None):
    """`value` with thinking blocks dropped, secret-named fields blanked and every string redacted."""
    if isinstance(value, str):
        return scrub_text(value, token)
    if isinstance(value, list):
        return [scrub(v, token) for v in value
                if not (isinstance(v, dict) and v.get('type') in THINKING)]
    if isinstance(value, dict):
        return {redact(k, token): secret_field(k, v) if SECRET_KEY.search(k) else scrub(v, token)
                for k, v in value.items()}
    return value


def secret_field(key, value):
    """The value of a field named like a secret, withheld whatever its type. Only an
    integer under a `*_tokens` usage counter, a boolean, null or an empty value stays."""
    if value is None or isinstance(value, bool) or value in ('', [], {}):
        return value
    if USAGE_COUNTER.fullmatch(key) and isinstance(value, int):
        return value
    return '[REDACTED:secret]'


def scrub_text(text, token=None):
    """A string, read as JSON first when it is one, so a serialized tool result is
    scrubbed field by field."""
    if text.lstrip()[:1] in ('{', '['):
        try:
            value = json.loads(text)
        except ValueError:
            pass
        else:
            clean = scrub(value, token)
            if clean != value:
                return json.dumps(clean, ensure_ascii=False)
    return redact(text, token)


def scrub_line(line, token=None):
    """One JSONL line, scrubbed; a clean line is returned byte for byte."""
    try:
        value = json.loads(line)
    except ValueError:
        return redact(line, token)
    clean = scrub(value, token)
    return line if clean == value else json.dumps(clean, ensure_ascii=False, separators=(',', ':'))


def escaped_len(text):
    """Bytes `text` costs in the request body, as transcript::encoded_len counts them."""
    return len(json.dumps(text, ensure_ascii=False).encode()) - 2


def read_tail(path, limit=MAX_TRANSCRIPT_BYTES):
    """The whole lines in the last `limit` bytes of `path`, oldest first.

    Escaping never shrinks a line, so no line outside them could fit.
    """
    with open(path, 'rb') as f:
        start = max(0, f.seek(0, os.SEEK_END) - limit)
        f.seek(start)
        lines = f.read(limit).decode('utf-8', 'replace').split('\n')
    return lines[1:] if start else lines


def compose(lines, limit=MAX_TRANSCRIPT_BYTES, token=None):
    """The newest scrubbed lines whose escaped size fits in `limit`, joined as JSONL."""
    kept, used = [], 0
    for line in reversed(lines):
        if not line.strip():
            continue
        line = scrub_line(line, token) + '\n'
        used += escaped_len(line)
        if used > limit:
            break
        kept.append(line)
    return ''.join(reversed(kept))


def stamp_path(session_id, state_dir):
    return pathlib.Path(state_dir) / (re.sub(r'[^A-Za-z0-9_-]', '_', session_id) + '.last')


def throttled(session_id, now, state_dir):
    """True when this session was captured on Stop less than a minute ago."""
    try:
        return now - float(stamp_path(session_id, state_dir).read_text()) < THROTTLE_SECONDS
    except (OSError, ValueError):
        return False


def stamp(session_id, now, state_dir):
    path = stamp_path(session_id, state_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(str(now))


def git(cwd, deadline, *args):
    try:
        r = subprocess.run(['git', '-C', cwd, *args], capture_output=True, text=True,
                           timeout=min(GIT_SECONDS, deadline.remaining()))
    except (OSError, subprocess.TimeoutExpired):
        return None
    if r.returncode:
        return None
    return r.stdout.strip() or None


def bare_remote(url):
    """A remote URL with credentials, query and fragment removed: a proxy remote can carry a token."""
    if not url:
        return None
    parts = urllib.parse.urlsplit(url)
    if not parts.netloc:
        return url
    host = parts.netloc.rsplit('@', 1)[-1]
    return urllib.parse.urlunsplit(parts._replace(netloc=host, query='', fragment=''))


def context(cwd, deadline, token=None):
    top = git(cwd, deadline, 'rev-parse', '--show-toplevel')
    fields = {
        'project': os.path.basename(top or cwd) or None,
        'git_branch': git(cwd, deadline, 'rev-parse', '--abbrev-ref', 'HEAD'),
        'git_sha': git(cwd, deadline, 'rev-parse', 'HEAD'),
        'git_remote': bare_remote(git(cwd, deadline, 'remote', 'get-url', 'origin')),
        'cwd': cwd,
    }
    return {k: redact(v, token) for k, v in fields.items() if v}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never follow a redirect: it could carry the token to another origin or over http."""

    def redirect_request(self, *args, **kwargs):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


class Mcp:
    """Just enough Streamable HTTP MCP client to make one tool call."""

    def __init__(self, url, token, deadline):
        self.url, self.token, self.deadline = endpoint(url), bearer(token), deadline
        self.session, self.next_id, self.version = None, 1, PROTOCOL_VERSION

    def post(self, message):
        headers = {'Content-Type': 'application/json',
                   'Accept': 'application/json, text/event-stream',
                   'Authorization': f'Bearer {self.token}',
                   'MCP-Protocol-Version': self.version,
                   'User-Agent': USER_AGENT}
        if self.session:
            headers['Mcp-Session-Id'] = self.session
        body = json.dumps(message, ensure_ascii=False).encode()
        request = urllib.request.Request(self.url, body, headers, method='POST')
        try:
            with OPENER.open(request, timeout=self.deadline.remaining()) as response:
                session = response.headers.get('Mcp-Session-Id')
                kind = response.headers.get('Content-Type', '')
                raw = response.read(MAX_RESPONSE_BYTES + 1)
        except urllib.error.HTTPError as e:
            e.close()
            raise CaptureError(f'http {int(e.code)}') from None
        except TimeoutError:
            raise CaptureError('timeout') from None
        except (urllib.error.URLError, http.client.HTTPException, OSError):
            raise CaptureError('network') from None
        except ValueError:
            raise CaptureError('invalid request') from None
        if len(raw) > MAX_RESPONSE_BYTES:
            raise CaptureError('response too large')
        self.deadline.remaining()  # a socket timeout bounds each read, not the whole response
        if session and re.fullmatch(r'[\x21-\x7e]+', session):
            self.session = session
        if 'id' not in message:
            return None
        return reply_to(message['id'], raw.decode('utf-8', 'replace'), 'text/event-stream' in kind)

    def request(self, method, params):
        message = {'jsonrpc': '2.0', 'id': self.next_id, 'method': method, 'params': params}
        self.next_id += 1
        reply = self.post(message)
        if reply is None:
            raise CaptureError(f'{method}: no reply')
        if 'error' in reply or not isinstance(reply.get('result'), dict):
            raise CaptureError(f'{method}: error')
        return reply['result']

    def call(self, tool, arguments):
        init = self.request('initialize', {'protocolVersion': PROTOCOL_VERSION, 'capabilities': {},
                                           'clientInfo': {'name': 'remind-me-capture-hook', 'version': '2'}})
        version = init.get('protocolVersion', PROTOCOL_VERSION)
        if version not in SUPPORTED_VERSIONS:
            raise CaptureError('unsupported protocol version')
        self.version = version
        self.post({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
        result = self.request('tools/call', {'name': tool, 'arguments': arguments})
        if result.get('isError'):
            raise CaptureError('tool error')
        return result


def sse_events(body):
    """The data of each complete event in an event stream; a trailing partial one is dropped."""
    events, data = [], []
    for line in body.splitlines():
        if not line:
            if data:
                events.append('\n'.join(data))
            data = []
        elif line.startswith('data:'):
            value = line[5:]
            data.append(value[1:] if value.startswith(' ') else value)
    return events


def reply_to(message_id, body, sse):
    """The JSON-RPC reply to `message_id` in a plain or event-stream body."""
    for chunk in sse_events(body) if sse else [body]:
        try:
            reply = json.loads(chunk)
        except ValueError:
            continue
        if isinstance(reply, dict) and reply.get('id') == message_id:
            return reply
    return None


def run(payload, env, now, state_dir, deadline=None, config=CONFIG):
    """Capture the session; returns what happened, for tests. Raises CaptureError."""
    target = settings(env, config)
    if not target:
        return 'off'
    url, token = target
    deadline = deadline or Deadline(BUDGET_SECONDS)
    event = payload.get('hook_event_name', '')
    session_id = payload.get('session_id') or ''
    path = payload.get('transcript_path') or ''
    if not session_id or not os.path.isfile(path):
        return 'nothing to capture'
    if event == 'Stop':
        if payload.get('stop_hook_active'):
            return 're-entrant'
        if throttled(session_id, now, state_dir):
            return 'throttled'
    mcp = Mcp(url, token, deadline)
    transcript = compose(read_tail(path), token=token)
    if not transcript.strip():
        return 'nothing to capture'
    arguments = {'session_id': session_id, 'transcript': transcript,
                 'reason': 'precompact' if event == 'PreCompact' else 'stop',
                 **context(payload.get('cwd') or os.getcwd(), deadline, token)}
    mcp.call('remind_me_capture_transcript', arguments)
    if event == 'Stop':
        stamp(session_id, now, state_dir)
    return 'captured'


def main():
    try:
        payload = json.load(sys.stdin)
        state_dir = os.path.join(tempfile.gettempdir(), 'remind-me-capture')
        run(payload, os.environ, time.time(), state_dir)
    except CaptureError as e:
        print(f'remind-me-capture: {e}', file=sys.stderr)
    except Exception as e:  # a hook must never fail the session, nor echo what failed
        print(f'remind-me-capture: unexpected {type(e).__name__}', file=sys.stderr)
    return 0


if __name__ == '__main__':
    sys.exit(main())
