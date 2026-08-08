- Update `docs/design.md` / `docs/roadmap.md` when decisions change.
- run `cargo fmt --all`. Do not commit unformatted Rust. Prefer a fmt-only commit only when reformatting a large pre-existing drift.

## Build

```bash
cargo fmt --all
cargo build
cargo test
cargo run -- doctor
```
