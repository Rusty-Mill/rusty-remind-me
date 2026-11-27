"""remind-me over MCP for the Hermes memory provider (ADR-0028, card #1552).

Hermes-free on purpose: everything here runs and is tested without Hermes
installed. `__init__.py` adapts it to Hermes's `MemoryProvider`.

One contract: every call is an MCP tool call, over stdio (`rusty-remind-me
server`, when Hermes runs beside the store) or Streamable HTTP (the remote
connector). Which one is a single setting: a `url` means remote.
"""
from __future__ import annotations

import json
import os
import queue
import re
import shutil
import subprocess
import threading
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Protocol, Sequence

PROTOCOL_VERSION = "2025-06-18"
CLIENT_VERSION = "1"
SETTINGS_FILE = "remind-me.json"
CLIENT_PREFIX = "hermes-"
CLIENT_ID_MAX = 40
DEFAULT_COMMAND = "rusty-remind-me"
REQUEST_SECONDS = 30.0
MAX_RESPONSE_BYTES = 4 * 1024 * 1024
LOOPBACK = frozenset({"127.0.0.1", "::1", "localhost"})
USER_AGENT = "remind-me-hermes/1"
# transcript::MAX_TRANSCRIPT_BYTES: what remind_me_capture_transcript accepts,
# counted JSON-escaped.
MAX_TRANSCRIPT_BYTES = 3 * 1024 * 1024
CAPTURE_TOOL = "remind_me_capture_transcript"


class RemindMeError(Exception):
    """A failed call, named by a short category; never carries the token."""


@dataclass(frozen=True)
class Settings:
    """Where remind-me is and how to reach it."""

    command: str = DEFAULT_COMMAND
    url: str | None = None
    token: str | None = field(default=None, repr=False)
    project: str | None = None
    # ADR-0036's capture-only credential: remote captures go through
    # `remind_me_capture_transcript`, one per session (D134).
    capture_token: str | None = field(default=None, repr=False)

    @property
    def remote(self) -> bool:
        return self.url is not None

    def problem(self) -> str | None:
        """Why these settings cannot work, or None. Never touches the network."""
        if self.remote:
            if not self.token:
                return "url is set but no token (REMIND_ME_TOKEN)"
            try:
                endpoint(self.url or "")
            except RemindMeError as e:
                return str(e)
            return None
        if shutil.which(self.command) is None:
            return f"{self.command!r} is not on PATH"
        return None


def load_settings(hermes_home: str | None, env: Mapping[str, str]) -> Settings:
    """Settings from `$HERMES_HOME/remind-me.json`, then `REMIND_ME_URL` and
    `REMIND_ME_TOKEN` and `REMIND_ME_CAPTURE_TOKEN` (environment only). A
    file that exists but does not parse is an error."""
    stored: dict[str, Any] = {}
    if hermes_home:
        path = Path(hermes_home) / SETTINGS_FILE
        if path.exists():
            try:
                loaded = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError) as e:
                raise RemindMeError(f"invalid {SETTINGS_FILE}: {type(e).__name__}") from None
            if not isinstance(loaded, dict):
                raise RemindMeError(f"invalid {SETTINGS_FILE}: not an object")
            stored = loaded
    return Settings(
        command=_text(stored.get("command")) or DEFAULT_COMMAND,
        url=_text(env.get("REMIND_ME_URL")) or _text(stored.get("url")),
        token=_text(env.get("REMIND_ME_TOKEN")),
        project=_text(stored.get("project")),
        capture_token=_text(env.get("REMIND_ME_CAPTURE_TOKEN")),
    )


def _text(value: object) -> str | None:
    return value.strip() or None if isinstance(value, str) else None


def client_id(agent_identity: str | None) -> str:
    """`hermes-<profile>`: the registry id this bot writes as (ADR-0028 §1)."""
    profile = re.sub(r"[^a-z0-9-]+", "-", (agent_identity or "default").lower()).strip("-")
    return (CLIENT_PREFIX + (profile or "default"))[:CLIENT_ID_MAX].rstrip("-")


def endpoint(url: str) -> str:
    """`url` if it is credential-free https (http only for loopback)."""
    parts = urllib.parse.urlsplit(url)
    secure = parts.scheme == "https" or (parts.scheme == "http" and parts.hostname in LOOPBACK)
    if not secure or not parts.hostname or "@" in parts.netloc or parts.fragment:
        raise RemindMeError("url must be https (http only for localhost), without credentials")
    return url


# -- transports --------------------------------------------------------------


class Transport(Protocol):
    def send(self, message: dict[str, Any]) -> dict[str, Any] | None:
        """Send one JSON-RPC message; the reply to it, or None for a notification."""

    def close(self) -> None: ...


class StdioTransport:
    """`rusty-remind-me server` as a child process, one JSON-RPC line each way."""

    def __init__(self, command: str, client: str) -> None:
        env = dict(os.environ, REMIND_ME_CLIENT=client)
        self._proc = subprocess.Popen(
            [command, "server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
            encoding="utf-8",
        )
        self._lines: queue.Queue[str | None] = queue.Queue()
        self._lock = threading.Lock()
        threading.Thread(target=self._read, name="remind-me-stdio", daemon=True).start()

    def _read(self) -> None:
        assert self._proc.stdout is not None
        for line in self._proc.stdout:
            self._lines.put(line)
        self._lines.put(None)

    def send(self, message: dict[str, Any]) -> dict[str, Any] | None:
        with self._lock:
            if self._proc.stdin is None or self._proc.poll() is not None:
                raise RemindMeError("server exited")
            self._proc.stdin.write(json.dumps(message) + "\n")
            self._proc.stdin.flush()
            if "id" not in message:
                return None
            while True:
                try:
                    line = self._lines.get(timeout=REQUEST_SECONDS)
                except queue.Empty:
                    raise RemindMeError("timeout") from None
                if line is None:
                    raise RemindMeError("server exited")
                reply = _json_object(line)
                if reply is not None and reply.get("id") == message["id"]:
                    return reply

    def close(self) -> None:
        if self._proc.stdin is not None:
            self._proc.stdin.close()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait()
        if self._proc.stdout is not None:
            self._proc.stdout.close()


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """A redirect could carry the token elsewhere: never follow one."""

    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None


class HttpTransport:
    """Just enough Streamable HTTP for request/reply tool calls."""

    def __init__(self, url: str, token: str) -> None:
        self._url, self._token = endpoint(url), token
        self._session: str | None = None
        self._version = PROTOCOL_VERSION
        self._opener = urllib.request.build_opener(_NoRedirect)

    def send(self, message: dict[str, Any]) -> dict[str, Any] | None:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "Authorization": f"Bearer {self._token}",
            "MCP-Protocol-Version": self._version,
            "User-Agent": USER_AGENT,
        }
        if self._session:
            headers["Mcp-Session-Id"] = self._session
        # UTF-8, not \u escapes: sent as transcript_jsonl sized it, so a
        # transcript within MAX_TRANSCRIPT_BYTES fits the server's body limit.
        body = wire_bytes(message)
        request = urllib.request.Request(self._url, body, headers, method="POST")
        try:
            with self._opener.open(request, timeout=REQUEST_SECONDS) as response:
                session = response.headers.get("Mcp-Session-Id")
                sse = "text/event-stream" in response.headers.get("Content-Type", "")
                raw = response.read(MAX_RESPONSE_BYTES + 1)
        except urllib.error.HTTPError as e:
            e.close()
            raise RemindMeError(f"http {e.code}") from None
        except (urllib.error.URLError, OSError):
            raise RemindMeError("network") from None
        if len(raw) > MAX_RESPONSE_BYTES:
            raise RemindMeError("response too large")
        if session and re.fullmatch(r"[\x21-\x7e]+", session):
            self._session = session
        if "id" not in message:
            return None
        return reply_to(message["id"], raw.decode("utf-8", "replace"), sse)

    def close(self) -> None:
        self._session = None


def sse_events(body: str) -> list[str]:
    """The data of each complete event in an event stream."""
    events: list[str] = []
    data: list[str] = []
    for line in body.splitlines():
        if not line:
            if data:
                events.append("\n".join(data))
            data = []
        elif line.startswith("data:"):
            value = line[5:]
            data.append(value[1:] if value.startswith(" ") else value)
    if data:
        events.append("\n".join(data))
    return events


def reply_to(message_id: int, body: str, sse: bool) -> dict[str, Any] | None:
    """The JSON-RPC reply to `message_id` in a plain or event-stream body."""
    for chunk in sse_events(body) if sse else [body]:
        reply = _json_object(chunk)
        if reply is not None and reply.get("id") == message_id:
            return reply
    return None


def _json_object(text: str) -> dict[str, Any] | None:
    try:
        value = json.loads(text)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


# -- the client --------------------------------------------------------------


class McpClient:
    """remind-me's tools over one transport, initialised on first use."""

    def __init__(self, transport: Transport, client: str) -> None:
        self._transport, self._client = transport, client
        self._next_id = 1
        self._ready = False
        self._lock = threading.Lock()

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            message = {"jsonrpc": "2.0", "id": self._next_id, "method": method, "params": params}
            self._next_id += 1
        reply = self._transport.send(message)
        if reply is None or "error" in reply or not isinstance(reply.get("result"), dict):
            raise RemindMeError(f"{method} failed")
        return reply["result"]

    def _initialize(self) -> None:
        if self._ready:
            return
        # The client id rides the handshake too: over remote it is the only
        # name the server sees, matched as a registry alias.
        self._request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": self._client, "version": CLIENT_VERSION},
            },
        )
        self._transport.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self._ready = True

    def tools(self) -> list[dict[str, Any]]:
        """The tools the server lists for this client (its registry profile)."""
        self._initialize()
        listed = self._request("tools/list", {}).get("tools", [])
        return [t for t in listed if isinstance(t, dict) and isinstance(t.get("name"), str)]

    def call(self, tool: str, arguments: Mapping[str, Any]) -> str:
        """The tool's text result.

        Raises RemindMeError when the tool reports an error, with its text."""
        self._initialize()
        result = self._request("tools/call", {"name": tool, "arguments": dict(arguments)})
        text = "\n".join(
            part.get("text", "")
            for part in result.get("content", [])
            if isinstance(part, dict) and part.get("type") == "text"
        )
        if result.get("isError"):
            raise RemindMeError(text or f"{tool} failed")
        return text

    def close(self) -> None:
        self._transport.close()


def hermes_schema(tool: Mapping[str, Any]) -> dict[str, Any]:
    """An MCP tool as Hermes's function schema (`name`, `description`, `parameters`)."""
    return {
        "name": tool["name"],
        "description": tool.get("description", ""),
        "parameters": tool.get("inputSchema") or {"type": "object", "properties": {}},
    }


# -- capture -----------------------------------------------------------------


@dataclass(frozen=True)
class Message:
    role: str
    text: str


def transcript(messages: Sequence[Mapping[str, Any]]) -> list[Message]:
    """OpenAI-style messages as `{role, text}`, keeping only messages with text.
    A list `content` keeps its text parts."""
    out: list[Message] = []
    for message in messages:
        role = message.get("role")
        if not isinstance(role, str) or not role.strip():
            continue
        text = _content_text(message.get("content"))
        if text.strip():
            out.append(Message(role.strip(), text))
    return out


def _content_text(content: object) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = [
            p.get("text", "")
            for p in content
            if isinstance(p, dict) and p.get("type") == "text" and isinstance(p.get("text"), str)
        ]
        return "\n".join(parts)
    return ""


def session_key(client: str, session_id: str) -> str:
    """The session id a capture is stored under: qualified by the client, so
    two profiles that reuse a Hermes session id never replace each other's
    capture (D133)."""
    return f"{client}:{session_id}"


def capture_local(command: str, client: str, session_id: str, messages: Sequence[Message]) -> str:
    """Capture through `rusty-remind-me capture-transcript --format messages`:
    one capture per session, replaced in place, labelled as a machine capture."""
    payload = json.dumps([{"role": m.role, "text": m.text} for m in messages])
    try:
        done = subprocess.run(
            [command, "capture-transcript", "--format", "messages",
             "--session-id", session_id, "--reason", "end"],
            input=payload,
            capture_output=True,
            text=True,
            encoding="utf-8",
            env=dict(os.environ, REMIND_ME_CLIENT=client),
            timeout=REQUEST_SECONDS,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        raise RemindMeError(f"capture-transcript: {type(e).__name__}") from None
    if done.returncode != 0:
        raise RemindMeError(f"capture-transcript exited {done.returncode}")
    return done.stdout


def wire_bytes(value: object) -> bytes:
    """JSON as sent over HTTP: UTF-8 with non-ASCII unescaped, the policy
    `escaped_len` sizes transcripts by. A lone surrogate becomes `?`."""
    return json.dumps(value, ensure_ascii=False).encode("utf-8", "replace")


def escaped_len(text: str) -> int:
    """Bytes `text` costs JSON-escaped, as transcript::encoded_len counts them
    and as `wire_bytes` sends it."""
    return len(wire_bytes(text)) - 2


def transcript_jsonl(messages: Sequence[Message], limit: int = MAX_TRANSCRIPT_BYTES) -> str:
    """One `{role, content}` line per message, as `messages_jsonl` writes them:
    the newest whole lines whose escaped size fits in `limit`."""
    kept: list[str] = []
    used = 0
    for m in reversed(messages):
        line = json.dumps({"role": m.role, "content": m.text}, ensure_ascii=False, separators=(",", ":")) + "\n"
        used += escaped_len(line)
        if used > limit:
            break
        kept.append(line)
    return "".join(reversed(kept))


def capture_remote(settings: Settings, client: str, session_id: str, messages: Sequence[Message]) -> str:
    """Capture through `remind_me_capture_transcript` with the capture-only
    token, in its own MCP session: one capture per session, replaced in
    place (D134). The server pins the writer to `remind-me-capture-token`."""
    if not settings.url or not settings.capture_token:
        raise RemindMeError("capture token not configured")
    transcript = transcript_jsonl(messages)
    if not transcript:
        raise RemindMeError("transcript too large")
    capture = McpClient(HttpTransport(settings.url, settings.capture_token), client)
    try:
        return capture.call(CAPTURE_TOOL, {"session_id": session_id, "transcript": transcript, "reason": "end"})
    finally:
        capture.close()


def capture_arguments(client: str, session_id: str, messages: Sequence[Message]) -> dict[str, Any]:
    """`remind_me_auto_capture` arguments for a remote capture without a capture
    token: each callback may write its own capture (D134). The summary is
    mechanical: the tool requires one and the plugin has no model of its own."""
    conversation = "\n\n".join(f"{m.role}: {m.text}" for m in messages)
    first_user = next((m.text for m in messages if m.role == "user"), "")
    opening = " ".join(first_user.split())[:200]
    summary = f"Hermes session {session_id} ({client}), {len(messages)} messages."
    if opening:
        summary += f" Opened with: {opening}"
    return {
        "conversation": conversation,
        "summary": summary,
        "title": f"Hermes {client} session {session_id}"[:120],
        "tags": ["hermes", client],
        "category": "session",
        "metadata": {"session_id": session_id, "source": "hermes"},
    }
