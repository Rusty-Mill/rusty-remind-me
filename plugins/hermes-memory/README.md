# remind-me as a Hermes memory provider

Gives each Hermes profile (JARVIS, the gateway, the dashboard) remind-me as
its memory (Mill Board #1552, ADR-0028). Standard library only; no packages
to install.

Per session it:

- opens the system prompt with `remind_me_brief` for the configured project,
  once (nothing when no project is set);
- before the model answers, adds the memories most relevant to each turn's
  text (`remind_me_brief` with `prompt`, the same Prompt mode as Claude
  Code's per-prompt hook);
- offers the model the tools remind-me lists for this client, as they are;
- captures the transcript once at session end. Cron jobs and subagents read
  memory but do not capture.

Nothing is written per turn: the model saves facts with `remind_me_add`.
When remind-me cannot be reached, recall is empty and the turn goes on; the
failure is logged.

## Set up

1. Copy this folder to `$HERMES_HOME/plugins/remind-me/` for each profile
   that should use it (`__init__.py`, `remind_me_client.py`, `plugin.yaml`).
2. In that profile's `config.yaml`:

   ```yaml
   memory:
     provider: remind-me
   ```

3. Choose a mode in `$HERMES_HOME/remind-me.json` (Hermes's setup writes it
   from `get_config_schema`; every key is optional):

   ```json
   { "project": "rusty_mill", "url": "", "command": "rusty-remind-me" }
   ```

   - **Local** (no `url`): runs `rusty-remind-me server` over stdio, so the
     binary must be on `PATH` or named in `command`.
   - **Remote**: set `url` to the connector's MCP URL (ending `/mcp`) and the
     token in `REMIND_ME_TOKEN`. `REMIND_ME_URL` overrides `url`. The token is
     never written to `remind-me.json`. For one capture per session, also
     set `REMIND_ME_CAPTURE_TOKEN` to the server's
     `REMIND_ME_REMOTE_CAPTURE_TOKEN` (ADR-0036); it too stays out of
     `remind-me.json`.

4. Register each profile in remind-me's `clients.json` (beside the store) so
   it gets its own client id and a small tool list:

   ```json
   { "hermes-jarvis": { "aliases": ["hermes-jarvis"], "profile": "minimal", "format": "json", "brief_budget": 600 } }
   ```

   The provider writes as `hermes-<profile>` (`hermes-jarvis` for the
   `jarvis` profile) and sends that name in the MCP handshake, which is what
   the alias matches. Unregistered, it still works, with the server's
   default profile and its writes counted as `unknown`.

## Capture

- Local: `rusty-remind-me capture-transcript --format messages --session-id
  ID --reason end` reads `[{role, text}]` on stdin. It is stored like a
  Claude Code hook capture, one per session; a second capture of the same
  session replaces the first.
- Either way the session is keyed `<client>:<hermes session id>`
  (`hermes-jarvis:abc`), so two profiles that reuse a session id keep their
  own captures (D139).
- Remote, capture token set: `remind_me_capture_transcript` with the
  conversation as `{role, content}` JSONL (its newest lines within the
  server's 3 MiB escaped limit) and the same session key, in its own MCP
  session with `REMIND_ME_CAPTURE_TOKEN`. One capture per session, as
  locally. Its rows carry `client = remind-me-capture-token`, not the Hermes
  client id (D67), so the bot shows only in the session key (D140).
- Remote, no capture token: `remind_me_auto_capture` with the conversation
  and a mechanical summary, tagged `hermes` and the client id. Each callback
  may write its own capture; this is the documented exception (D140).

Redaction is the server's, either way.

## Tests

```sh
python3 -m unittest discover -s plugins/hermes-memory/tests -v
```

Run by hand; not in CI (the #1552 design keeps Python out of CI). They run without Hermes, against a fake server. Set `REMIND_ME_TEST_BINARY`
to a built `rusty-remind-me` to also run a round trip against the real
server on a throwaway store.
