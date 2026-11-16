# CLAUDE.md

Guidance for Claude Code sessions (and any agents they dispatch) working in this repo.

Read `docs/handoff/DOMAIN.md` at the start of every session here.

## Project experts
Before asking Nano any design, scope or behaviour question, ask the
expert for the paths involved. Never ask Nano first.

| Paths | Expert |
|---|---|
| `**` | `rusty-remind-me-expert` |

Call it with `subagent_type: "<expert>"`. **Fallback:** if that type isn't
available (cloud sessions can fail to load agents), read
`.claude/agents/<expert>.md` fresh at call time, spawn a general-purpose Agent
whose prompt is the file's body (everything after the frontmatter) followed by
the question. That counts as the expert. Never answer without it, and never
copy the file's text anywhere.
- ANSWER: act on it; the source is cited.
- ESCALATE: ask Nano exactly that block: question, options,
  recommendation. Nothing more.
- When Nano answers, pass the answer to the expert to record.
  Don't write DECISIONS.md or remind-me yourself; one writer keeps each
  answer recorded exactly once.
- Non-blocking default? Take the expert's default, keep going, and add the
  question to the pack's OPEN_QUESTIONS.md. An ESCALATE is never acted on
  before Nano answers, whatever its RECOMMEND says: leave that item pending
  and continue unrelated work.

## Commit signing: retry once, then commit unsigned and say so

This environment signs commits via a helper at `/tmp/code-sign` (a symlink to
the session's `environment-manager`, using an SSH signing key configured via
`gpg.ssh.program`/`gpg.format`/`user.signingkey` in the *global* git config —
nothing about this is set at the repo level). That symlink goes missing when
the container's `/tmp` is cleared, which makes `git commit` fail with:

```
fatal: cannot exec '/tmp/code-sign': No such file or directory
fatal: failed to write commit object
```

When any Claude Code session or dispatched agent hits this:

1. **Wait ~15–30 seconds and retry the commit once.** Sometimes the helper is
   only briefly absent around a container/session boundary and the retry
   succeeds cleanly.
2. **If the retry also fails, commit unsigned** (`--no-gpg-sign`) and state
   plainly in the commit message *and* in the reply that it is unsigned and
   why. Do not stall the task waiting for a human.
3. **Never bypass any other hook or check on your own initiative**, and never
   pass `--no-verify`. This exception is specific to commit signing when the
   helper is genuinely unreachable — it is not licence to skip tests, lint, or
   pre-commit hooks under time pressure.
4. **Do not silently paper over it.** An unsigned commit that does not say it
   is unsigned is worse than a blocked one, because the history then misreports
   its own provenance. If you claim a commit is unsigned, check first — verify
   with `git cat-file commit HEAD` and look for a `gpgsig` header rather than
   `git log --format=%G?`, whose `N` can mean "cannot verify" (no
   `gpg.ssh.allowedSignersFile`) rather than "not signed".

This supersedes the previous version of this section, which told agents to stop
and report instead. That rule cost a full work session: the helper stayed
missing for the rest of the run, and a completed, tested change sat uncommitted
across repeated automated nudges waiting for a human who had already moved on.
Blocking is the wrong default when the failure is environmental and the fix is
one clearly-labelled flag.
