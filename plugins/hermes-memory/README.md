# remind-me as a Hermes memory provider

Gives each Hermes profile (JARVIS, the gateway, the dashboard) remind-me as
its memory (Mill Board #1552, ADR-0028). Standard library only; no packages
to install.

Per session it:

- opens the system prompt with `remind_me_brief` for the configured project,
  once (nothing when no project is set);
- searches remind-me with each turn's text before the model answers
  (`remind_me_search`, compact, `prefetch_budget` tokens, default 600);
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
   { "project": "rusty_mill", "url": "", "command": "rusty-remind-me", "prefetch_budget": 600 }
   ```

   - **Local** (no `url`): runs `rusty-remind-me server` over stdio, so the
     binary must be on `PATH` or named in `command`.
   - **Remote**: set `url` to the connector's MCP URL (ending `/mcp`) and the
     token in `REMIND_ME_TOKEN`. `REMIND_ME_URL` overrides `url`. The token is
     never written to `remind-me.json`.

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
- Remote: `remind_me_auto_capture` with the conversation and a mechanical
  summary, tagged `hermes` and the client id.

Redaction is the server's, either way.

## Tests

```sh
python3 -m unittest discover -s plugins/hermes-memory/tests -v
```

They run without Hermes, against a fake server. Set `REMIND_ME_TEST_BINARY`
to a built `rusty-remind-me` to also run a round trip against the real
server on a throwaway store.
