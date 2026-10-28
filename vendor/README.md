# Vendored patch: aws-sigv4 1.6.0 (RUSTSEC-2026-0387)

A source patch to **aws-sigv4 1.6.0**, copied from rusty_mill's `vendor/`
(see its README for the full rationale and review notes). Cargo's
`[patch.crates-io]` does not carry through git dependencies, so this
repository applies it itself. Only `remind_me_core`'s `cloud-backup` feature
reaches aws-sigv4. Keep this copy identical to rusty_mill's; when rusty_mill
drops or changes the patch, do the same here.

```sh
python3 vendor/verify.py
cargo test --locked -p remind_me_core --features cloud-backup --lib cloud_backup::
```
