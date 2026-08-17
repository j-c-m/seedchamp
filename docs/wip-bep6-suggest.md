# WIP: BEP 6 Suggest Piece

Temporary agent playbook. Delete this file when Suggest recv is honored in the picker (and send is either implemented or explicitly declined). Do not add a row to [roadmap.md](roadmap.md).

Related: [design.md](design.md) §5. Dead-path cleanup left these symbols in place.

---

## Agent contract

1. Grep callers before changing Fast helpers. Do not break Have All/None, Reject Request, or Allowed Fast.
2. Suggest send is **optional** in BEP 6. Recv honor is the required work unless the user asks for send.
3. Comments document the function, not history.
4. Verify: `cargo test -p seedchamp-engine --lib wire:: hot:: peer::` then `cargo test`. Smoke after a picker change if you have time (`./bench/smoke.py` after release build). Rasterbar cells exercise Fast but not Suggest specifically.
5. Delete this file when the work is done.

---

## What BEP 6 already does

Live today — **do not change** unless a bug is in the same hunk:

- Handshake reserved bit (Fast).
- Have All / Have None instead of a full bitfield when Fast is mutual.
- Reject Request on failed fill (`OutQueue::push_reject`).
- Allowed Fast send (`encode_allowed_fast_messages`, IPv4 set) and recv (`PeerDownload.allowed_fast` in `piece_ok` while choked).

---

## What Suggest does today

| Piece | Where | Behavior |
|-------|--------|----------|
| Parse | `wire/messages.rs` `MSG_SUGGEST` | `Message::SuggestPiece` |
| Recv | `peer/established.rs` `SuggestPiece` arm | `fast.on_suggest(i)` if downloading |
| Store | `wire/fast.rs` `FastSession::suggested` | Cap 32, FIFO |
| Encode send | `encode_suggest_messages` | **No caller** |
| Picker | `peer/download.rs` → `HotTorrent::pick_rarest_piece` | **Never reads** `suggested` |

BEP 6: Suggest is advisory. The receiver **may** request that piece. Send is optional.

---

## Goal (recv)

Honor incoming Suggest when choosing the next piece to request.

1. One store, not two. Prefer `PeerDownload.suggested` (same pattern as `allowed_fast`) and call it from the Suggest arm. Drop or slim `FastSession::suggested` if it becomes unused.
2. In `PeerDownload` request fill (`download.rs`, `take_requests` / `pick_rarest_piece`), try suggested indices **first** if they pass the same gates as rarest-first: wanted, we do not have, peer has, not hashing/staging, `try_claim`, Allowed Fast if choked (`may`).
3. Drop an index from `suggested` when we have it, no longer want it, or the peer no longer has it.
4. Keep parsing Suggest so we do not disconnect.
5. Unit test: a suggested eligible piece is preferred over a rarer non-suggested one (`hot/tests.rs` and/or download tests).

Do **not** request a suggested piece that fails those gates.

## Send (optional)

Do not invent a super-seed policy unless asked. `encode_suggest_messages` can stay unused until there is a real hint policy (e.g. seed-while-leech pieces we just verified). Spec does not require send.

## Keep

- `generate_allowed_fast_set`, `encode_allowed_fast_messages`, `ALLOWED_FAST_K`
- Have All/None, Reject
- `peer_allows_while_choked` / `we_allow_while_choking` only if you wire them; otherwise they may stay unused

## Docs when done

Update design §5: “Suggest recv — honored in picker.” Delete this file.

## Out of scope

- Changing Allowed Fast set size or IPv6
- Upload slots / optimistic unchoke ([roadmap.md](roadmap.md))
- `MAX_REQUEST_LENGTH` vs 16 KiB
