# ncom — improvement plan (post-review round 2)

Derived from the second full code review (2026-10-09). Items are grouped into
phases executed strictly in order, one fresh implementer subagent per phase
(per AGENTS.md). Every phase ends with `cargo build`,
`cargo clippy --all-targets -- -D warnings`, `cargo test` clean, and a
todo.txt entry (append-only). Baseline: 164 tests passing.

## Phase 1 — Correctness of the core view (H1, M5, M4)

1. **Pin the paused viewport** (`src/app.rs` `push_line`, `src/ui.rs` `visible_lines`)
   - Bug: `view_offset` is tail-relative; it must grow only when a new
     VISIBLE line is appended. Current code rebases by `evicted_visible`,
     so below the ring cap (or with a filter and non-matching arrivals) a
     paused viewport slides forward over content.
   - Fix: compute `appended_visible` (does the pushed line match the active
     filter? always true unfiltered) and, when `!follow_tail`,
     `view_offset = clamp_offset(view_offset + appended_visible)`. Drop the
     `evicted_visible` increment for the offset (KEEP the `last_match`
     eviction rebase — it is index-based, not offset-based). Keep the
     unconditional clamp.
   - Tests: paused below cap + arrivals → same content pinned; filter active,
     non-matching append (view unchanged) and matching append (offset +1);
     filter active, matching evict + non-matching append → no over-rebase;
     at-cap case must keep passing (regression).
   - Update the doc comments in `push_line` and `visible_lines` that state
     the old rule.

2. **Error freshness without equality check** (`src/command.rs` `execute`)
   - Bug: stale errors cleared by `app.last_error == err_before`; an
     identical repeated failure (e.g. `:send x` twice while port down)
     silently shows no error the second time.
   - Fix: set `app.last_error = None` at the start of `execute`; drop
     `err_before` and the tail comparison. Every failing arm sets its own
     error (verify while editing).
   - Test: same failing command twice → error visible both times.

3. **Clear `last_match` on filter changes** (`src/command.rs` Filter arm)
   - Bug: `last_match` is absolute without a filter and view-relative with
     one; toggling `:filter`/`filter off` leaves it in the wrong coordinate
     system → wrong `n`/`N` jumps (then eviction compounds it).
   - Fix: on any `filter_pattern` transition (None↔Some or pattern change),
     clear `last_match` (keep `last_search` so `/pat` Enter re-runs it —
     document).
   - Tests: search unfiltered → filter on → `n` re-runs search in view
     coords, no wrong jump; filter off likewise; `:noh` still clears both.

## Phase 2 — Worker robustness (M1, L1, L2, L9)

4. **Serve commands on every read outcome** (`src/serial.rs` `run`)
   - Bug: `serve_commands` runs only in the `TimedOut` branch; a device
     streaming without a ≥25 ms gap starves TX/reconfig/pin messages.
   - Fix: drain commands after `Ok(n)` sends data AND in the `Ok(0)` branch
     (also fixes the busy-spin). Watch the borrow structure the comment at
     the top of `run` describes.
   - Tests: extend the pty integration tests — stream continuously from the
     master while issuing `ToWorker::Tx`/`Config`; assert they are applied
     promptly (bounded time), not only after a gap.

5. **Abort pending sendfile on `:connect`/`:r`** (`src/command.rs` `connect`)
   - Bug: reconnecting while still connected mid-transfer continues the
     remaining chunks onto the NEW port, contradicting the `pump_sendfile`
     doc.
   - Fix: `connect()` takes `app.pending_sendfile`; if set, clear it and
     status `sendfile: aborted (reconnect)`.
   - Test: mid-transfer connect → pending cleared, no Tx to the new worker.

6. **Chunk accounting on failed send** (`src/app.rs` `pump_sendfile`)
   - Bug: `remaining`/`sent` update before `send_tx`; a failed channel send
     drops the chunk but counts it; final status overstates.
   - Fix: make `send_tx` (or the pump call) report success; on failure abort
     the transfer (clear pending, set error, no count) and only set the
     completion status after a successful final chunk.
   - Test: worker dies mid-pump → transfer aborted, status shows error, no
     false byte count.

## Phase 3 — Hex view & filter performance (M2, M3)

7. **Hex view: full line, multiple rows** (`src/ui.rs` `hexdump_parts`, `hex_body`)
   - Bug: only bytes 0–15 of each stored line (up to 64 KiB) render, with no
     overflow marker.
   - Fix: emit `ceil(len/16)` rows per line. Row offset is per-line relative
     (0000, 0010, …) — keep the "no absolute offsets" rationale. TS prefix
     applies to the first row only (document). Reassess viewport row budget:
     `visible_lines` selects lines, hex renders multiple rows per line —
     clamp so the LAST line's rows fill the viewport exactly (document the
     sizing rule).
   - Tests: long line renders all bytes across rows; 16-byte and 17-byte
     lines; empty line; alignment of ASCII gutter on all rows.

8. **Cache the filtered index** (`src/app.rs`)
   - Perf: `filtered_indices`/`view_len` do a full decode+scan of the 10k
     buffer per render/motion/arrival (four duplicated scan sites).
   - Fix: `filter_cache: Vec<usize>` in App. Invalidate on pattern change;
     update in `push_line` (append index if matching) and on eviction (drop
     prefix by evicted count — the same quantity `last_match` uses).
     `visible_lines`, `clamp_offset`, `find_matches`, and the eviction
     visibility check all read the cache. Keep behavior identical.
   - Tests: cache matches a freshly computed index across appends/evictions/
     pattern flips (property-style vs. the naive scan); perf smoke (10k
     lines, one render pass — assert bounded allocations or time budget);
     all existing filter tests unchanged.

## Phase 4 — Ports scan, pins, prompts, config polish (L3, L4, L5, L7, L8, L10, L11, L14)

9. `scan_ports`: `canonicalize(entry.path())` (the link itself) so udev's
   relative symlinks resolve — plus a test with a RELATIVE symlink target.
10. `set_pin`: after a successful reopen, re-apply the pin level; report
    failure if it still errors (`:dtr 0` must not silently leave DTR high).
11. Prompt/modifier hygiene: in `/` and `:` handlers reject `Char` events
    with CONTROL/ALT (matching `handle_insert_raw`); in NORMAL mode add
    explicit ctrl arms or gates so Ctrl-N/Ctrl-G/Ctrl-J/K stop acting as
    plain keys. Document the intended ctrl keys.
12. `reconfigure` error wording: `"{name}: not connected"` (currently bare).
13. Config: non-NotFound IO errors (e.g. permission denied) must WARN and
    fall back to defaults; join multiple config warnings on the status line
    (or cycle) instead of `.take(1)`.
14. `:w` guard via `OpenOptions::create_new(!force)` mapping `AlreadyExists`
    to the guard error (closes the TOCTOU); keep `w!` semantics.
15. Empty `:` + Enter: silently ignore (vim behavior) — skip parse in
    `execute_command` when trimmed input is empty.
16. Help entry `"databits 5|6|7|8  data bits"` (double space) so the name
    renders bold like the rest; width-fit test still passes.

## Phase 5 — Cleanup, docs, repo hygiene (L6, L12, L13, L15, L16)

17. Remove dead code: unreachable `else` in `render_prompt_line`, stale
    `#[allow(dead_code)]` on `Line.flags`, rename the stale hex test name
    (`…_with_ts_and_offset` → no-offset wording).
18. Final `app.drain()` before the quit `break` in `main.rs` so queued RX
    reaches the session log; test via logger+channel seam if practical.
19. README: fix the `fake-serial.sh` reference (file no longer exists) —
    rewrite the subsection around `scripts/fake-serial.py` (uv); mention
    musl builds need `ld.lld` on PATH if the absolute linker path is
    dropped (see 20).
20. `.cargo/config.toml`: drop the hardcoded absolute rustup linker path —
    use `linker = "ld.lld"` (PATH) and keep `--icf=all`; README notes the
    lld requirement for the musl target.
21. Spec doc: remove the stale hex "offset column" sentence (superseded by
    item 7); reconcile any wording the earlier phases changed.
22. One-shot `cargo fmt` across the crate (drift ~1,750 lines); commit as
    an isolated whitespace-only commit so the real diffs stay reviewable.
23. Commit the working tree at each phase boundary (source, scripts/ minus
    `.venv/`, README, spec doc — todo.txt/`.cargo` per the standing
    .gitignore, which stays untouched).

## Validation (final)

- Full suite green (164 + ~25–30 new tests), clippy `-D warnings`, clean
  release build (glibc) and musl static-pie build.
- Manual pass with `scripts/fake-serial.py --loop` on a pty: pause + stream
  (viewport must NOT slide), filter + stream, hex view of long lines,
  `:dtr`/`:rts` via `stty -F`, search + filter + `n`/`N`, `:noh`, help pages.
- Re-run a focused review pass on Phases 1–3 diffs (highest-risk changes).
