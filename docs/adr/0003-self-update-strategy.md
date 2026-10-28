# ADR-0003: Self-update means "pull and rebuild," not "swap a binary"

Status: Accepted (amended by [ADR-0020](0020-moved-into-the-rusty-mill-monorepo.md): builds `-p rusty-remind-me -p remind_me_hub` rather than `--workspace`, and counts only commits touching this product)
Date: 2026-07-29

## Context

`#58` ports `remind_me_mcp/updater.py`, `remind_me_check_update`, and
`remind_me_self_update`. The issue is explicit that the reference's own
mechanism does not port directly: it runs `git pull --ff-only` followed by
`pip install -e .`, which works because the reference is an interpreted
package installed in editable mode — the same source tree it just pulled
*is* the running installation. A compiled Rust binary has no such identity
between "the source I pulled" and "the code currently executing"; the
issue calls out three plausible answers (check-only, rebuild-and-restart,
fetch-a-release-binary-and-swap) and asks that the choice be decided and
recorded before implementing, precisely because they differ so much in
risk and in what infrastructure they assume.

## Decision

**`remind_me_check_update` is read-only and ports directly**: `git fetch
origin --quiet`, then compare `HEAD` against `origin/main` by commit, exactly
mirroring `check_for_update()`. Nothing about this step depends on how the
running process was built or installed, so there is no compiled-vs-interpreted
gap to resolve here at all.

**`remind_me_self_update` means "pull the source, rebuild the workspace, tell
the operator to restart"** — `git pull --ff-only` (refusing a dirty tree
unless `force=true`, exactly like the reference), followed by `cargo build
--release --workspace` in place of `pip install -e .`, and reporting
`restart_required: true` unconditionally on success. This is the
"rebuild and require a restart" option the issue named as plausible, not
the "fetch a prebuilt release binary and swap it" option — that needs a
release pipeline (built artifacts published somewhere fetchable) this repo
does not have, and building one is a decision far larger than this issue,
were it ever wanted. It is also not the "check-only" option: the issue
frames that as the fallback if updating "doesn't turn out to be worth
porting," and rebuilding in place is little additional risk on top of the
git operations `remind_me_check_update` already needs, so there is no
reason to stop short of it.

Critically, **even the reference's own `remind_me_self_update` already
requires a restart** ("the MCP server should be restarted for changes to
take effect," per its own docstring) — a running Python process keeps
executing the module objects it already imported; `pip install -e .`
updates files on disk, not the interpreter's live state. So "rebuild,
then tell the operator to restart" is not a lesser answer forced by
compilation — it is the same restart requirement the reference already
has, just made explicit for a case (recompiling code) where it is even
more obviously unavoidable.

**`force` only bypasses the dirty-working-tree guard, exactly as the
reference does — never the `--ff-only` guard.** Verified against
`perform_update()` directly rather than assumed: `force` is checked
before the dirty-tree `git status --porcelain` call and nowhere near the
`git pull --ff-only` call, so a local commit history that has diverged
from `origin/main` still refuses to update, force or not. Silently
merging or rebasing local commits away under `force=true` would be a
materially more destructive operation than what the reference actually
implements, and the issue specifically asks this be confirmed rather than
assumed.

**Repository discovery walks up from the current working directory, not
from the running executable's own path.** The reference's `_find_repo_root`
walks up from `Path(__file__)` because an editable pip install's package
files live inside the repo it needs to find — that relationship is
structural for an interpreted, `-e`-installed package. A compiled binary
has no equivalent relationship: `cargo install` copies the executable
somewhere entirely outside any source tree, and even a `target/release/`
build's path tells you nothing reliable about which checkout produced it.
Self-update is only a coherent operation at all when invoked from inside
the repo it is meant to rebuild — there is no other source tree it could
mean — so this port requires exactly that: the operator runs
`remind_me_check_update`/`remind_me_self_update` from within the repo (or
a subdirectory of it), the same way `git status` or `cargo build` already
have to be. This is a real, stated behavioral difference, not a silent
approximation.

Verifying a candidate `.git` directory is actually *this* repository (not
some unrelated repo the walk happened to pass through, e.g. a nested
vendor checkout) uses the same intent as the reference's
`_is_remind_me_repo_root` (which reads `pyproject.toml`'s `[project].name`)
adapted to this repo's actual layout: it checks that the candidate's own
`Cargo.toml` mentions `crates/remind_me_core`, this workspace's own
distinctive member path, rather than parsing TOML with a new dependency
just for this one check.

## Alternatives considered

**Fetch a prebuilt release binary and swap the running executable.**
Rejected: no release pipeline publishes binaries for this project, and
building one is an infrastructure decision on its own, unrelated to
porting the reference's update-checking behavior. Revisitable later behind
the same tool names if a release pipeline is ever built.

**Check-only, with no `remind_me_self_update` at all.** Rejected: the
issue offers this as the fallback specifically if a real update path
"doesn't turn out to be worth porting." Rebuilding via `cargo build
--release --workspace` after a successful `git pull --ff-only` is a small
addition on top of the git plumbing `remind_me_check_update` already
needs, so declining it here would be giving up a well-scoped feature for
no corresponding reduction in risk.

**Attempt to replace the running process's own binary in place (exec into
the freshly-built one).** Rejected as needless risk for no real gain: the
reference doesn't attempt live process replacement either (it requires a
restart), and doing so from inside the same binary that is mid-request
handling a `tools/call` is a materially riskier operation than the
reference's own restart-required design for no corresponding benefit.

## Consequences

- `remind_me_self_update` takes measurably longer than the reference's own
  (a release-mode workspace rebuild vs. an editable pip install), and
  needs a build toolchain present on the machine running the MCP server —
  true of the reference too (rebuilding nothing there, but `pip install -e
  .` still assumes a working Python toolchain and network access to
  wherever its dependencies resolve from).
- A build failure after a successful `git pull` leaves the source tree
  ahead of the last known-good build. Exactly like the reference's own
  `pip install` failure path, this is rolled back automatically (`git
  reset --hard` to the pre-pull commit) when possible, and reported
  plainly with the manual recovery command when it is not.
- Self-update only works when invoked from inside the repository, not
  from wherever a packaged/installed binary happens to live — a real,
  documented divergence from the reference, driven by there being no
  compiled-binary equivalent of "the package's own file location is
  inside the repo."
