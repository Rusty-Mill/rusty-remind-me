"""remind-me as a Hermes memory provider (card #1552, ADR-0028).

Select it with `memory.provider: remind-me` in the profile's config.yaml.
Each Hermes profile (bot) gets its own instance and writes as
`hermes-<profile>`, so remind-me can tell JARVIS from the gateway.

- system prompt: `remind_me_brief` for the configured project, once.
- before each call: `remind_me_brief` Prompt mode with the turn's text (D132).
- tools: whatever remind-me lists for this client (its registry profile,
  `minimal` for small models), proxied as they are.
- session end: the transcript is captured, once per session.
- per turn: nothing is written; the model saves facts with `remind_me_add`.
"""
from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any

try:
    from agent.memory_provider import MemoryProvider
except ImportError:  # Outside Hermes (the tests): a plain base.
    MemoryProvider = object  # type: ignore[misc,assignment]

try:
    from . import remind_me_client as rm
except ImportError:  # Loaded as a lone module: import the sibling by path.
    import importlib.util

    _spec = importlib.util.spec_from_file_location(
        "remind_me_client", Path(__file__).with_name("remind_me_client.py")
    )
    assert _spec is not None and _spec.loader is not None
    rm = importlib.util.module_from_spec(_spec)
    _spec.loader.exec_module(rm)

log = logging.getLogger(__name__)
PROVIDER_NAME = "remind-me"


class RemindMeProvider(MemoryProvider):  # type: ignore[misc,valid-type]
    """Hermes's memory, kept in remind-me."""

    def __init__(self) -> None:
        self._settings: rm.Settings | None = None
        self._problem = ""
        self._client_id = rm.client_id(None)
        self._session_id = ""
        self._primary = True
        self._mcp: rm.McpClient | None = None
        self._prompt_block: str | None = None
        self._prefetched: tuple[str, str] | None = None
        self._tools: list[dict[str, Any]] | None = None

    @property
    def name(self) -> str:
        return PROVIDER_NAME

    # -- lifecycle -----------------------------------------------------------

    def _load(self, hermes_home: str | None) -> rm.Settings | None:
        try:
            settings = rm.load_settings(hermes_home, os.environ)
        except rm.RemindMeError as e:
            self._problem = str(e)
            return None
        self._problem = settings.problem() or ""
        return None if self._problem else settings

    def is_available(self) -> bool:
        """Configured and runnable; no network."""
        return self._load(os.environ.get("HERMES_HOME")) is not None

    def unavailable_reason(self) -> str:
        return self._problem

    def initialize(self, session_id: str, **kwargs: Any) -> None:
        self._settings = self._load(kwargs.get("hermes_home"))
        self._client_id = rm.client_id(kwargs.get("agent_identity"))
        self._session_id = session_id
        # Cron jobs and subagents read memory but do not capture.
        self._primary = kwargs.get("agent_context", "primary") == "primary"

    def on_session_switch(self, new_session_id: str, **kwargs: Any) -> None:
        self._session_id = new_session_id
        self._prefetched = None

    def shutdown(self) -> None:
        if self._mcp is not None:
            self._mcp.close()
            self._mcp = None

    def _client(self) -> rm.McpClient | None:
        if self._mcp is not None or self._settings is None:
            return self._mcp
        s = self._settings
        try:
            transport: rm.Transport = (
                rm.HttpTransport(s.url or "", s.token or "")
                if s.remote
                else rm.StdioTransport(s.command, self._client_id)
            )
        except (rm.RemindMeError, OSError) as e:
            log.warning("remind-me: cannot connect: %s", type(e).__name__)
            return None
        self._mcp = rm.McpClient(transport, self._client_id)
        return self._mcp

    def _call(self, tool: str, arguments: dict[str, Any]) -> str:
        """`tool`'s text, or "" when remind-me cannot be reached: memory is an
        aid, so a failure is logged and the turn goes on without it."""
        client = self._client()
        if client is None:
            return ""
        try:
            return client.call(tool, arguments)
        except rm.RemindMeError as e:
            log.warning("remind-me: %s: %s", tool, e)
            return ""

    # -- recall --------------------------------------------------------------

    def system_prompt_block(self) -> str:
        """The configured project's brief, fetched once; "" without a project."""
        if self._prompt_block is None:
            project = self._settings.project if self._settings else None
            self._prompt_block = self._brief({"project": project}) if project else ""
        return self._prompt_block

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        """Memories ranked for this turn's text, cached for a repeated query."""
        if not query.strip() or self._settings is None:
            return ""
        if self._prefetched and self._prefetched[0] == query:
            return self._prefetched[1]
        text = self._brief({"prompt": query})
        self._prefetched = (query, text)
        return text

    def _brief(self, target: dict[str, str]) -> str:
        """A brief as Markdown for the prompt. No token_budget: the registry's
        brief_budget for this client applies (D128)."""
        return self._call("remind_me_brief", {**target, "response_format": "markdown"})

    def sync_turn(self, user_content: str, assistant_content: str, **kwargs: Any) -> None:
        """Nothing per turn: the model saves durable facts with remind_me_add."""

    # -- tools ---------------------------------------------------------------

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        if self._tools is None:
            client = self._client()
            try:
                self._tools = [rm.hermes_schema(t) for t in client.tools()] if client else []
            except rm.RemindMeError as e:
                log.warning("remind-me: tools/list: %s", e)
                self._tools = []
        return self._tools

    def handle_tool_call(self, tool_name: str, args: dict[str, Any], **kwargs: Any) -> str:
        """The tool's result as a JSON string: `{"result": text}` or `{"error": text}`."""
        client = self._client()
        if client is None:
            return json.dumps({"error": f"remind-me unavailable: {self._problem or 'not connected'}"})
        try:
            return json.dumps({"result": client.call(tool_name, args)})
        except rm.RemindMeError as e:
            return json.dumps({"error": str(e)})

    # -- capture -------------------------------------------------------------

    def on_session_end(self, messages: list[dict[str, Any]]) -> None:
        """Capture the conversation once per session (ADR-0028 §5): locally
        through `capture-transcript --format messages`, remotely through
        `remind_me_capture_transcript` with the capture token, else
        `remind_me_auto_capture` (D134). Redaction is the server's (D15)."""
        if not self._primary or self._settings is None or not self._session_id:
            return
        kept = rm.transcript(messages)
        if not kept:
            return
        session = rm.session_key(self._client_id, self._session_id)
        try:
            if self._settings.remote and self._settings.capture_token:
                rm.capture_remote(self._settings, self._client_id, session, kept)
            elif self._settings.remote:
                args = rm.capture_arguments(self._client_id, session, kept)
                client = self._client()
                if client is not None:
                    client.call("remind_me_auto_capture", args)
            else:
                rm.capture_local(self._settings.command, self._client_id, session, kept)
        except rm.RemindMeError as e:
            log.warning("remind-me: capture: %s", e)

    # -- setup ---------------------------------------------------------------

    def get_config_schema(self) -> list[dict[str, Any]]:
        return [
            {"key": "url", "description": "Remote remind-me MCP URL (ending /mcp); blank runs it locally",
             "required": False, "default": ""},
            {"key": "token", "description": "Remote connector token", "secret": True,
             "required": False, "env_var": "REMIND_ME_TOKEN"},
            {"key": "capture_token", "description": "Remote capture-only token, for one capture per session (optional)",
             "secret": True, "required": False, "env_var": "REMIND_ME_CAPTURE_TOKEN"},
            {"key": "project", "description": "Project whose brief opens each session (optional)",
             "required": False, "default": ""},
        ]

    def save_config(self, values: dict[str, Any], hermes_home: str) -> None:
        """Keep the non-secret settings in `$HERMES_HOME/remind-me.json`."""
        path = Path(hermes_home) / rm.SETTINGS_FILE
        current: dict[str, Any] = {}
        if path.exists():
            try:
                loaded = json.loads(path.read_text(encoding="utf-8"))
                current = loaded if isinstance(loaded, dict) else {}
            except (OSError, ValueError) as e:
                log.warning("remind-me: replacing unreadable %s: %s", path.name, type(e).__name__)
        for key in ("url", "project", "command"):
            if key in values:
                current[key] = values[key]
        temp = path.with_suffix(".tmp")
        temp.write_text(json.dumps(current, indent=2), encoding="utf-8")
        temp.replace(path)


def register(ctx: Any) -> None:
    """Hermes's entry point: a new instance per call (one per profile)."""
    ctx.register_memory_provider(RemindMeProvider())
