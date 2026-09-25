# ADR-0020: Moved into the Rusty Mill monorepo

Status: Accepted
Date: 2026-09-24

Amends [ADR-0003](0003-self-update-strategy.md) (self-update). The
workspace-level half of this decision (tags, release workflow, marketplace,
CI) is the monorepo's own `docs/adr/0004-release-products-from-the-monorepo.md`.

## Context

This product was imported, with full history, into `Rusty-Mill/rusty_mill`
at `crates/apps/rusty_remind_me` via `git subtree`, as every other crate
there was (the monorepo's ADR-0001). Development and releases now happen
there; `baileyrd/rusty_remind_me` is frozen at v0.2.0.

Several things in this product assumed the repository root *was* this
product, and were wrong the moment it wasn't:

- `remind_me_self_update` rebuilt with `cargo build --release --workspace`.
  At the monorepo root that is ~240 crates, several needing system libraries
  (WebKitGTK, ALSA, protoc) this product never did.
- `remind_me_check_update` counted `HEAD..origin/main`. In the monorepo,
  `main` moves for every crate, so the "update available" notice would be
  on permanently, for commits that change nothing here.
- The version lived in `[workspace.package]`, which the monorepo already
  uses for other crates (0.1.0, a different license).
- `rust-toolchain.toml` pinned 1.97.0. Nested in a subdirectory, rustup
  would honour it only when cargo runs from this directory, so one checkout
  would build with two compilers depending on the shell's cwd.

The monorepo's single `Cargo.lock` also moved two dependencies: `rusqlite`
0.31 → 0.39 (it declares `links = "sqlite3"`, so one version per graph) and
`rmcp` 3.0.1 → 3.1.4 (the rest of the workspace requires `^3.1`, and Cargo
keeps one version per semver-compatible range).

## Decision

- **Self-update recognises both layouts.** `updater::product_dir` returns
  `crates/apps/rusty_remind_me` in the monorepo and `.` in an old standalone
  clone (so that clone still reports accurately rather than failing to find
  itself). "Commits behind" and the listed messages are scoped to that
  directory with a pathspec. A root `Cargo.lock` bump alone is deliberately
  not counted: it is picked up by the next update that touches this product.
- **Self-update rebuilds `-p rusty-remind-me -p remind_me_hub`**
  (`updater::BUILD_PACKAGES`), the two binaries a release ships, in either
  layout. ADR-0003's pull/build/rollback semantics are otherwise unchanged,
  including `git reset --hard` on a failed build, which now resets the
  whole monorepo checkout. That was always the whole checkout; it is just a
  bigger one now.
- **Each crate carries a literal `version`**, kept in lockstep by
  `scripts/get_workspace_version.sh`, which now fails when the six disagree.
  Inheritance used to guarantee that for free.
- **No toolchain pin.** CI builds with stable, like the rest of the
  monorepo. The floor stays 1.94.
- **Adopt the dependency moves rather than fight them.** The one `rusqlite`
  break was `usize` losing `ToSql` in 0.32 (`recalibrate.rs`'s `LIMIT`, now
  a saturating `i64`). The `rmcp` 3.1 change is spec enforcement: a
  session-free `2026-07-28` request must carry `_meta` with
  `io.modelcontextprotocol/protocolVersion` and `clientCapabilities`, or it
  gets `-32602`. A conformant client already sends both, so the test
  fixtures that didn't were fixed, not the server.
- **v0.2.1 is the cutover release.** The binaries differ from v0.2.0 (the
  dependency moves and the updater change), so they must not ship as 0.2.0
  again. v0.2.1 is also the first `rusty-remind-me-v*` tag the monorepo
  publishes.

## Consequences

- A standalone clone's `remind_me_check_update` stops finding updates,
  because `baileyrd/rusty_remind_me`'s `main` no longer moves. That
  repository's README says where development went. Nothing in a frozen
  clone can announce the move itself.
- A `2026-07-28` client that omits the per-request `_meta` gets a JSON-RPC
  error from `remind_me_remote` where v0.2.0 answered it. That is what the
  spec requires; legacy (session / pre-`2026-07-28`) requests are
  unaffected.
- This product's `.github/workflows/` are dead (GitHub reads workflows only
  at the repository root). They stay for history, like every other imported
  crate's, and the live versions are the monorepo's
  `remind-me-release.yml`, `remind-me-checks.yml` and `ci.yml`'s
  `remind-me*` jobs.
