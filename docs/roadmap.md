# seedchamp roadmap

Open and deferred work only. Architecture: [design.md](design.md). Modules: [domains.md](domains.md).

## Non-goals

Do not implement unless explicitly requested:

| Item |
|------|
| PEX |
| DHT |
| WebUI / plugins / Windows parity |

Trackers and optional manual peers only.

## Open / deferred

| Item | Status | Notes |
|------|--------|--------|
| TrackerList promote | open | Failover / promote better trackers after announce outcomes |
| Upload slots / optimistic unchoke | deferred | Always unchoke until requested |
| magnet / ut_metadata | deferred | Not scheduled |

## Verify

```bash
cargo fmt --all
cargo build --release
cargo test
cargo run -- doctor
./bench/smoke.py   # after release build
```

## When to update

- Close or re-open a row above
- Explicitly request a non-goal
- Do not list finished work here
