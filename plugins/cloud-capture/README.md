# remind-me cloud capture (private plugin template)

Saves cloud Claude Code sessions, such as claude.ai Projects and Claude Duet
chats, to remind-me. Those sessions have no environment variables or setup
script of yours, and they skip plugin hooks that call a connector, so this
plugin's hook runs a small script that sends the transcript itself
(ADR-0029, ADR-0036).

It carries a **capture-only** token: one that can save a session transcript
and nothing else. Never put the full connector token in it, and never publish
it. It is not listed in this repository's marketplace.

## Set up

1. Make a capture-only token (PowerShell 7 shown) and set it as
   `REMIND_ME_REMOTE_CAPTURE_TOKEN` in the remote connector's environment,
   then restart the connector. It must be at least 32 visible characters and
   differ from the connector token, or the server leaves capture-only access
   closed.

   ```powershell
   [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
   ```

   No tool profile change is needed: the capture-only caller reaches
   `remind_me_capture_transcript` under any profile, `core` included.
   `remind_me_server_status` reports `remote.capture_token: ready` once it is set.
2. Copy `config.example.json` to `config.json` in this folder and fill in the
   connector URL (ending in `/mcp`) and the capture-only token. `config.json`
   is ignored by git.
3. Zip this folder and upload it to claude.ai as a private plugin for your
   organization.

Captures it makes are stored with the client name `remind-me-capture-token`,
so they can be told apart from the model's own.

## What it does not do

- It runs only when `CLAUDE_CODE_REMOTE=true`, so it never doubles up with the
  main plugin's hook in a local session.
- Anything a capture-only token is sent with other than opening a session or
  calling `remind_me_capture_transcript` is refused by the server (403).
- Anyone who can read `config.json` can write transcripts into remind-me. If it
  leaks, change `REMIND_ME_REMOTE_CAPTURE_TOKEN` on the server.

## Test

```powershell
python -m unittest discover -s plugins/cloud-capture/tests -v
```
