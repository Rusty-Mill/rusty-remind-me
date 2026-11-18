# Open questions — rusty-remind-me
Claude Code: append here instead of stopping when a question isn't blocking.
Format: `- [ ] Q<n> (<date>, blocking? y/n): <question> — interim choice: <what you did>`
Resolved items are checked and point to the D#/QA entry that answers them.
Programme questions (order, gates, R1 to R9) stay in rusty_mill
`docs/projects/millwright/QUESTIONS.md`; they are not copied here.

- [x] Q1 → D12. (2026-11-16, n): Several ADRs say the owner decided or approved them, but the harvest found no quote of Nano's words: ADR-0024 item 5 (revisions deleted with a memory), ADR-0025 (engine is the only store), ADR-0027 ("Approve as written", card #57 notes), ADR-0030 (`brief` in core "at the owner's request"), ADR-0032 (merged "with Nano's approval", `mem_5d6b02b2d6894beaa20101ef8f199872`), ADR-0035 ("confirmed by Nano"). Can each be confirmed with a quote? — interim choice: all are [default].
- [x] Q2 → D13. (2026-11-16, n): Quote for D5 (dialogs immutable and append-only)? Only model paraphrases were found. — interim choice: D5 [default].
- [x] Q3 → D14. (2026-11-16, n): Quote for D6 (pure Rust and no third-party database are preferences)? — interim choice: D6 [default].
- [x] Q4 → D15. (2026-11-16, n): Quote for D7 (Luhn-passing number lists stay redacted, card #717)? — interim choice: D7 [default].
- [x] Q5 → D16. (2026-11-16, n): Quote for D8 (wiki compiles are manual, from the dashboard)? — interim choice: D8 [default].
- [ ] Q6 (2026-11-16, n): Conflict: README.md "Rusty Mill Ecosystem" says no `rusty_*` crates are dependencies, but the README header, the root `Cargo.toml` and ARCHITECTURE.md § 2 (`remind_me_remote` on `rusty_tokio`, `rusty_axum`, `rusty_mcp_server`) say monorepo crates are git dependencies pinned to a `libs-*` tag. Code wins; README needs a fix. — interim choice: follow Cargo.toml.
- [ ] Q7 (2026-11-16, n): Nano asked (2026-11-02) for memories to be tied to an active project and filtered on it (`mem_709f3ff47bdb474f8520726ed6a27c10`, open action item). Is ADR-0027's project model the answer, or is more wanted? — interim choice: none; not built from this pack.
- [ ] Q8 (2026-11-16, n): The repo has no AGENTS.md, PR template or change-fragment folder (repo-config audit gaps, not filled by this harvest). Add them? — interim choice: left as is; RELEASE_NOTES.md stays the per-PR log.
- [x] Q9 → D11; DOMAIN.md added in PR #400. (2026-11-16, n): `docs/handoff/DOMAIN.md` is missing from the tree. CLAUDE.md says to read it every session, SPEC.md's index and D9 name it, and Nano's hard-line rule (D11) was said about it. Should it be written, and if so, who writes it? — interim choice: not created; D11 recorded from the relayed words.
- [x] Q10 → D22 (2026-11-18, n; blocks only the MCP part of card #1565): Adding an MCP tool is a public API change (D3, D17). Should the decisions-log export be a new tool `remind_me_export`, or a `template` argument on the existing `remind_me_export_memories` (admin group), which ADR-0030 §4 would favour over a second export tool that looks like it? Either way it stays out of core (ADR-0030, D12). — interim choice: build the renderer and the CLI (D18 to D21); add no MCP surface until Nano answers.
