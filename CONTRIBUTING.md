# Contributing Guidelines

Thank you for contributing to `rusty_remind_me`! This document outlines our development workflows, coding standards, and testing procedures.

---

## 1. Development Setup

### Prerequisites
- **Rust Toolchain**: Rust 1.94+ with Cargo (a demonstrated floor, not a bisected minimum). CI builds with current stable, like the rest of the Rusty Mill monorepo; the `rust-toolchain.toml` pin this product had as a standalone repo did not come across (a nested one would only apply when cargo runs from this directory, giving two toolchains in one checkout).

### Local Workspace Setup
This product lives in the Rusty Mill monorepo at `crates/apps/rusty_remind_me`. Run cargo from the monorepo root and always select this product's crates with `-p` — `--workspace` there means every Rusty Mill crate.

```bash
# Clone the repository
git clone https://github.com/Rusty-Mill/rusty_mill
cd rusty_mill

# Validate cargo dependencies and path resolution
cargo check -p remind_me_core -p remind_me_mcp -p remind_me_api -p rusty-remind-me -p remind_me_remote -p remind_me_hub

# Run tests
cargo test -p remind_me_core -p remind_me_mcp -p remind_me_api -p rusty-remind-me -p remind_me_remote -p remind_me_hub
```

---

## 2. Code Quality & Standards

We enforce strict Rust idiom code quality across the workspace:

### Formatting & Linting
Run the following commands before submitting code:

```bash
# Format code according to Rust standard style
cargo fmt --all

# Run Clippy lints
cargo clippy -p remind_me_core -p remind_me_mcp -p remind_me_api -p rusty-remind-me -p remind_me_remote -p remind_me_hub --all-targets -- -D warnings
```

### Key Coding Conventions
1. **Zero Warnings**: All code should compile cleanly without warnings.
2. **Error Handling**: Use explicit `thiserror` and `Result<T, E>` types instead of `.unwrap()` or `.expect()` in non-test code.
3. **Thread Safety**: Any database access across threads must acquire a lock via `db.conn()` (a `parking_lot::Mutex<Connection>` guard — see `ARCHITECTURE.md` §6). Most background work in this workspace is plain OS threads (`std::thread::Builder::spawn`), not `tokio` tasks; `remind_me_remote` is the one crate that runs on `tokio`.
4. **Preserve Comments & Docstrings**: Maintain architectural comments explaining mathematical formulas (e.g. ACT-R decay and RRF scoring).

---

## 3. Testing Standards

Every feature or bug fix must be accompanied by automated unit or integration tests.

### Running Test Suites
```bash
# Run unit tests across all six crates
cargo test -p remind_me_core -p remind_me_mcp -p remind_me_api -p rusty-remind-me -p remind_me_remote -p remind_me_hub

# Run a specific test by name
cargo test test_database_creation_and_add_memory

# Run tests with output printed
cargo test -p remind_me_core -- --nocapture
```

### Test Locations
- **Unit Tests**: Placed inside module files within `src/` (e.g., `vitality.rs`, `retrieval.rs`) under `#[cfg(test)]`.
- **Integration Tests**: Placed in crate `tests/` directories (e.g., `crates/remind_me_core/tests/db_test.rs`).

### Changing environment variables in tests
Use `crate::test_env::set_var` / `remove_var` (`crates/remind_me_core/src/test_env.rs`), never `std::env::set_var` / `remove_var`; `clippy.toml` rejects the `std` pair. Tests run on parallel threads, and a raw write can segfault the whole test binary if it lands while SQLite is reading the environment during its first-connection setup. The helper finishes that setup before writing. An integration test binary declares the helper with `#[path = "../src/test_env.rs"] mod test_env;` (`../../remind_me_core/src/test_env.rs` from another crate). A test that depends on a variable's *value* still holds its file's env lock.

---

## 4. The Rusty Mill Ecosystem — Siblings, Not Dependencies (Yet)

Every crate in this product once listed the `rusty_*` "Rusty Mill" crates
(`rusty_tokio`, `rusty-db`, `rusty_json`, `rusty-search`, `rusty_http`,
`rusty_lines`, `rusty_term`, `rusty_time`, `rusty_config`) as
`../Rusty_Mill/...` path dependencies against a monorepo that did not exist
at those paths — the workspace failed to load, and not one source file
actually called into any of them. They were removed.

That monorepo now exists, and this product lives in it: those crates are
workspace siblings under the monorepo's `crates/`. **Still do not add a
`rusty_*` crate speculatively.** If a real call site needs a capability one
of them provides, depend on it as a workspace path dependency (its
`[workspace.dependencies]` entry in the root `Cargo.toml`, via
`crate.workspace = true`) — never as a git dependency, which the monorepo's
dependency-policy CI job rejects for any crate that is also a workspace
member (root `docs/adr/0002-dependency-sovereignty-policy.md`). The layer
rule applies too: this product is in `apps/`, so it may depend on
`foundation/`, `platform/` and `libs/` crates, but nothing may depend on it
(root `docs/adr/0003-workspace-layout-by-layer.md`).

---

## 5. Pull Request Checklist

Before submitting a Pull Request:
- [ ] `cargo check` (with the six `-p` flags above) compiles cleanly.
- [ ] `cargo test` (with the six `-p` flags above) passes all unit and integration tests.
- [ ] `cargo fmt --all` formats all code files.
- [ ] Documentation in `README.md` and `ARCHITECTURE.md` is updated if API signatures or CLI subcommands changed.
