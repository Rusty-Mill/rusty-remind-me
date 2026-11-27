"""Tests for the remind-me Hermes memory provider. Standard library only.

    python3 -m unittest discover -s plugins/hermes-memory/tests -v

Set REMIND_ME_TEST_BINARY to a built `rusty-remind-me` to also run the
round trip against the real server (it uses a throwaway store).
"""
from __future__ import annotations

import importlib.util
import json
import os
import stat
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from typing import Any

PLUGIN = Path(__file__).resolve().parents[1]


def _load(name: str, path: Path, package: bool = False) -> Any:
    spec = importlib.util.spec_from_file_location(
        name, path, submodule_search_locations=[str(PLUGIN)] if package else None
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


plugin = _load("hermes_memory", PLUGIN / "__init__.py", package=True)
rm = plugin.rm  # The package's own copy, so its exception classes match.


class FakeTransport:
    """Answers like remind-me: records what was sent."""

    def __init__(self, tools: list[dict[str, Any]] | None = None, fail: set[str] | None = None) -> None:
        self.sent: list[dict[str, Any]] = []
        self.tools = tools or [{"name": "remind_me_search", "description": "d", "inputSchema": {"type": "object"}}]
        self.fail = fail or set()
        self.closed = False

    def send(self, message: dict[str, Any]) -> dict[str, Any] | None:
        self.sent.append(message)
        if "id" not in message:
            return None
        method = message["method"]
        if method == "initialize":
            result: dict[str, Any] = {"protocolVersion": rm.PROTOCOL_VERSION}
        elif method == "tools/list":
            result = {"tools": self.tools}
        else:
            name = message["params"]["name"]
            text = f"{name}:{json.dumps(message['params']['arguments'], sort_keys=True)}"
            result = {"content": [{"type": "text", "text": text}], "isError": name in self.fail}
        return {"jsonrpc": "2.0", "id": message["id"], "result": result}

    def close(self) -> None:
        self.closed = True

    def calls(self) -> list[tuple[str, dict[str, Any]]]:
        return [
            (m["params"]["name"], m["params"]["arguments"])
            for m in self.sent
            if m.get("method") == "tools/call"
        ]


def provider_with(transport: FakeTransport, settings: Any, **init: Any) -> Any:
    p = plugin.RemindMeProvider()
    p.initialize("sess-1", **init)
    p._settings = settings
    p._mcp = rm.McpClient(transport, p._client_id)
    return p


class ClientIdTest(unittest.TestCase):
    def test_profile_becomes_a_registry_id(self) -> None:
        self.assertEqual(rm.client_id("JARVIS"), "hermes-jarvis")
        self.assertEqual(rm.client_id("My Gateway_2"), "hermes-my-gateway-2")
        self.assertEqual(rm.client_id(None), "hermes-default")
        self.assertEqual(rm.client_id("!!!"), "hermes-default")
        long = rm.client_id("x" * 80)
        self.assertLessEqual(len(long), 40)
        self.assertRegex(long, r"^[a-z0-9-]{1,40}$")


class SettingsTest(unittest.TestCase):
    def test_file_then_env(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            Path(home, rm.SETTINGS_FILE).write_text(
                json.dumps({"project": "rusty-remind-me", "url": "https://a/mcp"})
            )
            s = rm.load_settings(home, {"REMIND_ME_URL": "https://b/mcp", "REMIND_ME_TOKEN": "t"})
        self.assertEqual((s.url, s.token, s.project), ("https://b/mcp", "t", "rusty-remind-me"))
        self.assertTrue(s.remote)
        self.assertIsNone(s.problem())
        self.assertNotIn("'t'", repr(s), "the token never shows in a repr")

    def test_the_capture_token_comes_from_the_environment_only(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            Path(home, rm.SETTINGS_FILE).write_text(json.dumps({"capture_token": "from-file"}))
            s = rm.load_settings(home, {"REMIND_ME_URL": "https://a/mcp", "REMIND_ME_TOKEN": "t"})
            self.assertIsNone(s.capture_token)
            s = rm.load_settings(home, {"REMIND_ME_CAPTURE_TOKEN": " c-token "})
        self.assertEqual(s.capture_token, "c-token")
        self.assertNotIn("c-token", repr(s))

    def test_defaults_are_local(self) -> None:
        s = rm.load_settings(None, {})
        self.assertFalse(s.remote)
        self.assertEqual(s.command, rm.DEFAULT_COMMAND)

    def test_problems_are_named_without_network(self) -> None:
        self.assertIn("token", rm.Settings(url="https://a/mcp").problem() or "")
        self.assertIn("https", rm.Settings(url="http://example.com/mcp", token="t").problem() or "")
        self.assertIsNone(rm.Settings(url="http://localhost:8000/mcp", token="t").problem())
        self.assertIn("PATH", rm.Settings(command="no-such-binary-here").problem() or "")

    def test_a_broken_file_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            Path(home, rm.SETTINGS_FILE).write_text("{nope")
            with self.assertRaises(rm.RemindMeError):
                rm.load_settings(home, {})
            Path(home, rm.SETTINGS_FILE).write_text("[]")
            with self.assertRaises(rm.RemindMeError):
                rm.load_settings(home, {})


class WireTest(unittest.TestCase):
    def test_sse_and_plain_replies(self) -> None:
        reply = {"jsonrpc": "2.0", "id": 3, "result": {}}
        self.assertEqual(rm.reply_to(3, json.dumps(reply), sse=False), reply)
        stream = f"event: message\ndata: {json.dumps({'id': 2})}\n\ndata: {json.dumps(reply)}\n\n"
        self.assertEqual(rm.reply_to(3, stream, sse=True), reply)
        self.assertIsNone(rm.reply_to(4, stream, sse=True))

    def test_client_initialises_once_and_names_itself(self) -> None:
        t = FakeTransport()
        client = rm.McpClient(t, "hermes-jarvis")
        client.call("remind_me_search", {"query": "x"})
        client.call("remind_me_search", {"query": "y"})
        methods = [m.get("method") for m in t.sent]
        self.assertEqual(methods.count("initialize"), 1)
        self.assertEqual(t.sent[0]["params"]["clientInfo"]["name"], "hermes-jarvis")
        self.assertIn("notifications/initialized", methods)

    def test_a_tool_error_raises_with_its_text(self) -> None:
        client = rm.McpClient(FakeTransport(fail={"remind_me_add"}), "c")
        with self.assertRaisesRegex(rm.RemindMeError, "remind_me_add"):
            client.call("remind_me_add", {})


class ProviderTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = FakeTransport()
        self.p = provider_with(self.t, rm.Settings(project="rusty-remind-me"), agent_identity="JARVIS")

    def test_name_and_client(self) -> None:
        self.assertEqual(self.p.name, "remind-me")
        self.assertEqual(self.p._client_id, "hermes-jarvis")

    def test_system_block_is_the_project_brief_once(self) -> None:
        first = self.p.system_prompt_block()
        self.p.system_prompt_block()
        self.assertEqual(
            self.t.calls(), [("remind_me_brief", {"project": "rusty-remind-me", "response_format": "markdown"})]
        )
        self.assertTrue(first.startswith("remind_me_brief"))

    def test_no_project_no_block(self) -> None:
        p = provider_with(self.t, rm.Settings())
        self.assertEqual(p.system_prompt_block(), "")
        self.assertEqual(self.t.calls(), [])

    def test_prefetch_briefs_the_turn_and_caches_it(self) -> None:
        self.p.prefetch("what did we decide about sqlite")
        self.p.prefetch("what did we decide about sqlite")
        self.assertEqual(self.p.prefetch("   "), "")
        self.assertEqual(
            self.t.calls(),
            [("remind_me_brief", {"prompt": "what did we decide about sqlite", "response_format": "markdown"})],
        )

    def test_a_failure_is_empty_recall_not_a_crash(self) -> None:
        p = provider_with(FakeTransport(fail={"remind_me_brief"}), rm.Settings())
        with self.assertLogs(plugin.log, "WARNING"):
            self.assertEqual(p.prefetch("x"), "")

    def test_tools_are_the_servers_list_in_hermes_shape(self) -> None:
        schemas = self.p.get_tool_schemas()
        self.assertEqual(schemas, [{"name": "remind_me_search", "description": "d", "parameters": {"type": "object"}}])
        out = json.loads(self.p.handle_tool_call("remind_me_search", {"query": "q"}))
        self.assertIn("result", out)
        p = provider_with(FakeTransport(fail={"remind_me_add"}), rm.Settings())
        self.assertIn("error", json.loads(p.handle_tool_call("remind_me_add", {})))

    def test_sync_turn_writes_nothing(self) -> None:
        self.p.sync_turn("u", "a", session_id="s")
        self.assertEqual(self.t.sent, [])

    def test_remote_capture_uses_auto_capture(self) -> None:
        p = provider_with(self.t, rm.Settings(url="https://a/mcp", token="t"), agent_identity="gw")
        p.on_session_end([
            {"role": "system", "content": "be nice"},
            {"role": "user", "content": [{"type": "text", "text": "ship it?"}, {"type": "image_url"}]},
            {"role": "assistant", "content": "yes"},
            {"role": "tool", "content": None},
        ])
        [(tool, args)] = self.t.calls()
        self.assertEqual(tool, "remind_me_auto_capture")
        self.assertEqual(args["conversation"], "system: be nice\n\nuser: ship it?\n\nassistant: yes")
        self.assertIn("Opened with: ship it?", args["summary"])
        self.assertEqual(args["tags"], ["hermes", "hermes-gw"])
        self.assertEqual(args["metadata"]["session_id"], "hermes-gw:sess-1")

    def test_remote_capture_with_the_capture_token_is_session_keyed(self) -> None:
        seen: list[tuple[Any, str, str, list[Any]]] = []
        original = rm.capture_remote
        rm.capture_remote = lambda settings, client, session, kept: seen.append((settings, client, session, kept)) or ""
        settings = rm.Settings(url="https://a/mcp", token="t", capture_token="c")
        try:
            p = provider_with(self.t, settings, agent_identity="gw")
            p.on_session_end([{"role": "user", "content": "ship it?"}])
        finally:
            rm.capture_remote = original
        self.assertEqual(seen, [(settings, "hermes-gw", "hermes-gw:sess-1", [rm.Message("user", "ship it?")])])
        self.assertEqual(self.t.calls(), [], "no auto_capture beside it")

    def test_local_capture_keys_the_session_by_client(self) -> None:
        seen: list[tuple[str, str]] = []
        original = rm.capture_local
        rm.capture_local = lambda command, client, session, kept: seen.append((client, session)) or ""
        try:
            for identity in ("jarvis", "gw"):
                p = provider_with(self.t, rm.Settings(), agent_identity=identity)
                p.on_session_end([{"role": "user", "content": "hi"}])
        finally:
            rm.capture_local = original
        self.assertEqual(seen, [("hermes-jarvis", "hermes-jarvis:sess-1"), ("hermes-gw", "hermes-gw:sess-1")])

    def test_subagents_and_empty_sessions_do_not_capture(self) -> None:
        p = provider_with(self.t, rm.Settings(url="https://a/mcp", token="t"), agent_context="subagent")
        p.on_session_end([{"role": "user", "content": "hi"}])
        self.p.on_session_end([{"role": "user", "content": "  "}])
        self.assertEqual(self.t.calls(), [])

    def test_save_config_keeps_secrets_out(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            self.p.save_config({"url": "https://a/mcp", "token": "secret", "capture_token": "c", "project": "p"}, home)
            saved = json.loads(Path(home, rm.SETTINGS_FILE).read_text())
        self.assertEqual(saved, {"url": "https://a/mcp", "project": "p"})

    def test_register_makes_a_new_instance_each_time(self) -> None:
        made: list[Any] = []

        class Ctx:
            def register_memory_provider(self, provider: Any) -> None:
                made.append(provider)

        plugin.register(Ctx())
        plugin.register(Ctx())
        self.assertEqual(len(made), 2)
        self.assertIsNot(made[0], made[1])


FAKE_SERVER = textwrap.dedent(
    """\
    #!{python}
    import json, sys, os
    if sys.argv[1] == "capture-transcript":
        data = json.load(sys.stdin)
        with open(os.environ["FAKE_OUT"], "w") as f:
            json.dump({{"argv": sys.argv[1:], "client": os.environ.get("REMIND_ME_CLIENT"), "data": data}}, f)
        print(json.dumps({{"messages": len(data)}}))
        sys.exit(0)
    for line in sys.stdin:
        msg = json.loads(line)
        if "id" not in msg:
            continue
        if msg["method"] == "tools/call":
            result = {{"content": [{{"type": "text", "text": os.environ.get("REMIND_ME_CLIENT", "")}}]}}
        else:
            result = {{}}
        print(json.dumps({{"jsonrpc": "2.0", "id": msg["id"], "result": result}}), flush=True)
    """
)


class TranscriptTest(unittest.TestCase):
    def test_lines_are_flat_role_content_json(self) -> None:
        got = rm.transcript_jsonl([rm.Message("user", 'say "hi"\nplease'), rm.Message("assistant", "héllo")])
        lines = got.splitlines()
        self.assertEqual([json.loads(l) for l in lines], [
            {"role": "user", "content": 'say "hi"\nplease'},
            {"role": "assistant", "content": "héllo"},
        ])
        self.assertTrue(got.endswith("\n"))
        self.assertIn("héllo", got, "non-ASCII is not escaped, as on the server")

    def test_the_newest_whole_lines_fit_the_escaped_limit(self) -> None:
        messages = [rm.Message("user", f"turn {n}") for n in range(5)]
        line = rm.transcript_jsonl(messages[-1:])
        limit = 2 * rm.escaped_len(line)
        got = rm.transcript_jsonl(messages, limit)
        self.assertEqual([json.loads(l)["content"] for l in got.splitlines()], ["turn 3", "turn 4"])
        self.assertLessEqual(rm.escaped_len(got), limit)
        self.assertEqual(rm.escaped_len('"\\\n'), 6, "quote, backslash and newline count twice")
        self.assertEqual(rm.transcript_jsonl([rm.Message("user", "x" * 50)], 10), "")


class CaptureServer:
    """A loopback MCP endpoint that records each request (headers and body)."""

    def __init__(self) -> None:
        import http.server
        import threading

        seen = self.seen = []
        raw = self.raw = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802 (http.server's name)
                raw_body = self.rfile.read(int(self.headers["Content-Length"]))
                raw.append(raw_body)
                body = json.loads(raw_body)
                seen.append((dict(self.headers), body))
                if "id" not in body:
                    self.send_response(202)
                    self.end_headers()
                    return
                if body["method"] == "initialize":
                    result: dict[str, Any] = {"protocolVersion": rm.PROTOCOL_VERSION}
                else:
                    result = {"content": [{"type": "text", "text": "{}"}]}
                reply = json.dumps({"jsonrpc": "2.0", "id": body["id"], "result": result}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Mcp-Session-Id", "sess-abc")
                self.send_header("Content-Length", str(len(reply)))
                self.end_headers()
                self.wfile.write(reply)

            def log_message(self, *args: Any) -> None:
                pass

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}/mcp"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


class CaptureRemoteTest(unittest.TestCase):
    def test_capture_transcript_goes_with_the_capture_token_in_its_own_session(self) -> None:
        server = CaptureServer()
        try:
            settings = rm.Settings(url=server.url, token="connector", capture_token="capture")
            rm.capture_remote(settings, "hermes-gw", "hermes-gw:s1", [rm.Message("user", "hi")])
        finally:
            server.close()
        methods = [body.get("method") for _, body in server.seen]
        self.assertEqual(methods, ["initialize", "notifications/initialized", "tools/call"])
        self.assertTrue(all(h["Authorization"] == "Bearer capture" for h, _ in server.seen))
        headers, call = server.seen[-1]
        self.assertEqual(headers["Mcp-Session-Id"], "sess-abc")
        self.assertEqual(call["params"], {"name": "remind_me_capture_transcript", "arguments": {
            "session_id": "hermes-gw:s1",
            "transcript": '{"role":"user","content":"hi"}\n',
            "reason": "end",
        }})

    def test_multilingual_text_is_sent_as_it_was_sized(self) -> None:
        # Server limits: MAX_TRANSCRIPT_BYTES escaped, a 4 MiB request body.
        text = "Grüße, 東京で会いましょう — до встречи 👋 \"ok\"\n" * 2000
        messages = [rm.Message("user" if n % 2 else "assistant", text) for n in range(60)]
        server = CaptureServer()
        try:
            settings = rm.Settings(url=server.url, token="connector", capture_token="capture")
            rm.capture_remote(settings, "hermes-gw", "hermes-gw:s1", messages)
        finally:
            server.close()
        body = server.raw[-1]
        transcript = server.seen[-1][1]["params"]["arguments"]["transcript"]
        self.assertNotIn(b"\\u", body, "non-ASCII travels as UTF-8, not escapes")
        self.assertLessEqual(rm.escaped_len(transcript), rm.MAX_TRANSCRIPT_BYTES)
        self.assertGreater(rm.escaped_len(transcript), rm.MAX_TRANSCRIPT_BYTES - 2 * len(text.encode()))
        overhead = len(body) - rm.escaped_len(transcript)
        self.assertLess(overhead, 1024, "the body is the sized transcript plus a small envelope")
        self.assertLessEqual(len(body), 4 * 1024 * 1024)

    def test_no_capture_token_or_nothing_that_fits_raises(self) -> None:
        with self.assertRaises(rm.RemindMeError):
            rm.capture_remote(rm.Settings(url="https://a/mcp", token="t"), "c", "s", [rm.Message("user", "x")])


class ProcessTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.bin = Path(self.dir.name, "fake-rrm")
        self.bin.write_text(FAKE_SERVER.format(python=sys.executable))
        self.bin.chmod(self.bin.stat().st_mode | stat.S_IEXEC)

    def tearDown(self) -> None:
        self.dir.cleanup()

    @unittest.skipIf(os.name == "nt", "shebang scripts")
    def test_stdio_server_gets_the_client_id(self) -> None:
        client = rm.McpClient(rm.StdioTransport(str(self.bin), "hermes-jarvis"), "hermes-jarvis")
        try:
            self.assertEqual(client.call("remind_me_search", {"query": "q"}), "hermes-jarvis")
        finally:
            client.close()

    @unittest.skipIf(os.name == "nt", "shebang scripts")
    def test_local_capture_pipes_messages(self) -> None:
        out = Path(self.dir.name, "out.json")
        os.environ["FAKE_OUT"] = str(out)
        try:
            rm.capture_local(str(self.bin), "hermes-jarvis", "s9", [rm.Message("user", "hi")])
        finally:
            del os.environ["FAKE_OUT"]
        seen = json.loads(out.read_text())
        self.assertEqual(seen["argv"], ["capture-transcript", "--format", "messages", "--session-id", "s9", "--reason", "end"])
        self.assertEqual(seen["client"], "hermes-jarvis")
        self.assertEqual(seen["data"], [{"role": "user", "text": "hi"}])

    def test_a_failing_capture_raises(self) -> None:
        with self.assertRaises(rm.RemindMeError):
            rm.capture_local(str(Path(self.dir.name, "missing")), "c", "s", [rm.Message("user", "x")])


@unittest.skipUnless(os.environ.get("REMIND_ME_TEST_BINARY"), "set REMIND_ME_TEST_BINARY to run")
class LiveTest(unittest.TestCase):
    """Against a real `rusty-remind-me`, on a throwaway store."""

    def test_round_trip(self) -> None:
        binary = os.environ["REMIND_ME_TEST_BINARY"]
        with tempfile.TemporaryDirectory() as store:
            saved = dict(os.environ)
            os.environ.update({"REMIND_ME_MCP_DIR": store, "REMIND_ME_TOOL_PROFILE": "minimal"})
            try:
                client = rm.McpClient(rm.StdioTransport(binary, "hermes-live"), "hermes-live")
                try:
                    names = {t["name"] for t in client.tools()}
                    self.assertEqual(names, {"remind_me_search", "remind_me_get", "remind_me_add",
                                             "remind_me_auto_capture", "remind_me_brief"})
                    client.call("remind_me_add", {"content": "the live quokka fact"})
                    self.assertIn("quokka", client.call("remind_me_search", {"query": "quokka"}))
                    brief = client.call("remind_me_brief", {"prompt": "the quokka", "response_format": "markdown"})
                    self.assertIn("quokka", brief)
                finally:
                    client.close()
                out = rm.capture_local(binary, "hermes-live", "live-1",
                                       [rm.Message("user", "hello"), rm.Message("assistant", "hi there")])
                self.assertEqual(json.loads(out)["messages"], 2)
            finally:
                os.environ.clear()
                os.environ.update(saved)


if __name__ == "__main__":
    unittest.main()
