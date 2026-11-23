"""remind-me-capture.py: the cloud session capture hook (ADR-0029, ADR-0036).

Synthetic only: a local fake MCP endpoint, fake tokens, generated transcripts.
Run: python3 -m unittest discover -s plugins/cloud-capture/tests -v
"""
import contextlib
import http.server
import importlib.util
import io
import json
import pathlib
import shutil
import tempfile
import threading
import unittest
from unittest import mock

HOOKS = pathlib.Path(__file__).resolve().parents[1] / 'hooks'
spec = importlib.util.spec_from_file_location('remind_me_capture', HOOKS / 'remind-me-capture.py')
hook = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hook)

TOKEN = 'fake-t0ken-for-tests'
GHP = 'ghp_' + 'A' * 36


class FakeServer(http.server.BaseHTTPRequestHandler):
    """A remind-me MCP endpoint: session-managed, answering tools/call as an event stream."""
    calls = []
    bodies = []
    tool_text = 'ok'
    fail_tool = False
    redirect_to = None
    pad = 0
    init_version = None

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        message = json.loads(raw)
        FakeServer.calls.append((dict(self.headers), message))
        FakeServer.bodies.append(raw)
        if FakeServer.redirect_to:
            self.send_response(302)
            self.send_header('Location', FakeServer.redirect_to)
            self.end_headers()
            return
        if self.headers.get('Authorization') != f'Bearer {TOKEN}':
            self.send_response(401)
            self.end_headers()
            return
        if 'id' not in message:
            self.send_response(202)
            self.end_headers()
            return
        if message['method'] == 'initialize':
            result = {'protocolVersion': FakeServer.init_version} if FakeServer.init_version else {}
            body, kind = json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result}), 'application/json'
        else:
            content = [{'type': 'text', 'text': FakeServer.tool_text + ' ' * FakeServer.pad}]
            result = {'isError': FakeServer.fail_tool, 'content': content}
            reply = json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result})
            body, kind = f'event: message\ndata: {reply}\n\n', 'text/event-stream'
        self.send_response(200)
        self.send_header('Content-Type', kind)
        self.send_header('Mcp-Session-Id', 'sess-1')
        self.end_headers()
        self.wfile.write(body.encode())

    def log_message(self, *args):
        pass


def serve():
    server = http.server.HTTPServer(('127.0.0.1', 0), FakeServer)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Run(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.transcript = self.tmp / 's.jsonl'
        self.transcript.write_text('{"type":"user","message":{"role":"user","content":"hi"}}\n')
        self.server = serve()
        FakeServer.calls, FakeServer.bodies = [], []
        FakeServer.fail_tool, FakeServer.redirect_to = False, None
        FakeServer.tool_text, FakeServer.pad, FakeServer.init_version = 'ok', 0, None
        self.env = {'CLAUDE_CODE_REMOTE': 'true', 'REMIND_ME_CLOUD_CAPTURE': '1',
                    'REMIND_ME_TOKEN': TOKEN,
                    'REMIND_ME_URL': f'http://127.0.0.1:{self.server.server_port}/mcp'}

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        shutil.rmtree(self.tmp, ignore_errors=True)

    def payload(self, event='Stop', **extra):
        return {'hook_event_name': event, 'session_id': 'cloud-1', 'cwd': str(self.tmp),
                'transcript_path': str(self.transcript), **extra}

    def run_hook(self, payload, now=1000.0, env=None, deadline=None, config=None):
        return hook.run(payload, self.env if env is None else env, now, self.tmp / 'state', deadline,
                        self.tmp / 'config.json' if config is None else config)

    def write_config(self, **fields):
        path = self.tmp / 'config.json'
        path.write_text(json.dumps(fields))
        return path

    def stderr_of_main(self, env):
        """What main() prints for this payload and environment."""
        err = io.StringIO()
        with mock.patch.dict(hook.os.environ, env, clear=True), \
                mock.patch.object(hook.sys, 'stdin', io.StringIO(json.dumps(self.payload()))), \
                mock.patch.object(hook.tempfile, 'gettempdir', return_value=str(self.tmp)), \
                mock.patch.object(hook, 'CONFIG', self.tmp / 'config.json'), \
                contextlib.redirect_stderr(err):
            self.assertEqual(hook.main(), 0)
        return err.getvalue()

    def test_captures_through_the_mcp_handshake(self):
        self.assertEqual(self.run_hook(self.payload()), 'captured')
        methods = [m.get('method') for _, m in FakeServer.calls]
        self.assertEqual(methods, ['initialize', 'notifications/initialized', 'tools/call'])
        headers, call = FakeServer.calls[-1]
        self.assertEqual(headers.get('Mcp-Session-Id'), 'sess-1')
        self.assertEqual(headers.get('User-Agent'), 'remind-me-capture/1', 'not urllib\'s default')
        args = call['params']['arguments']
        self.assertEqual(call['params']['name'], 'remind_me_capture_transcript')
        self.assertEqual((args['session_id'], args['reason']), ('cloud-1', 'stop'))
        self.assertEqual(args['transcript'], self.transcript.read_text())
        self.assertEqual(args['cwd'], str(self.tmp))

    def test_off_without_the_opt_in_a_cloud_session_or_config(self):
        for missing in ('CLAUDE_CODE_REMOTE', 'REMIND_ME_CLOUD_CAPTURE', 'REMIND_ME_URL', 'REMIND_ME_TOKEN'):
            env = {k: v for k, v in self.env.items() if k != missing}
            self.assertEqual(self.run_hook(self.payload(), env=env), 'off')
        self.assertEqual(self.run_hook(self.payload(), env={**self.env, 'REMIND_ME_CLOUD_CAPTURE': 'yes'}), 'off')
        self.assertEqual(FakeServer.calls, [])

    def test_a_config_file_turns_capture_on_without_the_opt_in(self):
        url = self.env['REMIND_ME_URL']
        config = self.write_config(url=url, token=TOKEN)
        env = {'CLAUDE_CODE_REMOTE': 'true'}
        self.assertEqual(self.run_hook(self.payload(), env=env, config=config), 'captured')
        headers = {k.lower(): v for k, v in FakeServer.calls[-1][0].items()}
        self.assertEqual(headers.get('authorization'), f'Bearer {TOKEN}')

    def test_a_config_file_wins_over_the_environment(self):
        config = self.write_config(url=self.env['REMIND_ME_URL'], token=TOKEN)
        env = {**self.env, 'REMIND_ME_TOKEN': 'some-other-token'}
        self.assertEqual(self.run_hook(self.payload(), env=env, config=config), 'captured')

    def test_a_config_file_does_nothing_outside_a_cloud_session(self):
        config = self.write_config(url=self.env['REMIND_ME_URL'], token=TOKEN)
        self.assertEqual(self.run_hook(self.payload(), env={}, config=config), 'off')
        self.assertEqual(FakeServer.calls, [])

    def test_a_broken_config_file_is_an_error_not_a_fallback(self):
        for text in ('not json', '[]', '{"url": "https://x/mcp"}', '{"url": "", "token": "t"}'):
            path = self.tmp / 'config.json'
            path.write_text(text)
            with self.assertRaises(hook.CaptureError) as caught:
                self.run_hook(self.payload(), config=path)
            self.assertEqual(str(caught.exception), 'invalid config.json', text)
        self.assertEqual(FakeServer.calls, [])

    def test_stop_is_throttled_after_success_and_skipped_when_reentrant(self):
        self.assertEqual(self.run_hook(self.payload()), 'captured')
        self.assertEqual(self.run_hook(self.payload(), now=1030.0), 'throttled')
        self.assertEqual(self.run_hook(self.payload('PreCompact'), now=1031.0), 'captured')
        self.assertEqual(FakeServer.calls[-1][1]['params']['arguments']['reason'], 'precompact')
        self.assertEqual(self.run_hook(self.payload(stop_hook_active=True), now=2000.0), 're-entrant')
        self.assertEqual(self.run_hook(self.payload(), now=1061.0), 'captured')

    def test_a_failed_stop_capture_is_not_throttled(self):
        FakeServer.fail_tool = True
        with self.assertRaises(hook.CaptureError):
            self.run_hook(self.payload())
        FakeServer.fail_tool = False
        self.assertEqual(self.run_hook(self.payload(), now=1001.0), 'captured')

    def test_a_missing_transcript_captures_nothing(self):
        payload = self.payload(transcript_path=str(self.tmp / 'gone.jsonl'))
        self.assertEqual(self.run_hook(payload), 'nothing to capture')

    def test_secrets_and_thinking_never_leave_the_container(self):
        lines = [
            {'type': 'assistant', 'message': {'content': [
                {'type': 'thinking', 'thinking': 'private reasoning'},
                {'type': 'text', 'text': f'pushed with {GHP}'}]}},
            {'type': 'user', 'message': {'content': 'export API_KEY=abcd1234efgh5678'}},
            {'type': 'tool_result', 'content': f'Authorization: Bearer {TOKEN} and Bearer zz9yy8xx7ww6'},
        ]
        self.transcript.write_text(''.join(json.dumps(line) + '\n' for line in lines))
        self.assertEqual(self.run_hook(self.payload()), 'captured')
        sent = b''.join(FakeServer.bodies[-1:]).decode()
        for leaked in ('private reasoning', GHP, 'abcd1234efgh5678', TOKEN, 'zz9yy8xx7ww6'):
            self.assertNotIn(leaked, sent)
        self.assertIn('[REDACTED:github_token]', sent)
        self.assertIn('[REDACTED:secret]', sent)

    def test_secret_named_fields_and_serialized_results_are_withheld(self):
        result = json.dumps({'config': {'db_password': 'opaque-one'}, 'rows': 3})
        lines = [
            {'type': 'assistant', 'message': {'content': [{'type': 'tool_use', 'name': 'login', 'input': {
                'user': 'bob', 'password': 'opaque-two', 'nested': {'API_KEY': 'opaque-three'},
                'pin': {'token': 90817263}, 'api_key': 55443322, 'secret': 7.25e8}}],
                'usage': {'output_tokens': 41, 'cache_read_input_tokens': 12}}},
            {'type': 'user', 'message': {'content': json.dumps({'password': 31415926})}},
            {'type': 'user', 'message': {'content': [{'type': 'tool_result', 'content': result}]}},
            {'type': 'user', 'message': {'content': json.dumps(json.dumps({'client_secret': 'opaque-four'}))}},
            {'type': 'user', 'message': {'content': 'creds.yaml has token: "op5"'}},
            {'type': 'system', 'env': {GHP: 'set'}},
        ]
        self.transcript.write_text(''.join(json.dumps(line) + '\n' for line in lines))
        self.assertEqual(self.run_hook(self.payload()), 'captured')
        sent = FakeServer.bodies[-1].decode()
        for leaked in ('opaque-one', 'opaque-two', 'opaque-three', 'opaque-four', 'op5', GHP,
                       '90817263', '55443322', '725000000', '31415926'):
            self.assertNotIn(leaked, sent)
        transcript = FakeServer.calls[-1][1]['params']['arguments']['transcript']
        first = json.loads(transcript.splitlines()[0])
        self.assertEqual(first['message']['usage'], {'output_tokens': 41, 'cache_read_input_tokens': 12},
                         'usage counters are not secrets')
        self.assertEqual(first['message']['content'][0]['input']['pin'], {'token': '[REDACTED:secret]'})
        self.assertEqual(first['message']['content'][0]['input']['user'], 'bob')

    def test_the_negotiated_protocol_version_is_used_or_refused(self):
        FakeServer.init_version = '2025-03-26'
        self.assertEqual(self.run_hook(self.payload()), 'captured')
        headers = {k.lower(): v for k, v in FakeServer.calls[-1][0].items()}
        self.assertEqual(headers.get('mcp-protocol-version'), '2025-03-26')
        FakeServer.init_version = '1999-01-01'
        with self.assertRaises(hook.CaptureError) as caught:
            self.run_hook(self.payload('PreCompact'))
        self.assertEqual(str(caught.exception), 'unsupported protocol version')
        self.assertNotEqual(FakeServer.calls[-1][1].get('method'), 'tools/call')

    def test_remote_metadata_carries_no_credentials(self):
        hook_git = hook.git
        remote = f'https://proxy:{GHP}@git.example/o/r?access_token=q1w2e3r4t5#frag-secret'

        def fake_git(cwd, deadline, *args):
            return remote if args[:2] == ('remote', 'get-url') else hook_git(cwd, deadline, *args)

        with mock.patch.object(hook, 'git', fake_git):
            self.assertEqual(self.run_hook(self.payload()), 'captured')
        args = FakeServer.calls[-1][1]['params']['arguments']
        self.assertEqual(args['git_remote'], 'https://git.example/o/r')
        sent = FakeServer.bodies[-1].decode()
        for leaked in (GHP, 'q1w2e3r4t5', 'frag-secret'):
            self.assertNotIn(leaked, sent)

    def test_a_redirect_is_not_followed(self):
        other = serve()
        try:
            FakeServer.redirect_to = f'http://127.0.0.1:{other.server_port}/elsewhere'
            with self.assertRaises(hook.CaptureError) as caught:
                self.run_hook(self.payload())
            self.assertEqual(str(caught.exception), 'http 302')
            self.assertEqual(len(FakeServer.calls), 1, 'the redirect target was never contacted')
        finally:
            other.shutdown()
            other.server_close()

    def test_an_insecure_or_credentialed_endpoint_is_refused_before_sending(self):
        for url in ('http://memory.example/mcp', 'ftp://memory.example/mcp',
                    f'https://u:{TOKEN}@memory.example/mcp', 'https://memory.example/mcp#x',
                    'https:///mcp', 'https://memory.example/m cp'):
            with self.assertRaises(hook.CaptureError) as caught:
                self.run_hook(self.payload(), env={**self.env, 'REMIND_ME_URL': url})
            self.assertEqual(str(caught.exception), 'invalid REMIND_ME_URL')
            self.assertNotIn(TOKEN, str(caught.exception))
        self.assertEqual(hook.endpoint('https://memory.example/mcp'), 'https://memory.example/mcp')
        self.assertEqual(FakeServer.calls, [])

    def test_errors_are_reported_without_tokens_or_server_text(self):
        FakeServer.fail_tool, FakeServer.tool_text = True, f'echo {TOKEN} private-body'
        err = self.stderr_of_main(self.env)
        self.assertEqual(err.strip(), 'remind-me-capture: tool error')
        bad_header = {**self.env, 'REMIND_ME_TOKEN': f'{TOKEN}\nX-Leak: 1'}
        err = self.stderr_of_main(bad_header)
        self.assertEqual(err.strip(), 'remind-me-capture: invalid REMIND_ME_TOKEN')
        self.assertNotIn(TOKEN, err)

    def test_an_oversized_response_is_refused(self):
        FakeServer.pad = 64
        with mock.patch.object(hook, 'MAX_RESPONSE_BYTES', 32):
            with self.assertRaises(hook.CaptureError) as caught:
                self.run_hook(self.payload())
        self.assertEqual(str(caught.exception), 'response too large')

    def test_an_expired_deadline_sends_nothing(self):
        expired = hook.Deadline(0)
        with self.assertRaises(hook.CaptureError) as caught:
            self.run_hook(self.payload(), deadline=expired)
        self.assertEqual(str(caught.exception), 'timeout')
        self.assertEqual(FakeServer.calls, [])


class Helpers(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_read_tail_reads_a_bounded_suffix_of_whole_lines(self):
        path = self.tmp / 't.jsonl'
        path.write_bytes(b'a' * 100 + b'\nbbbb\ncccc\n')
        self.assertEqual(hook.read_tail(path, 8), ['cccc', ''])
        self.assertEqual(hook.read_tail(path, 11), ['bbbb', 'cccc', ''])
        self.assertEqual(hook.read_tail(path, 1000)[0], 'a' * 100)

    def test_compose_keeps_whole_newest_lines(self):
        lines = ['aaaa', 'bbbb', 'cccc', '']
        self.assertEqual(hook.compose(lines, 100), 'aaaa\nbbbb\ncccc\n')
        self.assertEqual(hook.compose(lines, 8), 'cccc\n')

    def test_compose_counts_escaped_bytes_not_raw_ones(self):
        quotes = '"' * 10
        self.assertEqual(hook.escaped_len(quotes + '\n'), 22)
        self.assertEqual(hook.compose(['ok', quotes], 21), '')
        self.assertEqual(hook.compose([quotes, 'ok'], 21), 'ok\n')
        self.assertEqual(hook.escaped_len('é'), 2, 'UTF-8 is sent as is')

    def test_a_clean_line_is_sent_byte_for_byte(self):
        line = '{"b": 1,  "a": "x"}'
        self.assertEqual(hook.scrub_line(line), line)
        self.assertEqual(hook.scrub_line('not json password=hunter2hunter2'),
                         'not json password=[REDACTED:secret]')

    def test_redact_covers_the_server_shapes(self):
        jwt = 'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl'
        cases = {
            '-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----': 'private_key',
            jwt: 'jwt',
            'https://user:pa55word@host/x': 'url_credentials',
            'AKIAABCDEFGHIJKLMNOP': 'aws_access_key',
            GHP: 'github_token',
            'xoxb-1234567890-abc': 'slack_token',
            'token: abcdefgh12': 'secret',
        }
        for text, kind in cases.items():
            self.assertIn(f'[REDACTED:{kind}]', hook.redact(text), text)
        self.assertEqual(hook.redact('a token is fine'), 'a token is fine')

    def test_bare_remote_strips_credentials_query_and_fragment(self):
        self.assertEqual(hook.bare_remote('http://proxy:s3cret@127.0.0.1:9/git/o/r?t=1#f'),
                         'http://127.0.0.1:9/git/o/r')
        self.assertEqual(hook.bare_remote('git@github.com:o/r.git'), 'git@github.com:o/r.git')
        self.assertIsNone(hook.bare_remote(None))

    def test_reply_to_reads_complete_events_only(self):
        reply = {'jsonrpc': '2.0', 'id': 2, 'result': {}}
        self.assertEqual(hook.reply_to(2, json.dumps(reply), False), reply)
        text = json.dumps(reply)
        split = f'data: {text[:10]}\ndata:{text[10:]}\n\n'
        self.assertEqual(hook.reply_to(2, split, True), json.loads(text[:10] + '\n' + text[10:]))
        stream = f'data: {json.dumps({"id": 1})}\n\ndata: {text}\n'
        self.assertIsNone(hook.reply_to(2, stream, True), 'an unterminated event is dropped')
        self.assertIsNone(hook.reply_to(3, stream + '\n', True))


if __name__ == '__main__':
    unittest.main()
