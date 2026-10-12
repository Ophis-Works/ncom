//! App state machine: mode, scrollback buffer, view offset, config status.
//!
//! Scrollback is a ring buffer of raw lines (`Vec<u8>` + arrival wall-clock
//! timestamp + flags); rendering decodes them. RX flows in via
//! [`App::feed_rx`], which runs the [`LineSplitter`]: bytes append to a
//! partial line until `\n` or the 100 ms idle timeout flush.

use chrono::{DateTime, Local};
use std::collections::VecDeque;
use std::fs::File;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::config::{Config, Theme};
use crate::logger::Logger;
use crate::serial::{
    FromWorker, LINE_IDLE_FLUSH, LineSplitter, SerialConfig, ToWorker, TxNewline,
    decode_lossy_caret,
};

/// Max scrollback lines kept in memory (spec default: 10 000).
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Line-mode input history cap (spec: "in-memory, newest 100").
pub const LINE_HISTORY_CAP: usize = 100;

/// How long an informational status message stays visible before it
/// auto-expires via [`App::tick_status`] (main.rs calls this every event-loop
/// iteration, ~25 ms apart). One TTL applies to ALL status messages —
/// including the `:ports` listing, for consistency: 5 s is enough to read a
/// one-line feedback message, and a longer listing can simply be re-run with
/// `:ports`. `last_error` NEVER auto-expires.
pub const STATUS_TTL: Duration = Duration::from_secs(5);

/// Hard size cap for `:sendfile` (16 MiB): a terminal is not an FTP client —
/// larger transfers need flow control the app does not have yet, and an
/// accidental `:sendfile /dev/zero`-sized path must fail before anything is
/// queued to the worker.
pub const SENDFILE_MAX: u64 = 16 * 1024 * 1024;

/// Bytes of a pending `:sendfile` queued per main-loop iteration
/// ([`App::pump_sendfile`]): small enough that the UI keeps redrawing between
/// chunks, large enough that a 16 MiB transfer finishes in ~4096 iterations.
pub const SENDFILE_CHUNK: usize = 4096;

/// In-progress `:sendfile` transfer, drained one chunk per main-loop iteration
/// by [`App::pump_sendfile`] so a large file never blocks the UI. `sent`
/// accumulates the total for the completion status; bytes already queued stay
/// queued — there is no unsend.
pub struct PendingSendFile {
    pub file: File,
    pub remaining: u64,
    pub sent: u64,
    pub path: String,
}

/// Help popup content (`:help` / `?`): one `:` command per line with a short
/// description. Command names are padded to a common 26-char column so all
/// descriptions align, and lines stay at or under 62 chars so the box fits
/// its 64-column width; `ui.rs::help_line_spans` bolds the leading name.
/// The parity/stopbits entries note the serialport 4.10.1 limits (mark/space
/// and 1.5 are rejected at parse/apply time — see command.rs module docs).
pub const HELP_LINES: &[&str] = &[
    "q                         quit (q! forces)",
    "q!                        force quit",
    "w <file>                  dump scrollback (no overwrite)",
    "w! <file>                 dump scrollback, overwrite",
    "log <path>|off            session log on/off",
    "hex on|off                hex view",
    "ts on|off                 timestamps",
    "line on|off               line-mode input",
    "echo on|off               show sent TX lines",
    "txnl cr|lf|crlf           tx line ending",
    "baud [<n>]                show/set baud",
    "databits 5|6|7|8          data bits",
    "parity none|even|odd      no mark/space",
    "stopbits 1|2              no 1.5",
    "flow none|xonz|rtscts     flow control",
    "dtr 0|1                   DTR level",
    "rts 0|1                   RTS level",
    "hl <pat>|off              highlight matches",
    "filter <pat>|off          show only matching",
    "noh                       clear / search highlight",
    "ports                     list devices",
    "connect|c <dev> [<baud>]  open serial port",
    "r                         reconnect",
    "send <text>               send + txnl ending",
    "sendhex <hex>             send raw bytes",
    "sendfile <path>           send raw file",
    "help                      this help",
];

/// COMMAND MODE (vi-like NORMAL mode) keys, one per line, same width budget
/// as [`HELP_LINES`] (<= 62 chars, name split off by 2+ spaces). Names share
/// the page-2 common column of 17 chars (longest name `ctrl-d / ctrl-u` is
/// 15 + a 2-space separator) so descriptions align with the `esc` entry of
/// [`HELP_SERIAL_LINES`]. Accurate against `handle_normal` in input.rs.
pub const HELP_KEY_LINES: &[&str] = &[
    "i                enter serial mode",
    ":                command line",
    "/pattern         search forward",
    "n / N            next / previous match",
    "j / k            scroll down / up",
    "ctrl-d / ctrl-u  half page down / up",
    "ctrl-f / ctrl-b  page down / up",
    "gg / G           top / end + follow",
    "F or p           pause / resume follow",
    "ZZ               quit (same rules as :q)",
    "?                this help",
    "ctrl-c           quit from any mode",
];

/// SERIAL MODE (INSERT) summary lines, same width budget as [`HELP_LINES`].
/// All keyed entries (`:line on`, `ctrl-u`, `ctrl-w`, `up/down`, `esc`,
/// `:line off`) use the page-2 common column of 17 chars so their
/// descriptions align with [`HELP_KEY_LINES`]; a description
/// too long for the remaining 45 columns wraps onto a continuation line
/// indented to that same column. The lead-in prose has no name separator and
/// stays FLUSH-LEFT on purpose (documented choice): it is a wrapped body
/// paragraph, not a key entry — indenting it to the description column would
/// imply a missing key name. `ui.rs::help_line_spans` renders prose and
/// continuation lines (a separator run with no name before it) plain.
/// Accurate against `handle_insert_line` in input.rs.
pub const HELP_SERIAL_LINES: &[&str] = &[
    "keys are sent to the port as typed",
    ":line on         switch on line editing",
    "                 allows full line editing with history",
    "                 sending full line when enter is pressed",
    "ctrl-u           clears line",
    "ctrl-w           deletes a word",
    "up/down          walk up/down the history",
    "esc              back to command mode",
    ":line off        switch back to direct serial mode",
];

/// Section headers rendered between the three help sections, in order.
pub const HELP_SECTION_HEADERS: [(&str, &[&str]); 3] = [
    (" commands:", HELP_LINES),
    (" command mode keys:", HELP_KEY_LINES),
    (" serial mode:", HELP_SERIAL_LINES),
];

/// Blank separator lines inserted before every section header AFTER the first
/// one on a page, so ` serial mode:` on page 2 sits two empty rows below the
/// keys section. Chained into [`help_page_lines`] (not rendered separately)
/// so the popup height math counts them automatically; `ui.rs::
/// help_line_spans` renders each empty line as one plain empty span.
pub const HELP_SECTION_SPACERS: &[&str] = &["", ""];

/// Number of help popup pages: page 1 is the ` commands:` section, page 2
/// the ` command mode keys:` + ` serial mode:` sections.
pub const HELP_PAGE_COUNT: usize = 2;

/// One help popup page: header + content lines for the sections on that page
/// (page 0 = commands, page 1 = command mode keys + serial mode), with
/// [`HELP_SECTION_SPACERS`] before each section after the first. The popup
/// height derives from this; `page` is clamped into `0..HELP_PAGE_COUNT`.
pub fn help_page_lines(page: usize) -> impl Iterator<Item = &'static str> {
    let page = page.min(HELP_PAGE_COUNT - 1);
    let sections: &[(&str, &[&str])] = if page == 0 {
        &HELP_SECTION_HEADERS[..1]
    } else {
        &HELP_SECTION_HEADERS[1..]
    };
    sections
        .iter()
        .enumerate()
        .flat_map(|(i, (header, lines))| {
            let spacers: &'static [&'static str] = if i == 0 { &[] } else { HELP_SECTION_SPACERS };
            spacers
                .iter()
                .copied()
                .chain(std::iter::once(*header))
                .chain(lines.iter().copied())
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
}

/// Per-line flags (extensible; search-match highlighting etc. later tasks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineFlags {
    /// Line was cut by the [`crate::serial::MAX_PARTIAL_LINE`] cap (or is the
    /// tail of such a line) — the renderer appends a dimmed `…`.
    pub truncated: bool,
    /// Line is local echo (`:echo on`): bytes successfully queued to the port
    /// by [`App::send_tx`], pushed as their own scrollback line. Stored bytes
    /// stay RAW (no `> ` prefix) so `:w` dumps and the log stay byte-exact;
    /// the `> ` prefix is added by the TEXT renderer only (ui.rs), which also
    /// paints the whole line with `theme.tx`. Hex view renders TX lines like
    /// any line (neutral, no prefix).
    pub tx: bool,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub bytes: Vec<u8>,
    /// Wall-clock arrival/send time, for `:ts on` prefixes.
    pub ts: SystemTime,
    pub flags: LineFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertStyle {
    /// Raw keystroke passthrough (`:line off`, default).
    Raw,
    /// Local input line with editing + history (`:line on`, task 6).
    Line,
}

/// Pending two-key NORMAL sequence (vi semantics: `g`/`Z` wait for a second
/// key; any other key cancels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKey {
    G,
    Z,
}

pub struct App {
    pub mode: Mode,
    pub insert_style: InsertStyle,
    pub txnl: TxNewline,
    pub scrollback: VecDeque<Line>,
    pub scrollback_cap: usize,
    /// Lines back from the tail the view is scrolled (0 = newest).
    pub view_offset: usize,
    /// Follow tail: rendering shows the newest lines; RX still appends.
    pub follow_tail: bool,
    pub ts_on: bool,
    pub hex_on: bool,
    /// Local echo (`:echo on|off`, default off): when on, every successful TX
    /// send ([`App::send_tx`], including each `:sendfile` chunk) also pushes
    /// its bytes into the scrollback as a `LineFlags::tx` line. Logging is
    /// unaffected — the session log records TX at send time either way (see
    /// [`App::record_tx`]).
    pub echo_on: bool,
    /// Mirrors `logger.is_on()` (kept as a flag for the status line / quit
    /// guard); only mutated through [`App::log_open`]/[`App::log_off`].
    pub log_on: bool,
    /// Session log writer (`:log`); raw RX/TX bytes in stream order.
    pub logger: Logger,
    /// Current serial config; `None` when never connected.
    pub port_cfg: Option<SerialConfig>,
    pub connected: bool,
    /// Startup warnings (config parse problems).
    pub warnings: Vec<String>,
    /// Resolved theme (config.rs), applied frame-wide by ui.rs.
    pub theme: Theme,
    pub last_error: Option<String>,
    /// Informational (non-error) status message, e.g. `:ports` listing or
    /// `:baud` display; shown on the status line next to `last_error`.
    /// Paired with the `Instant` it was set at, for auto-expiry
    /// (see [`STATUS_TTL`] / [`App::tick_status`]).
    ///
    /// Status freshness rules (who clears what):
    /// - `last_error` is cleared UP FRONT by every `:` command (command.rs
    ///   `execute`); a failing command then sets its own error afterwards, so
    ///   the status line always shows the LATEST command's outcome (the same
    ///   failure repeated twice is visible both times). A successful TX send
    ///   (`send_tx`) and a successful connect/reconnect (command.rs
    ///   `connect`) also clear it. Errors NEVER auto-expire — unchanged by
    ///   the status-transience rules below.
    /// - `status_msg` is TRANSIENT (vim-like): it is cleared by the next Key
    ///   press (main.rs event loop, BEFORE routing the key — so the Enter
    ///   that produced the message does not immediately clear it), by
    ///   [`App::tick_status`] once [`STATUS_TTL`] has elapsed since it was
    ///   set (called every event-loop iteration), and whenever a new `:`
    ///   command starts executing (command.rs `execute`, via
    ///   [`App::clear_status`]) so a previous command's output never lingers
    ///   under the new one; it is set again by the command on success.
    pub status_msg: Option<(String, Instant)>,
    /// Last viewport height in body rows, updated by `ui::render` each frame;
    /// motions (`Ctrl-d`/`u`/`f`/`b`, `gg`) scale by it. Defaults to 24 so
    /// headless tests and the first keypress before the first draw behave.
    pub viewport_rows: usize,
    /// Pending NORMAL-mode operator key: `g` (awaiting `gg`) or `Z` (awaiting
    /// `ZZ`). Any other key cancels the pending state.
    pub pending: Option<PendingKey>,
    /// Active search prompt: `Some(typed)` while `/` input is open.
    pub search_prompt: Option<String>,
    /// Last executed search pattern (`n`/`N` reuse it).
    pub last_search: Option<String>,
    /// Scrollback index of the current search match, for `n`/`N` stepping.
    /// With an active filter this is a position in the FILTERED view (see
    /// [`App::find_matches`]); without one it is an absolute scrollback index.
    pub last_match: Option<usize>,
    /// Active `:hl <pattern>` highlight pattern (`Some` = on). Case-insensitive
    /// substring over decoded text, same matcher as `/` search. Independent of
    /// `filter_pattern`: both can be active at once (hl one pattern, filter
    /// another). Rendered by the TEXT view only (skipped in hex view).
    pub hl_pattern: Option<String>,
    /// Active `:filter <pattern>` view filter (`Some` = on). A view over the
    /// scrollback: the full buffer is retained (logging, `:w`, `:filter off`
    /// all see every line); the renderer and motions see only matching lines
    /// (see [`App::filtered_indices`]). Search (`/`, `n`/`N`) operates within
    /// the filtered view while a filter is active.
    pub filter_pattern: Option<String>,
    /// Cached FILTER view: absolute scrollback indices of the lines the active
    /// filter shows (see [`App::filtered_indices`]), oldest first. Tagged with
    /// the pattern it was built for: `filter_cache_for != filter_pattern`
    /// means STALE — readers then recompute without caching instead of
    /// trusting it. Kept fresh by construction in normal operation: the only
    /// pattern mutation is [`App::set_filter`] (rebuilds immediately) and the
    /// only scrollback mutation is [`App::push_line`] (maintains the cache
    /// incrementally). Without a filter the cache is simply `0..len`, so
    /// readers and maintenance share one path.
    filter_cache: Vec<usize>,
    /// The `filter_pattern` value `filter_cache` was built for (see above).
    filter_cache_for: Option<String>,
    pub command_line: String,
    /// `:help` popup open (mode-independent modal: while set, keys only close
    /// the popup — see input.rs `handle_key`; rendering in ui.rs).
    pub help_visible: bool,
    /// Current `:help` popup page (0-based, `0..HELP_PAGE_COUNT`). Reset to 0
    /// whenever the popup opens; vim-style page keys move it (input.rs).
    pub help_page: usize,
    /// Line-mode (`:line on`) input bar text — survives Esc → NORMAL → `i`
    /// (spec: "pending text" preservation).
    pub line_input: String,
    /// Line-mode cursor as a CHAR index into `line_input` (0 = before first
    /// char; editing must be char-wise, not byte-wise, for UTF-8 input).
    pub line_cursor: usize,
    /// Line-mode history, oldest first, newest last, capped at
    /// [`LINE_HISTORY_CAP`] (per-session, in-memory only).
    pub line_history: VecDeque<String>,
    /// History position while browsing: `Some(i)` = viewing `line_history[i]`;
    /// `None` = editing the draft (kept in `line_draft` while browsing).
    pub line_hist_pos: Option<usize>,
    /// Draft saved when history browsing starts, restored when Down steps
    /// past the newest entry.
    pub line_draft: String,
    pub quit: bool,
    /// TX channel to the serial worker; `None` until a port is opened.
    pub tx: Option<mpsc::Sender<ToWorker>>,
    /// RX event channel from the serial worker; `None` until a port is opened.
    pub rx: Option<mpsc::Receiver<FromWorker>>,
    /// In-progress `:sendfile`, one chunk per main-loop iteration
    /// ([`App::pump_sendfile`]). Quitting just drops it.
    pub pending_sendfile: Option<PendingSendFile>,
    splitter: LineSplitter,
}

impl App {
    pub fn new(config: &Config) -> Self {
        App {
            mode: Mode::Normal,
            insert_style: InsertStyle::Raw,
            txnl: TxNewline::default(),
            scrollback: VecDeque::new(),
            scrollback_cap: DEFAULT_SCROLLBACK,
            view_offset: 0,
            follow_tail: true,
            ts_on: false,
            hex_on: false,
            echo_on: false,
            log_on: false,
            logger: Logger::default(),
            port_cfg: None,
            connected: false,
            warnings: config.warnings.clone(),
            theme: config.theme,
            last_error: None,
            status_msg: None,
            viewport_rows: 24,
            pending: None,
            search_prompt: None,
            last_search: None,
            last_match: None,
            hl_pattern: None,
            filter_pattern: None,
            filter_cache: Vec::new(),
            filter_cache_for: None,
            command_line: String::new(),
            help_visible: false,
            help_page: 0,
            line_input: String::new(),
            line_cursor: 0,
            line_history: VecDeque::new(),
            line_hist_pos: None,
            line_draft: String::new(),
            quit: false,
            tx: None,
            rx: None,
            pending_sendfile: None,
            splitter: LineSplitter::new(LINE_IDLE_FLUSH),
        }
    }

    /// Append a completed line into the ring buffer; evict oldest beyond cap.
    ///
    /// View bookkeeping:
    /// Append a completed line into the ring buffer; evict oldest beyond cap.
    ///
    /// View bookkeeping:
    /// - `last_match` is a view index (ABSOLUTE scrollback index without a
    ///   filter, position in the FILTERED list with one — see
    ///   [`App::find_matches`]), so every evicted VISIBLE line shifts it DOWN
    ///   by one (`checked_sub` — `None` means the match line itself was
    ///   evicted; clearing it kills the highlight the same way the "no match"
    ///   paths in `exec_search`/`search_step` do). Without a filter every
    ///   evicted line is visible; with one, only evicted lines that MATCH the
    ///   filter shrink the front of the filtered list, so the rebase counts
    ///   those. (Index-based: eviction shifts every remaining line one
    ///   position up in the view list.)
    /// - `view_offset` counts lines back from the TAIL of the view (full
    ///   scrollback, or the filtered list): APPENDING a VISIBLE line moves
    ///   every existing line one line FARTHER from the tail, so a paused
    ///   viewport (`!follow_tail`) must GROW the offset by `appended_visible`
    ///   (1 per appended line the filter shows; always 1 unfiltered) to keep
    ///   showing the same content. Eviction is offset-neutral (it drops front
    ///   lines, which leaves the remaining lines' tail-relative distance
    ///   unchanged). The result is clamped unconditionally, so an offset left
    ///   past the last line by a shrunken view (a matching eviction with a
    ///   filter active) still lands on a visible window. Follow-tail keeps
    ///   `view_offset == 0` — rendering is tail-anchored there, so neither
    ///   appends nor eviction move the view.
    /// - The filtered-view cache ([`App::filtered_indices`]) is maintained
    ///   incrementally instead of rescanned: `appended_visible` ("new line
    ///   matches the filter") is decided ONCE and appends the new index, and
    ///   eviction drops the cache's front entries below the evicted RAW count
    ///   and shifts the rest — the entries below that count are exactly the
    ///   MATCHING evictions, i.e. the same `evicted_visible` quantity
    ///   `last_match` rebases on.
    pub fn push_line(&mut self, bytes: Vec<u8>, ts: SystemTime, flags: LineFlags) {
        // Rebuild the cache first if a `filter_pattern` change bypassed
        // `set_filter`, so the incremental maintenance below is always valid.
        self.ensure_filter_cache();
        // Guard `cap > 0` so a zero cap cannot spin forever on an empty buffer
        // (pop_front on empty is a no-op and would never shrink `len`).
        let mut evicted_raw = 0usize;
        while self.scrollback_cap > 0 && self.scrollback.len() >= self.scrollback_cap {
            if self.scrollback.pop_front().is_some() {
                evicted_raw += 1;
            }
        }
        // One match decision, shared by the view bookkeeping and the cache.
        let appended_visible = self.matches_filter(&bytes);
        let new_index = self.scrollback.len();
        self.scrollback.push_back(Line { bytes, ts, flags });
        let evicted_visible = {
            let cache = &mut self.filter_cache;
            let evicted_matches = cache.partition_point(|&i| i < evicted_raw);
            cache.drain(..evicted_matches);
            for i in cache.iter_mut() {
                *i -= evicted_raw;
            }
            evicted_matches
        };
        if appended_visible {
            self.filter_cache.push(new_index);
        }
        if evicted_visible > 0 {
            // `checked_sub`: None == the match line itself was evicted.
            self.last_match = self.last_match.and_then(|i| i.checked_sub(evicted_visible));
        }
        if !self.follow_tail {
            self.view_offset = self.clamp_offset(self.view_offset + appended_visible as usize);
        }
    }

    /// Drain the app-owned RX channel, if a port is open.
    pub fn drain(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        self.drain_worker_at(&rx, std::time::Instant::now());
        self.rx = Some(rx);
    }

    /// Consume worker events: drain RX chunks through the line splitter and
    /// record disconnects. RX always appends into the buffer, even while
    /// follow-tail is paused (rendering pauses, ingestion does not). The
    /// monotonic clock is injectable for tests (idle-flush timing).
    pub fn drain_worker_at(&mut self, rx: &mpsc::Receiver<FromWorker>, now: std::time::Instant) {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                FromWorker::Data(chunk, _arrival) => {
                    // Log the RAW chunk at arrival, before line splitting —
                    // the log is a byte-stream record, not a line record, so
                    // partial lines must not wait for the idle flush.
                    self.logger.write(&chunk);
                    let wall = SystemTime::now();
                    for (line, truncated) in self.splitter.feed(&chunk, now) {
                        self.push_line(line, wall, LineFlags { truncated, tx: false });
                    }
                }
                FromWorker::Disconnected(err) => {
                    self.connected = false;
                    self.last_error = Some(err);
                }
            }
        }
        // Idle-flush a warm partial line so silent transmissions still show up.
        if let Some((line, truncated)) = self.splitter.flush_idle(now) {
            self.push_line(line, SystemTime::now(), LineFlags { truncated, tx: false });
        }
    }

    /// Ordering hook for TX logging (spec: TX recorded at send time). Called
    /// by `send_tx` AFTER the worker channel send succeeded, so the log holds
    /// exactly the bytes actually queued to the port (a failed send is never
    /// logged — see `send_tx`); RX writes happen in the drain path (also on
    /// the main thread), giving a single ordered byte stream.
    pub fn record_tx(&mut self, bytes: &[u8]) {
        self.logger.write(bytes);
    }

    /// `:log <path>`: open the session log in append mode. On failure logging
    /// stays off (an existing open log is left untouched) and the error is
    /// surfaced to the caller for the status line.
    pub fn log_open(&mut self, path: &str) -> std::io::Result<()> {
        self.logger.open(path)?;
        self.log_on = true;
        Ok(())
    }

    /// `:log off`: close the log (idempotent).
    pub fn log_off(&mut self) {
        self.logger.close();
        self.log_on = false;
    }

    /// Send TX bytes to the worker, record them, and (with `:echo on`) echo
    /// them into the scrollback. Returns whether the bytes were queued to the
    /// worker.
    ///
    /// Failure semantics: the channel send is attempted FIRST; only a
    /// successful send is logged (log-after-successful-send — the session log
    /// must never contain bytes the port never saw) and clears `last_error`
    /// (a successful TX proves any prior error is stale — status freshness,
    /// see the field docs above). A failed send (`Err`: receiver dropped /
    /// worker gone) logs NOTHING and surfaces a status error instead; the
    /// bytes are dropped (there is no retry queue).
    ///
    /// Local echo (`App::echo_on`, `:echo on`): a successful send pushes the
    /// sent bytes as ONE scrollback line flagged [`LineFlags::tx`] — one line
    /// per successful send, so each 4 KiB `:sendfile` chunk is its own line
    /// (documented choice: simplest correct — the stored byte stream per line
    /// is exactly what one `send()` queued). The stored bytes are RAW (no
    /// `> ` prefix; embedded `\r`/`\n` render caret-encoded via
    /// `decode_lossy_caret`), so `:w` dumps and the log stay byte-exact; the
    /// prefix is render-only (ui.rs). Echo goes through [`App::push_line`]
    /// like RX: timestamps, ring eviction and `:filter` apply identically
    /// (filter/hl/search match the STORED bytes — the render-only `> ` prefix
    /// is not part of the matchable text). Logging is unaffected.
    pub fn send_tx(&mut self, tx: &mpsc::Sender<ToWorker>, bytes: Vec<u8>) -> bool {
        // `ToWorker::Tx` owns its bytes, so the log write (success path only)
        // works off a clone; sends are at most one 4 KiB sendfile chunk, so
        // the copy is negligible.
        match tx.send(ToWorker::Tx(bytes.clone())) {
            Ok(()) => {
                self.record_tx(&bytes);
                self.last_error = None;
                if self.echo_on {
                    // One echoed line per successful send (see doc above);
                    // timestamp is the send time, like RX arrival time.
                    self.push_line(
                        bytes.clone(),
                        SystemTime::now(),
                        LineFlags {
                            truncated: false,
                            tx: true,
                        },
                    );
                }
                true
            }
            Err(_) => {
                self.set_error("send: port not available");
                false
            }
        }
    }

    /// Feed ONE [`SENDFILE_CHUNK`] of the pending `:sendfile` to the worker
    /// (called every main-loop iteration from main.rs, next to `drain()`).
    ///
    /// - EOF (or a zero-length transfer): state cleared, status
    ///   `sendfile: <total> bytes`.
    /// - Read error mid-file: status error, state cleared; bytes already queued
    ///   stay queued (there is no unsend — documented in command.rs too).
    /// - Port went down mid-transfer: state cleared with an error so a later
    ///   `:r` does not surprise-restart the remaining file onto the new
    ///   connection.
    /// - A chunk the worker channel refuses (send failed): the chunk is not
    ///   counted and the transfer aborts (state cleared); the completion
    ///   status is only set after a successfully queued final chunk.
    pub fn pump_sendfile(&mut self, tx: &mpsc::Sender<ToWorker>) {
        if !self.connected {
            if self.pending_sendfile.take().is_some() {
                self.set_error("sendfile: port went down — transfer aborted");
            }
            return;
        }
        let Some(mut send) = self.pending_sendfile.take() else {
            return;
        };
        if send.remaining == 0 {
            self.set_status(format!("sendfile: {} bytes", send.sent));
            return;
        }
        let mut chunk = vec![0u8; SENDFILE_CHUNK.min(send.remaining as usize)];
        match std::io::Read::read(&mut send.file, &mut chunk) {
            Ok(0) => {
                // EOF before `remaining` ran out: the file shrank mid-send.
                // The queued prefix stands; report what was actually sent.
                self.set_status(format!("sendfile: {} bytes", send.sent));
            }
            Ok(n) => {
                chunk.truncate(n);
                if self.send_tx(tx, chunk) {
                    // Count the chunk only once it is actually queued; the
                    // completion status comes only after the final one.
                    send.remaining -= n as u64;
                    send.sent += n as u64;
                    if send.remaining == 0 {
                        self.set_status(format!("sendfile: {} bytes", send.sent));
                    } else {
                        // Still in flight: put it back for the next iteration.
                        self.pending_sendfile = Some(send);
                    }
                }
                // else: send_tx already surfaced the error; the chunk was not
                // queued so it is not counted, and `pending_sendfile` stays
                // cleared — the transfer is aborted, no completion status.
            }
            Err(e) => {
                self.set_error(format!("sendfile {}: {e}", send.path));
            }
        }
    }

    pub fn set_error(&mut self, err: impl Into<String>) {
        self.last_error = Some(err.into());
    }

    /// Set an informational (non-error) status-line message, stamped with
    /// `Instant::now()` for auto-expiry (see [`App::tick_status`]).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }

    /// Clear the informational status message immediately. Called by the
    /// main.rs event loop when the next Key press arrives (before routing the
    /// key) and by command.rs `execute` up front (new command replaces the
    /// previous command's output).
    pub fn clear_status(&mut self) {
        self.status_msg = None;
    }

    /// The status message text, ignoring when it was set (readers — ui.rs
    /// rendering, tests — never need the timestamp; see [`STATUS_TTL`]).
    pub fn status_text(&self) -> Option<&str> {
        self.status_msg.as_ref().map(|(m, _)| m.as_str())
    }

    /// Auto-expire the informational status message once [`STATUS_TTL`] has
    /// elapsed since it was set. `now` is injected so tests can run
    /// headlessly; main.rs calls this every event-loop iteration (the ~25 ms
    /// `event::poll` timeout bounds the check interval). Never touches
    /// `last_error` — errors have their own freshness rules.
    pub fn tick_status(&mut self, now: Instant) {
        if let Some((_, set_at)) = &self.status_msg
            && now.duration_since(*set_at) >= STATUS_TTL
        {
            self.status_msg = None;
        }
    }

    // -----------------------------------------------------------------------
    // Line-mode (`:line on`) input bar: local editing + in-memory history.
    // `line_cursor` is a char index; all edits are char-wise so multi-byte
    // UTF-8 input stays valid.
    // -----------------------------------------------------------------------

    /// Insert a printable char at the cursor.
    pub fn line_insert_char(&mut self, c: char) {
        let byte = self.char_to_byte(self.line_cursor);
        self.line_input.insert(byte, c);
        self.line_cursor += 1;
    }

    /// Backspace: delete the char before the cursor (no-op at position 0).
    pub fn line_backspace(&mut self) {
        if self.line_cursor == 0 {
            return;
        }
        let start = self.char_to_byte(self.line_cursor - 1);
        let end = self.char_to_byte(self.line_cursor);
        self.line_input.replace_range(start..end, "");
        self.line_cursor -= 1;
    }

    /// `Ctrl-u`: clear the whole input line, cursor home.
    pub fn line_clear(&mut self) {
        self.line_input.clear();
        self.line_cursor = 0;
    }

    /// `Ctrl-w`: delete the word before the cursor (vi/emacs style: skip
    /// whitespace back, then non-whitespace back).
    pub fn line_delete_word_back(&mut self) {
        let chars: Vec<char> = self.line_input.chars().collect();
        let mut i = self.line_cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = self.char_to_byte(i);
        let end = self.char_to_byte(self.line_cursor);
        self.line_input.replace_range(start..end, "");
        self.line_cursor = i;
    }

    /// Left arrow (no-op at the start).
    pub fn line_left(&mut self) {
        self.line_cursor = self.line_cursor.saturating_sub(1);
    }

    /// Right arrow (clamped at end of line).
    pub fn line_right(&mut self) {
        let len = self.line_input.chars().count();
        self.line_cursor = (self.line_cursor + 1).min(len);
    }

    /// Up arrow: step to an older history entry (draft saved on first step).
    pub fn line_history_prev(&mut self) {
        if self.line_history.is_empty() {
            return;
        }
        if self.line_hist_pos.is_none() {
            self.line_draft = std::mem::take(&mut self.line_input);
            self.line_hist_pos = Some(self.line_history.len()); // draft slot
        }
        let pos = self.line_hist_pos.unwrap().saturating_sub(1);
        self.line_hist_pos = Some(pos);
        self.load_history_at(pos);
    }

    /// Down arrow: step to a newer history entry; past the newest restores
    /// the draft saved by [`App::line_history_prev`].
    pub fn line_history_next(&mut self) {
        let Some(pos) = self.line_hist_pos else {
            return;
        };
        if pos + 1 >= self.line_history.len() {
            // Back to the draft slot.
            self.line_hist_pos = None;
            self.line_input = std::mem::take(&mut self.line_draft);
            self.line_cursor = self.line_input.chars().count();
            return;
        }
        self.line_hist_pos = Some(pos + 1);
        self.load_history_at(pos + 1);
    }

    fn load_history_at(&mut self, pos: usize) {
        if let Some(entry) = self.line_history.get(pos) {
            self.line_input = entry.clone();
            self.line_cursor = entry.chars().count();
        }
    }

    /// Enter in line mode: non-empty line appends to history (evicting beyond
    /// [`LINE_HISTORY_CAP`]) and the line + `:txnl` ending is sent via the
    /// worker (ordering hook runs inside [`App::send_tx`]). Input bar clears,
    /// history browsing resets. Empty line sends just the ending.
    ///
    /// While the port is DOWN (`App::connected == false`) nothing is sent —
    /// the pending text is preserved and the status line names `:r` as the
    /// reconnect path, so input can never silently vanish.
    pub fn line_submit(&mut self, tx: Option<&mpsc::Sender<ToWorker>>) {
        if !self.connected {
            self.set_error("port is DOWN — :r to reconnect");
            return;
        }
        let line = std::mem::take(&mut self.line_input);
        self.line_cursor = 0;
        self.line_hist_pos = None;
        self.line_draft.clear();
        if !line.is_empty() {
            while self.line_history.len() >= LINE_HISTORY_CAP {
                self.line_history.pop_front();
            }
            self.line_history.push_back(line.clone());
        }
        if let Some(tx) = tx {
            let bytes = self.txnl.encode_line(&line);
            self.send_tx(tx, bytes);
        }
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.line_input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.line_input.len())
    }

    // -----------------------------------------------------------------------
    // NORMAL-mode motions (vi scrollback; view_offset counts lines back from
    // the tail of the VIEW — the full scrollback without a filter, the filtered
    // line list with one — 0 = newest). All math saturates, so empty/short
    // buffers are no-ops and offsets stay in range by construction.
    // -----------------------------------------------------------------------

    /// The FILTER view: absolute scrollback indices of the lines currently
    /// displayed, oldest first. No filter (or an empty pattern) = every line;
    /// with `filter_pattern` set, only lines whose decoded text matches the
    /// pattern case-insensitively ([`find_ci`], the shared search matcher).
    ///
    /// Reads the cached index (`filter_cache` — see the field docs): a clone
    /// of the cache when it is fresh, so renders/motions/arrivals are O(view)
    /// instead of a full decode+scan per call. The cache is kept fresh by
    /// [`App::set_filter`] (the single pattern mutation point, rebuilds
    /// immediately) and [`App::push_line`] (incremental append/evict); only a
    /// `filter_pattern` change that bypasses `set_filter` (tests) can leave it
    /// stale, and readers then recompute WITHOUT caching rather than trust it.
    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.filter_cache_is_fresh() {
            return self.filter_cache.clone();
        }
        match &self.filter_pattern {
            Some(p) => self
                .scrollback
                .iter()
                .enumerate()
                .filter(|(_, line)| find_ci(&decode_lossy_caret(&line.bytes), p).is_some())
                .map(|(i, _)| i)
                .collect(),
            None => (0..self.scrollback.len()).collect(),
        }
    }

    /// Number of lines in the current view (filtered count with an active
    /// filter, `scrollback.len()` otherwise) — the cache's length when it is
    /// fresh, a recount otherwise (same fallback rules as
    /// [`App::filtered_indices`]).
    pub fn view_len(&self) -> usize {
        if self.filter_cache_is_fresh() {
            return self.filter_cache.len();
        }
        match &self.filter_pattern {
            Some(p) => self
                .scrollback
                .iter()
                .filter(|line| find_ci(&decode_lossy_caret(&line.bytes), p).is_some())
                .count(),
            None => self.scrollback.len(),
        }
    }

    /// Whether `filter_cache` was built for the current `filter_pattern`
    /// (compared by VALUE, not just `Some`-ness, so a pattern CHANGE is stale).
    fn filter_cache_is_fresh(&self) -> bool {
        self.filter_cache_for.as_deref() == self.filter_pattern.as_deref()
    }

    /// Rebuild `filter_cache` from a full scan unless it is already fresh
    /// (see the field docs for the freshness invariant).
    fn ensure_filter_cache(&mut self) {
        if self.filter_cache_is_fresh() {
            return;
        }
        self.filter_cache = match &self.filter_pattern {
            Some(p) => self
                .scrollback
                .iter()
                .enumerate()
                .filter(|(_, line)| find_ci(&decode_lossy_caret(&line.bytes), p).is_some())
                .map(|(i, _)| i)
                .collect(),
            None => (0..self.scrollback.len()).collect(),
        };
        self.filter_cache_for = self.filter_pattern.clone();
    }

    /// Set the active filter (`Some(p)` = `:filter <p>`, `None` = `:filter
    /// off`) and rebuild the filtered-view cache in the same step. This is
    /// the SINGLE mutation point for `filter_pattern` in command handling, so
    /// a pattern change can never leave the cache stale for the subsequent
    /// readers/renders; `push_line` keeps it fresh from here on. Coordinate
    /// transitions (`last_match` coordinate drop, `goto_end`) stay with the
    /// `:filter` command arms in command.rs.
    pub fn set_filter(&mut self, pattern: Option<String>) {
        self.filter_pattern = pattern;
        self.ensure_filter_cache();
    }

    /// Case-insensitive filter match over a line's decoded text — the ONE
    /// predicate shared by the cache maintenance in [`App::push_line`], the
    /// cache rebuild in [`App::ensure_filter_cache`], and the reader fallbacks
    /// (no filter = every line visible).
    fn matches_filter(&self, bytes: &[u8]) -> bool {
        match &self.filter_pattern {
            Some(p) => find_ci(&decode_lossy_caret(bytes), p).is_some(),
            None => true,
        }
    }

    /// Clamp `view_offset` to the largest value that still shows at least one
    /// line (`len - 1` back; the window ends there and older lines are cut).
    fn clamp_offset(&self, offset: usize) -> usize {
        offset.min(self.view_len().saturating_sub(1))
    }

    /// Move the view down (toward newer lines) by `n` lines (`j`, `Ctrl-d`,
    /// `Ctrl-f`). At the tail this is a no-op; follow-tail is NOT re-enabled —
    /// `G`/`F` do that explicitly.
    pub fn motion_down(&mut self, n: usize) {
        if !self.follow_tail {
            self.view_offset = self.view_offset.saturating_sub(n);
        }
    }

    /// Move the view up (toward older lines) by `n` lines (`k`, `Ctrl-u`,
    /// `Ctrl-b`). Pauses follow-tail on the first move.
    pub fn motion_up(&mut self, n: usize) {
        if self.view_len() == 0 {
            return;
        }
        self.follow_tail = false;
        self.view_offset = self.clamp_offset(self.view_offset.saturating_add(n));
    }

    /// `gg` — jump to the oldest line, shown at the top of the viewport.
    pub fn goto_top(&mut self) {
        if self.view_len() == 0 {
            return;
        }
        self.follow_tail = false;
        self.view_offset =
            self.clamp_offset(self.view_len().saturating_sub(self.viewport_rows.max(1)));
    }

    /// `G` — jump to the newest line and resume follow-tail.
    pub fn goto_end(&mut self) {
        self.view_offset = 0;
        self.follow_tail = true;
    }

    /// `F` / `p` — toggle follow-tail. Resuming clears any stale offset so the
    /// view snaps back to the tail; RX ingestion is unaffected either way.
    pub fn toggle_follow(&mut self) {
        self.follow_tail = !self.follow_tail;
        if self.follow_tail {
            self.view_offset = 0;
        }
    }

    // -----------------------------------------------------------------------
    // Search (`/pattern`, `n`, `N`): case-insensitive substring over the
    // decoded text (same decode as the renderer), wrap-around navigation.
    // -----------------------------------------------------------------------

    /// Positions, within the current VIEW, of all lines whose decoded text
    /// contains `pattern` case-insensitively. One [`find_ci`] scan per line
    /// (no per-position work), so the whole pass is linear in total bytes.
    /// With an active filter the candidate list IS the filtered-view cache
    /// ([`App::filtered_indices`]) — only matching lines are re-scanned.
    ///
    /// View-relative (not absolute): without a filter a position IS the
    /// absolute scrollback index; with an active filter the positions index
    /// into [`App::filtered_indices`] — `n`/`N` and `last_match` then live in
    /// filtered-view coordinates, so search navigates only the lines the
    /// filter shows.
    pub fn find_matches(&self, pattern: &str) -> Vec<usize> {
        if pattern.is_empty() {
            return Vec::new();
        }
        match &self.filter_pattern {
            None => self
                .scrollback
                .iter()
                .enumerate()
                .filter(|(_, line)| find_ci(&decode_lossy_caret(&line.bytes), pattern).is_some())
                .map(|(i, _)| i)
                .collect(),
            Some(_) => self
                .filtered_indices()
                .into_iter()
                .enumerate()
                .filter(|(_, i)| {
                    find_ci(&decode_lossy_caret(&self.scrollback[*i].bytes), pattern).is_some()
                })
                .map(|(pos, _)| pos)
                .collect(),
        }
    }

    /// Enter on the `/` prompt: run the search and jump to the first match at
    /// or below the current view top (wrap to the first match if none ahead).
    /// No match -> status-line message, view unchanged.
    pub fn exec_search(&mut self) {
        let pattern = self.search_prompt.take().unwrap_or_default();
        if pattern.is_empty() {
            return;
        }
        self.last_search = Some(pattern.clone());
        let matches = self.find_matches(&pattern);
        match self.first_match_at_or_below(&matches) {
            Some(i) => self.jump_to_match(i),
            None => {
                self.last_match = None;
                self.set_error(format!("/{}: no match", pattern));
            }
        }
    }

    /// First match index >= the top line of the current view, else the first
    /// match overall (wrap-around).
    fn first_match_at_or_below(&self, matches: &[usize]) -> Option<usize> {
        let top = self.view_top();
        matches
            .iter()
            .find(|&&i| i >= top)
            .copied()
            .or_else(|| matches.first().copied())
    }

    /// Index of the first visible (oldest) line in the viewport (a view
    /// position: filtered-view index with an active filter).
    fn view_top(&self) -> usize {
        let len = self.view_len();
        let end = len.saturating_sub(if self.follow_tail {
            0
        } else {
            self.view_offset
        });
        end.saturating_sub(self.viewport_rows.max(1))
    }

    /// `n` (forward) / `N` (backward) from the current match, wrapping around.
    /// Without a previous match or pattern it (re)runs `last_search`.
    pub fn search_step(&mut self, forward: bool) {
        let Some(pattern) = self.last_search.clone() else {
            self.set_error("no previous search");
            return;
        };
        let matches = self.find_matches(&pattern);
        if matches.is_empty() {
            self.last_match = None;
            self.set_error(format!("/{pattern}: no match"));
            return;
        }
        let next = match self.last_match {
            None => {
                if forward {
                    matches[0]
                } else {
                    matches[matches.len() - 1]
                }
            }
            Some(cur) if forward => matches
                .iter()
                .find(|&&i| i > cur)
                .copied()
                .unwrap_or(matches[0]),
            Some(cur) => matches
                .iter()
                .rev()
                .find(|&&i| i < cur)
                .copied()
                .unwrap_or(matches[matches.len() - 1]),
        };
        self.jump_to_match(next);
    }

    /// Scroll the view so the match line sits at the top of the viewport
    /// (clamped at the tail), pausing follow-tail. `index` is a view position
    /// (filtered-view index with an active filter, absolute otherwise).
    fn jump_to_match(&mut self, index: usize) {
        self.last_match = Some(index);
        self.follow_tail = false;
        self.view_offset = self.clamp_offset(
            self.view_len()
                .saturating_sub(index + self.viewport_rows.max(1)),
        );
    }
}

/// Case-insensitive substring search over `hay`, returning the char index of
/// the first match. Char-wise compare (not `str::to_lowercase` byte offsets,
/// which can shift for non-ASCII like `İ`).
///
/// Complexity: O(h · p) char comparisons, O(h + p) allocations (one `Vec<char>`
/// per input, once per call — callers like [`App::find_matches`] call this once
/// per line, so the whole search stays linear in scrollback size). Per-position
/// work is allocation-free via [`chars_match_ci`]; the UI highlighter
/// (ui.rs `highlight_spans`) scans all match positions in one pass on the same
/// terms instead of building a suffix `String` per position (the old shape was
/// O(n²) allocations).
pub fn find_ci(hay: &str, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if p.is_empty() {
        return None;
    }
    let h: Vec<char> = hay.chars().collect();
    if p.len() > h.len() {
        return None;
    }
    (0..=h.len() - p.len()).find(|&i| chars_match_ci(&h, &p, i))
}

/// Allocation-free check: does the case-insensitive pattern `p` match `h`
/// starting at char index `i` (same per-char `to_lowercase` iterator equality
/// as [`find_ci`])? Caller guarantees `i + p.len() <= h.len()`. Shared with the
/// ui.rs highlighter so both paths use identical match semantics.
pub fn chars_match_ci(h: &[char], p: &[char], i: usize) -> bool {
    h[i..i + p.len()]
        .iter()
        .zip(p)
        .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
}

/// Format a wall-clock timestamp as `HH:MM:SS.mmm` in the machine's local
/// timezone (chrono `Local`, feature `clock`; a serial terminal wants a
/// simple wall clock, not a timezone name).
pub fn format_ts(ts: SystemTime) -> String {
    let dt: DateTime<Local> = ts.into();
    dt.format("%H:%M:%S%.3f").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn app() -> App {
        App::new(&Config::default())
    }

    #[test]
    fn scrollback_appends_in_order() {
        let mut a = app();
        let t = SystemTime::now();
        a.push_line(b"a".to_vec(), t, LineFlags::default());
        a.push_line(b"b".to_vec(), t, LineFlags::default());
        a.push_line(b"c".to_vec(), t, LineFlags::default());
        let texts: Vec<&[u8]> = a.scrollback.iter().map(|l| l.bytes.as_slice()).collect();
        assert_eq!(texts, [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
    }

    #[test]
    fn scrollback_evicts_oldest_at_cap() {
        let mut a = app();
        a.scrollback_cap = 3;
        let t = SystemTime::now();
        for i in 0..5u8 {
            a.push_line(vec![b'0' + i], t, LineFlags::default());
        }
        assert_eq!(a.scrollback.len(), 3);
        let texts: Vec<u8> = a.scrollback.iter().flat_map(|l| l.bytes.clone()).collect();
        assert_eq!(texts, vec![b'2', b'3', b'4']);
        // Pushing one more evicts exactly one more.
        a.push_line(b"x".to_vec(), t, LineFlags::default());
        assert_eq!(a.scrollback.len(), 3);
        assert_eq!(a.scrollback.front().unwrap().bytes, b"3".to_vec());
    }

    #[test]
    fn default_modes_and_flags() {
        let a = app();
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.insert_style, InsertStyle::Raw);
        assert_eq!(a.txnl, TxNewline::Cr);
        assert!(a.follow_tail);
        assert!(!a.ts_on);
        assert!(!a.hex_on);
        assert!(!a.echo_on, "local echo defaults to off");
        assert!(!a.connected);
        assert_eq!(a.scrollback_cap, DEFAULT_SCROLLBACK);
    }

    #[test]
    fn mode_transitions_skeleton() {
        let mut a = app();
        a.mode = Mode::Insert;
        a.mode = Mode::Normal;
        a.mode = Mode::Command;
        assert_eq!(a.mode, Mode::Command);
    }

    #[test]
    fn drain_worker_splits_rx_lines() {
        let (tx, rx) = mpsc::channel();
        tx.send(FromWorker::Data(
            b"hello\r\nwor".to_vec(),
            std::time::Instant::now(),
        ))
        .unwrap();
        tx.send(FromWorker::Data(b"ld".to_vec(), std::time::Instant::now()))
            .unwrap();
        let t0 = std::time::Instant::now();
        let mut a = app();
        a.connected = true;
        a.drain_worker_at(&rx, t0);
        // "world" is still a warm partial; it flushes once idle.
        assert_eq!(a.scrollback.len(), 1);
        a.drain_worker_at(&rx, t0 + std::time::Duration::from_millis(150));
        let texts: Vec<&[u8]> = a.scrollback.iter().map(|l| l.bytes.as_slice()).collect();
        assert_eq!(texts, [b"hello".as_slice(), b"world".as_slice()]);
        assert!(a.connected); // no disconnect event seen
    }

    #[test]
    fn drain_flushes_queued_rx_to_session_log() {
        // The final `app.drain()` in main.rs (before the quit break): RX
        // queued in the app-owned channel must reach the session log, and
        // `drain` must restore `rx` so nothing is lost on the way out.
        let (tx, rx) = mpsc::channel();
        tx.send(FromWorker::Data(
            b"last words\n".to_vec(),
            std::time::Instant::now(),
        ))
        .unwrap();
        let mut a = app();
        a.connected = true;
        let path =
            std::env::temp_dir().join(format!("ncom-test-drainlog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        a.logger.open(path.to_str().unwrap()).unwrap();
        a.rx = Some(rx);

        a.drain();
        assert!(a.rx.is_some(), "drain must restore the receiver");
        assert_eq!(a.scrollback.len(), 1, "queued RX reaches the buffer");
        // The raw chunk was logged (write flushes immediately).
        assert_eq!(std::fs::read(&path).unwrap(), b"last words\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drain_worker_marks_disconnect() {
        let mut a = app();
        a.connected = true;
        let (tx, rx) = mpsc::channel();
        tx.send(FromWorker::Disconnected("usb unplug".into()))
            .unwrap();
        drop(tx);
        a.drain_worker_at(&rx, std::time::Instant::now());
        assert!(!a.connected);
        assert_eq!(a.last_error.as_deref(), Some("usb unplug"));
    }

    #[test]
    fn log_open_off_lifecycle() {
        let mut a = app();
        let path = std::env::temp_dir()
            .join(format!("ncom-app-log-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        assert!(!a.log_on);
        a.log_open(&path).unwrap();
        assert!(a.log_on);
        a.log_off();
        assert!(!a.log_on);
        a.log_off(); // idempotent
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn log_open_failure_keeps_logging_off() {
        let mut a = app();
        let bad = std::env::temp_dir()
            .join(format!("ncom-app-logfile-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        std::fs::write(&bad, b"x").unwrap();
        assert!(a.log_open(&format!("{bad}/sub/log")).is_err());
        assert!(!a.log_on);
        std::fs::remove_file(&bad).ok();
    }

    #[test]
    fn log_rx_tx_interleave_in_stream_order() {
        let mut a = app();
        let path = std::env::temp_dir()
            .join(format!("ncom-app-log-order-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        a.log_open(&path).unwrap();

        // TX at send time...
        let (tx_w, _rx_w) = mpsc::channel::<ToWorker>();
        a.send_tx(&tx_w, b"tx1>".to_vec());
        // ...then RX at arrival (raw chunk, before line splitting).
        let (tx_r, rx) = mpsc::channel();
        tx_r.send(FromWorker::Data(
            b"rx1\n".to_vec(),
            std::time::Instant::now(),
        ))
        .unwrap();
        a.drain_worker_at(&rx, std::time::Instant::now());
        a.send_tx(&tx_w, b"tx2>".to_vec());
        a.log_off();

        assert_eq!(std::fs::read(&path).unwrap(), b"tx1>rx1\ntx2>".to_vec());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn send_tx_records_then_sends() {
        let mut a = app();
        let (tx_w, rx_w) = mpsc::channel::<ToWorker>();
        a.send_tx(&tx_w, vec![1, 2, 3]);
        match rx_w.try_recv().unwrap() {
            ToWorker::Tx(b) => assert_eq!(b, vec![1, 2, 3]),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Local echo (`:echo on`): successful sends append a TX-flagged line
    // ------------------------------------------------------------------

    #[test]
    fn send_tx_echo_on_pushes_raw_flagged_line_and_log_unchanged() {
        let mut a = app();
        a.echo_on = true;
        let path = std::env::temp_dir()
            .join(format!("ncom-echo-log-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        a.log_open(&path).unwrap();
        let (tx_w, _rx_w) = mpsc::channel::<ToWorker>();
        assert!(a.send_tx(&tx_w, b"hello\r".to_vec()));
        // One scrollback line, flagged TX, holding the RAW sent bytes
        // (no `> ` prefix — the prefix is render-only).
        assert_eq!(a.scrollback.len(), 1);
        let line = a.scrollback.front().unwrap();
        assert_eq!(line.bytes, b"hello\r".to_vec());
        assert!(line.flags.tx);
        assert!(!line.flags.truncated);
        // Logging is unaffected: the log holds exactly the sent bytes.
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\r".to_vec());
        a.log_off();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn send_tx_echo_off_default_pushes_no_scrollback_line() {
        let mut a = app(); // echo_on defaults to false
        let path = std::env::temp_dir()
            .join(format!("ncom-echo-off-log-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        a.log_open(&path).unwrap();
        let (tx_w, rx_w) = mpsc::channel::<ToWorker>();
        assert!(a.send_tx(&tx_w, b"quiet".to_vec()));
        assert!(rx_w.try_recv().is_ok(), "send still goes to the worker");
        assert!(
            a.scrollback.is_empty(),
            "echo off: TX must not appear in the scrollback"
        );
        // Logging records TX regardless of echo.
        assert_eq!(std::fs::read(&path).unwrap(), b"quiet".to_vec());
        a.log_off();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn send_tx_failed_send_pushes_no_echo_line() {
        let mut a = app();
        a.echo_on = true;
        let (tx_w, rx_w) = mpsc::channel::<ToWorker>();
        drop(rx_w); // worker gone
        assert!(!a.send_tx(&tx_w, b"lost".to_vec()));
        assert!(
            a.scrollback.is_empty(),
            "a failed send is never echoed (nothing was sent)"
        );
    }

    #[test]
    fn sendfile_echo_lines_per_chunk() {
        let mut a = app();
        a.echo_on = true;
        a.connected = true;
        let path = std::env::temp_dir()
            .join(format!("ncom-echo-file-{}", std::process::id()));
        std::fs::write(&path, vec![0x61; 10_000]).unwrap(); // > 2 chunks
        a.pending_sendfile = Some(PendingSendFile {
            file: std::fs::File::open(&path).unwrap(),
            remaining: 10_000,
            sent: 0,
            path: path.to_string_lossy().into_owned(),
        });
        let (tx_w, rx_w) = mpsc::channel::<ToWorker>();
        // Collect the queued chunks, then pump the whole file.
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        while let Ok(ToWorker::Tx(b)) = rx_w.try_recv() {
            chunks.push(b);
        }
        for _ in 0..10_000 {
            a.pump_sendfile(&tx_w);
            if a.pending_sendfile.is_none() {
                break;
            }
        }
        while let Ok(ToWorker::Tx(b)) = rx_w.try_recv() {
            chunks.push(b);
        }
        assert!(chunks.len() >= 2, "10 000 bytes span several 4 KiB chunks");
        // One echo line per successful send (per chunk), each flagged TX and
        // holding exactly that chunk's raw bytes.
        assert_eq!(a.scrollback.len(), chunks.len());
        for (line, chunk) in a.scrollback.iter().zip(&chunks) {
            assert!(line.flags.tx);
            assert_eq!(line.bytes, *chunk);
        }
        // Concatenated echo lines reassemble the file exactly.
        let echoed: Vec<u8> = a.scrollback.iter().flat_map(|l| l.bytes.iter().copied()).collect();
        assert_eq!(echoed, vec![0x61u8; 10_000]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn echo_lines_evict_like_any_line() {
        let mut a = app();
        a.echo_on = true;
        a.scrollback_cap = 2;
        let (tx_w, _rx_w) = mpsc::channel::<ToWorker>();
        a.send_tx(&tx_w, b"one".to_vec());
        a.send_tx(&tx_w, b"two".to_vec());
        a.send_tx(&tx_w, b"three".to_vec());
        assert_eq!(a.scrollback.len(), 2, "ring cap applies to echo lines too");
        let texts: Vec<&[u8]> = a.scrollback.iter().map(|l| l.bytes.as_slice()).collect();
        assert_eq!(texts, [b"two".as_slice(), b"three".as_slice()]);
        assert!(a.scrollback.iter().all(|l| l.flags.tx));
    }

    #[test]
    fn ts_formatting() {
        // chrono's Local reads TZ per process; pin it so the fixed-instant
        // assertions are deterministic regardless of the host timezone.
        // Guarded by the shared test env lock (std::env is process-global).
        let _guard = crate::config::tests::lock_env_for_test();
        // SAFETY: single-threaded w.r.t. env mutation (guarded by the shared
        // test env lock); test harness never reads TZ concurrently.
        unsafe { std::env::set_var("TZ", "UTC0") };

        assert_eq!(
            format_ts(SystemTime::UNIX_EPOCH + Duration::from_millis(0)),
            "00:00:00.000"
        );
        // 1_791_824_523.456 s after epoch is 17:02:03.456 in UTC (TZ=UTC0)
        assert_eq!(
            format_ts(SystemTime::UNIX_EPOCH + Duration::from_millis(1_791_824_523_456)),
            "17:02:03.456"
        );
    }

    #[test]
    fn ts_formatting_matches_local_conversion() {
        // Offset-robust check: whatever the host timezone is, format_ts must
        // agree with a direct chrono Local conversion of the same instant.
        let ts = SystemTime::UNIX_EPOCH + Duration::from_millis(1_791_824_523_456);
        let dt: DateTime<Local> = ts.into();
        assert_eq!(format_ts(ts), dt.format("%H:%M:%S%.3f").to_string());
        assert_eq!(format_ts(ts).len(), 12);
    }

    #[test]
    fn motion_empty_buffer_is_noop() {
        let mut a = app();
        a.motion_up(5);
        a.motion_down(5);
        a.goto_top();
        a.goto_end();
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        // After an up-motion attempt on empty buffer follow-tail stays on.
    }

    #[test]
    fn motion_up_down_and_clamping() {
        let mut a = app_with_lines(3);
        a.viewport_rows = 2;
        a.motion_up(1);
        assert!(!a.follow_tail);
        assert_eq!(a.view_offset, 1);
        a.motion_up(1);
        assert_eq!(a.view_offset, 2); // len-1 = 2: last line stays visible
        a.motion_up(10);
        assert_eq!(a.view_offset, 2); // clamped
        a.motion_down(1);
        assert_eq!(a.view_offset, 1);
        a.motion_down(10);
        assert_eq!(a.view_offset, 0); // saturates at tail, follow stays off
        assert!(!a.follow_tail);
        a.motion_down(1);
        assert_eq!(a.view_offset, 0);
    }

    #[test]
    fn motion_short_buffer_noop_up() {
        let mut a = app_with_lines(1);
        a.motion_up(5);
        assert_eq!(a.view_offset, 0);
        assert!(!a.follow_tail); // paused, but the single line still shows
    }

    #[test]
    fn goto_top_and_end() {
        let mut a = app_with_lines(20);
        a.viewport_rows = 5;
        a.goto_top();
        assert!(!a.follow_tail);
        assert_eq!(a.view_offset, 15); // 20-5: first line at viewport top
        a.motion_up(3);
        a.goto_end();
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        // Short buffer: gg clamps to 0.
        let mut b = app_with_lines(2);
        b.goto_top();
        assert_eq!(b.view_offset, 0);
    }

    #[test]
    fn toggle_follow_snap_offset() {
        let mut a = app_with_lines(10);
        a.motion_up(4);
        assert!(!a.follow_tail);
        a.toggle_follow(); // resume
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        a.toggle_follow(); // pause again, offset stays 0
        assert!(!a.follow_tail);
    }

    #[test]
    fn search_case_insensitive_and_matches() {
        let mut a = app();
        let t = SystemTime::now();
        a.push_line(b"Hello World".to_vec(), t, LineFlags::default());
        a.push_line(b"nothing here".to_vec(), t, LineFlags::default());
        a.push_line(b"say HELLO again".to_vec(), t, LineFlags::default());
        assert_eq!(a.find_matches("hello"), vec![0, 2]);
        assert_eq!(a.find_matches("HELLO"), vec![0, 2]);
        assert_eq!(a.find_matches("zzz"), Vec::<usize>::new());
        assert_eq!(a.find_matches(""), Vec::<usize>::new());
        assert_eq!(find_ci("abc", "abcd"), None);
        assert_eq!(find_ci("xAbC", "abc"), Some(1));
    }

    #[test]
    fn search_exec_jumps_and_steps_wrap() {
        let mut a = app_with_lines(10);
        for (i, line) in a.scrollback.iter_mut().enumerate() {
            line.bytes = if i % 3 == 0 {
                format!("hit{i}").into_bytes()
            } else {
                b"miss".to_vec()
            };
        }
        // matches: 0, 3, 6, 9
        a.viewport_rows = 4;
        a.motion_up(2); // view top around line 4
        a.search_prompt = Some("HIT".into());
        a.exec_search();
        assert_eq!(a.last_search.as_deref(), Some("HIT"));
        assert_eq!(a.last_match, Some(6)); // first match at/below view top
        assert!(!a.follow_tail);

        a.search_step(true);
        assert_eq!(a.last_match, Some(9));
        a.search_step(true);
        assert_eq!(a.last_match, Some(0)); // wraps
        a.search_step(false);
        assert_eq!(a.last_match, Some(9)); // wraps backward
        a.search_step(false);
        assert_eq!(a.last_match, Some(6));
    }

    #[test]
    fn search_no_match_is_graceful() {
        let mut a = app_with_lines(3);
        a.search_prompt = Some("nope".into());
        a.exec_search();
        assert!(a.follow_tail); // view untouched
        assert_eq!(a.view_offset, 0);
        assert_eq!(a.last_match, None);
        assert!(a.last_error.clone().unwrap().contains("no match"));

        a.search_step(true); // no previous valid search results
        assert_eq!(a.last_match, None);
    }

    #[test]
    fn search_step_without_pattern_reports() {
        let mut a = app_with_lines(3);
        a.search_step(true);
        assert!(a.last_error.clone().unwrap().contains("no previous search"));
        a.last_search = Some("line".into()); // lines "line0".."line2" all match
        a.search_step(true);
        assert_eq!(a.last_match, Some(0));
        a.search_step(false);
        assert_eq!(a.last_match, Some(2)); // wraps backward to last match
    }

    #[test]
    fn pending_key_enum_defaults() {
        let a = app();
        assert_eq!(a.pending, None);
        assert_eq!(a.search_prompt, None);
        assert_eq!(a.last_search, None);
    }

    // ------------------------------------------------------------------
    // ITEM 4: index rebase on ring eviction
    // ------------------------------------------------------------------

    #[test]
    fn eviction_keeps_viewport_pinned_same_line_content() {
        let mut a = app();
        a.scrollback_cap = 4;
        let t = SystemTime::now();
        for name in [b"a", b"b", b"c", b"d"] {
            a.push_line(name.to_vec(), t, LineFlags::default());
        }
        a.follow_tail = false;
        a.view_offset = 2; // viewport window [0, 2): shows "a", "b", bottom = "b"
        a.push_line(b"e".to_vec(), t, LineFlags::default()); // evicts "a"
        // Offset grows by the VISIBLE APPEND (== eviction count at cap,
        // where every push evicts one and appends one visible line).
        assert_eq!(
            a.view_offset, 3,
            "offset grows by the appended visible line"
        );
        // The viewport bottom is STILL the same content: "b".
        let pinned = &a.scrollback[a.scrollback.len() - 1 - a.view_offset];
        assert_eq!(pinned.bytes, b"b".to_vec());
    }

    #[test]
    fn eviction_past_match_line_clears_last_match() {
        let mut a = app();
        a.scrollback_cap = 3;
        let t = SystemTime::now();
        a.push_line(b"hit".to_vec(), t, LineFlags::default());
        a.push_line(b"miss".to_vec(), t, LineFlags::default());
        a.push_line(b"miss".to_vec(), t, LineFlags::default());
        a.search_prompt = Some("hit".into());
        a.exec_search();
        assert_eq!(a.last_match, Some(0));
        // Evicting the match line itself clears the match (no stale index, no
        // wrong highlight — the renderer keys off `last_match.is_some()`).
        a.push_line(b"x".to_vec(), t, LineFlags::default());
        assert_eq!(a.last_match, None);
    }

    #[test]
    fn eviction_rebases_match_and_n_finds_next() {
        let mut a = app();
        a.scrollback_cap = 4;
        let t = SystemTime::now();
        a.push_line(b"foo".to_vec(), t, LineFlags::default());
        a.push_line(b"hit".to_vec(), t, LineFlags::default());
        a.push_line(b"foo".to_vec(), t, LineFlags::default());
        a.push_line(b"hit too".to_vec(), t, LineFlags::default());
        a.search_prompt = Some("hit".into());
        a.exec_search();
        assert_eq!(a.last_match, Some(1));
        // One eviction: both matches rebase 1 -> 0 and 3 -> 2.
        a.push_line(b"pad".to_vec(), t, LineFlags::default());
        assert_eq!(a.last_match, Some(0));
        a.search_step(true);
        assert_eq!(a.last_match, Some(2), "n steps to the rebased next match");
    }

    #[test]
    fn eviction_leaves_follow_tail_offset_at_zero() {
        let mut a = app();
        a.scrollback_cap = 2;
        let t = SystemTime::now();
        a.push_line(b"one".to_vec(), t, LineFlags::default());
        a.push_line(b"two".to_vec(), t, LineFlags::default());
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        a.push_line(b"three".to_vec(), t, LineFlags::default());
        assert_eq!(a.view_offset, 0, "follow-tail is unaffected by eviction");
        assert!(a.follow_tail);
        assert_eq!(a.scrollback.len(), 2);
    }

    // ------------------------------------------------------------------
    // Paused-viewport pinning: the tail-relative offset follows VISIBLE
    // APPENDS (not evictions) — see push_line's doc comments.
    // ------------------------------------------------------------------

    /// The OLDEST line currently in the viewport (`rows` body rows, window
    /// ending `view_offset` back from the tail — ui.rs `visible_lines` math).
    fn window_top(a: &App, rows: usize) -> &Line {
        let end = a.scrollback.len() - a.view_offset;
        &a.scrollback[end - rows]
    }

    /// The NEWEST line currently in the viewport (same window math, over the
    /// filtered view when one is active).
    fn window_bottom(a: &App) -> &Line {
        let f = a.filtered_indices();
        &a.scrollback[f[f.len() - 1 - a.view_offset]]
    }

    #[test]
    fn paused_viewport_below_cap_stays_pinned_on_arrivals() {
        let mut a = app();
        let t = SystemTime::now();
        for name in [b"a", b"b", b"c", b"d", b"e"] {
            a.push_line(name.to_vec(), t, LineFlags::default());
        }
        a.follow_tail = false;
        // Window ends 2 back from the tail: shows "b", "c".
        a.view_offset = 2;
        assert_eq!(window_top(&a, 2).bytes, b"b".to_vec());
        // Arrivals BELOW the cap used to slide the paused viewport forward
        // (the old code only grew the offset on eviction): the offset must
        // grow per visible append instead, keeping the same content pinned.
        a.push_line(b"f".to_vec(), t, LineFlags::default());
        a.push_line(b"g".to_vec(), t, LineFlags::default());
        assert_eq!(a.view_offset, 2 + 2, "offset grows by the visible appends");
        assert_eq!(
            window_top(&a, 2).bytes,
            b"b".to_vec(),
            "viewport top still shows \"b\""
        );
    }

    #[test]
    fn paused_viewport_filter_nonmatching_append_leaves_view_unchanged() {
        let mut a = app();
        let t = SystemTime::now();
        for text in ["err one", "ok mid", "err two"] {
            a.push_line(text.as_bytes().to_vec(), t, LineFlags::default());
        }
        a.filter_pattern = Some("err".into());
        a.follow_tail = false;
        a.view_offset = 1; // filtered [0, 2]; window bottom = "err one"
        a.push_line(b"ok tail".to_vec(), t, LineFlags::default());
        assert_eq!(a.view_offset, 1, "non-matching append adds no view line");
        assert_eq!(a.view_len(), 2);
        assert_eq!(
            window_bottom(&a).bytes,
            b"err one".to_vec(),
            "viewport still pinned to \"err one\""
        );
    }

    #[test]
    fn paused_viewport_filter_matching_append_grows_offset() {
        let mut a = app();
        let t = SystemTime::now();
        for text in ["err one", "ok mid", "err two"] {
            a.push_line(text.as_bytes().to_vec(), t, LineFlags::default());
        }
        a.filter_pattern = Some("err".into());
        a.follow_tail = false;
        a.view_offset = 1; // filtered [0, 2]; window bottom = "err one"
        a.push_line(b"err three".to_vec(), t, LineFlags::default());
        // The appended line IS visible: every existing view line moved one
        // farther from the tail, so the pinned offset grows by one.
        assert_eq!(a.view_offset, 2);
        assert_eq!(a.view_len(), 3);
        assert_eq!(
            window_bottom(&a).bytes,
            b"err one".to_vec(),
            "viewport stays pinned to \"err one\""
        );
    }

    #[test]
    fn paused_viewport_matching_evict_nonmatching_append_no_over_rebase() {
        let mut a = app();
        a.scrollback_cap = 5;
        let t = SystemTime::now();
        for text in ["hit a", "hit b", "hit c", "hit d", "pad1"] {
            a.push_line(text.as_bytes().to_vec(), t, LineFlags::default());
        }
        a.filter_pattern = Some("hit".into());
        a.follow_tail = false;
        // filtered [0, 1, 2, 3]; window bottom = filtered[len-1-1] = "hit c"
        a.view_offset = 1;
        assert_eq!(window_bottom(&a).bytes, b"hit c".to_vec());
        // Push a NON-matching line: it evicts "hit a" (MATCHING) from the
        // buffer — the filtered list loses its front element — but the
        // appended line adds nothing to the view. The old code rebased the
        // offset by the eviction count (over-rebase); eviction is
        // tail-relative-offset-neutral, so the offset must stay.
        a.push_line(b"pad2".to_vec(), t, LineFlags::default());
        assert_eq!(a.view_len(), 3, "filtered list: [hit b, hit c, hit d]");
        assert_eq!(
            a.view_offset, 1,
            "no over-rebase: eviction does not move a tail-relative offset"
        );
        assert_eq!(
            window_bottom(&a).bytes,
            b"hit c".to_vec(),
            "viewport still pinned to \"hit c\""
        );
    }

    // ------------------------------------------------------------------
    // ITEM 5: truthful TX logging (log-after-successful-send)
    // ------------------------------------------------------------------

    #[test]
    fn send_tx_closed_channel_logs_nothing_and_sets_error() {
        let mut a = app();
        let path = std::env::temp_dir()
            .join(format!("ncom-send-closed-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        a.log_open(&path).unwrap();
        a.last_error = None;
        let (tx, rx) = mpsc::channel::<ToWorker>();
        drop(rx); // worker gone
        a.send_tx(&tx, b"lost".to_vec());
        a.log_off();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            Vec::<u8>::new(),
            "a failed send must not be logged"
        );
        assert!(a.last_error.clone().unwrap().contains("port not available"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pump_sendfile_failed_send_aborts_without_counting_the_chunk() {
        let mut a = app();
        a.connected = true;
        let path = std::env::temp_dir()
            .join(format!("ncom-pump-fail-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        std::fs::write(&path, b"0123456789").unwrap();
        a.pending_sendfile = Some(PendingSendFile {
            file: std::fs::File::open(&path).unwrap(),
            remaining: 10,
            sent: 0,
            path: path.clone(),
        });
        let (tx, rx) = mpsc::channel::<ToWorker>();
        drop(rx); // worker gone mid-pump: the next chunk send must fail

        a.pump_sendfile(&tx);
        assert!(
            a.pending_sendfile.is_none(),
            "a failed chunk send must abort the transfer"
        );
        assert!(
            a.last_error.clone().unwrap().contains("port not available"),
            "send_tx error must be surfaced, got {:?}",
            a.last_error
        );
        assert!(
            a.status_msg.is_none(),
            "no completion status (and no byte count) after a failed send, got {:?}",
            a.status_msg
        );
        std::fs::remove_file(&path).ok();
    }

    // ------------------------------------------------------------------
    // ITEM 6: status freshness
    // ------------------------------------------------------------------

    #[test]
    fn successful_send_clears_stale_tx_error() {
        let mut a = app();
        // Stale error from a TX attempt while the port was DOWN (the exact
        // message input.rs/line_submit set) — a later successful send clears it.
        a.set_error("port is DOWN — :r to reconnect");
        let (tx_w, _rx_w) = mpsc::channel::<ToWorker>();
        a.send_tx(&tx_w, b"ok".to_vec());
        assert_eq!(a.last_error, None);
    }

    // ------------------------------------------------------------------
    // Transient status messages: cleared by the next keypress (main.rs calls
    // `clear_status` before routing the key) or auto-expired after
    // STATUS_TTL (`tick_status`, headless via injected Instants).
    // ------------------------------------------------------------------

    #[test]
    fn status_msg_expires_after_ttl() {
        let mut a = app();
        let t0 = Instant::now();
        a.status_msg = Some(("filter: off".into(), t0));
        // Just before the TTL it is still visible.
        a.tick_status(t0 + STATUS_TTL - Duration::from_millis(1));
        assert_eq!(a.status_text(), Some("filter: off"));
        // At the TTL it is dropped (main.rs ticks every ~25 ms iteration).
        a.tick_status(t0 + STATUS_TTL);
        assert!(a.status_msg.is_none());
        // Ticking with no message is a no-op.
        a.tick_status(t0 + STATUS_TTL * 10);
        assert!(a.status_msg.is_none());
    }

    #[test]
    fn fresh_status_resets_expiry_timer() {
        let mut a = app();
        let t0 = Instant::now();
        a.status_msg = Some(("echo: on".into(), t0));
        // A newer message (e.g. the next command's output) restamps the clock.
        let t1 = t0 + Duration::from_secs(4);
        a.status_msg = Some(("echo: off".into(), t1));
        a.tick_status(t0 + STATUS_TTL); // the OLD message's expiry: still alive
        assert_eq!(a.status_text(), Some("echo: off"));
        a.tick_status(t1 + STATUS_TTL); // the NEW message's expiry: gone
        assert!(a.status_msg.is_none());
    }

    #[test]
    fn set_status_stamps_fresh_message_and_clear_status_drops_it() {
        let mut a = app();
        a.set_status("baud: 115200");
        // A tick right after setting must NOT expire it (timer starts at set).
        a.tick_status(Instant::now());
        assert!(a.status_msg.is_some());
        // main.rs calls this before routing every Key press: the message set
        // by the previous keystroke (e.g. the Enter that ran the command) is
        // gone before the next key is handled.
        a.clear_status();
        assert!(a.status_msg.is_none());
    }

    #[test]
    fn tick_status_never_touches_last_error() {
        let mut a = app();
        a.set_error("send: no port open");
        a.status_msg = Some(("hex: on".into(), Instant::now()));
        a.tick_status(Instant::now() + STATUS_TTL * 10);
        assert!(a.status_msg.is_none());
        assert_eq!(
            a.last_error.as_deref(),
            Some("send: no port open"),
            "errors do not auto-expire"
        );
    }

    // ------------------------------------------------------------------
    // :filter view + :hl highlight state (view = filtered line list)
    // ------------------------------------------------------------------

    fn filtered_app() -> App {
        let mut a = app();
        let t = SystemTime::now();
        for text in ["err one", "ok line", "err two", "ok again", "err three"] {
            a.push_line(text.as_bytes().to_vec(), t, LineFlags::default());
        }
        a
    }

    // ------------------------------------------------------------------
    // Filtered-view cache (`filter_cache`): must always equal a fresh naive
    // scan of the scrollback, across appends, evictions and pattern flips.
    // ------------------------------------------------------------------

    /// The naive reference: a full scan, ignoring the cache entirely.
    fn naive_filtered_indices(a: &App) -> Vec<usize> {
        match &a.filter_pattern {
            Some(p) => a
                .scrollback
                .iter()
                .enumerate()
                .filter(|(_, line)| find_ci(&decode_lossy_caret(&line.bytes), p).is_some())
                .map(|(i, _)| i)
                .collect(),
            None => (0..a.scrollback.len()).collect(),
        }
    }

    #[test]
    fn filter_cache_matches_naive_scan_across_mutations() {
        let mut a = app();
        a.scrollback_cap = 6; // force evictions from push 7 on
        let t = SystemTime::now();
        let mut with_filter = false;
        for i in 0..40usize {
            let text = format!("line {i} {}", if i % 3 == 0 { "err" } else { "ok" });
            a.push_line(text.into_bytes(), t, LineFlags::default());
            // Flip between no filter and a matching pattern; ALSO assign
            // `filter_pattern` directly (bypassing `set_filter`) so the
            // readers' stale-fallback path is exercised, then let the next
            // `push_line` rebuild the cache via `ensure_filter_cache`.
            if i % 7 == 3 {
                with_filter = !with_filter;
            }
            a.filter_pattern = if with_filter {
                Some("err".into())
            } else {
                None
            };
            let expected = naive_filtered_indices(&a);
            assert_eq!(a.filtered_indices(), expected, "after push {i}");
            assert_eq!(a.view_len(), expected.len(), "view_len after push {i}");
        }
        // The `set_filter` path (what command.rs routes through) must leave
        // the cache fresh and correct, including a re-set of the same value.
        for p in [
            None,
            Some("err".to_string()),
            Some("line".to_string()),
            None,
            Some("err".to_string()),
        ] {
            a.set_filter(p);
            assert!(a.filter_cache_is_fresh(), "cache fresh after set_filter");
            assert_eq!(a.filtered_indices(), naive_filtered_indices(&a));
            assert_eq!(a.view_len(), naive_filtered_indices(&a).len());
        }
    }

    #[test]
    fn filter_cache_tracks_evictions_by_raw_front_count() {
        let mut a = app();
        a.scrollback_cap = 3;
        let t = SystemTime::now();
        for text in ["err one", "ok two", "err three"] {
            a.push_line(text.as_bytes().to_vec(), t, LineFlags::default());
        }
        a.set_filter(Some("err".into()));
        assert_eq!(a.filtered_indices(), vec![0, 2]);
        // Appending a NON-matching line evicts the matching "err one" (raw
        // front count 1): the cache drops index 0 and shifts the rest, so
        // "err three" lands at raw index 1 == view position 0.
        a.push_line(b"ok four".to_vec(), t, LineFlags::default());
        assert_eq!(a.scrollback.len(), 3);
        assert_eq!(a.filtered_indices(), vec![1]);
        assert_eq!(a.view_len(), 1);
        // Appending a MATCHING line evicts the non-matching "ok two": nothing
        // shifts (no matching eviction), and the new line appends at the back.
        a.push_line(b"err five".to_vec(), t, LineFlags::default());
        assert_eq!(a.filtered_indices(), vec![0, 2]);
        assert_eq!(a.view_len(), 2);
        // And a no-filter cache is the full index list, maintained likewise.
        a.set_filter(None);
        assert_eq!(a.filtered_indices(), vec![0, 1, 2]);
        a.push_line(b"six".to_vec(), t, LineFlags::default()); // evicts "err three"
        assert_eq!(a.filtered_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn filter_cache_view_len_fast_on_full_10k_buffer() {
        let mut a = app();
        let t = SystemTime::now();
        for i in 0..DEFAULT_SCROLLBACK {
            a.push_line(
                format!("line {i} payload").into_bytes(),
                t,
                LineFlags::default(),
            );
        }
        a.set_filter(Some("payload".into()));
        let start = std::time::Instant::now();
        let n = a.view_len();
        let elapsed = start.elapsed();
        assert_eq!(n, DEFAULT_SCROLLBACK);
        // Generous bound (CI-friendly): the cached reader is O(1); the old
        // full scan of 10k decoded lines was ~ms but per-render/per-keypress.
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "cached view_len took {elapsed:?}"
        );
    }

    #[test]
    fn filter_changes_view_len_and_find_matches_coordinates() {
        let mut a = filtered_app();
        assert_eq!(a.view_len(), 5);
        // Without a filter, matches are absolute scrollback indices.
        assert_eq!(a.find_matches("err"), vec![0, 2, 4]);

        a.filter_pattern = Some("err".into());
        assert_eq!(a.view_len(), 3);
        // With a filter, `find_matches` returns FILTERED-VIEW positions.
        assert_eq!(a.find_matches("err"), vec![0, 1, 2]);
        // Search runs INSIDE the filtered view: "ok" only appears on lines
        // the filter removed, so there are no view positions for it.
        assert_eq!(a.find_matches("ok"), Vec::<usize>::new());
        assert_eq!(a.find_matches("zzz"), Vec::<usize>::new());
        // The filter itself uses the shared case-insensitive matcher.
        a.filter_pattern = Some("ERR".into());
        assert_eq!(a.view_len(), 3);
    }

    #[test]
    fn filter_motions_operate_on_filtered_list() {
        let mut a = filtered_app();
        a.filter_pattern = Some("err".into());
        a.viewport_rows = 2;
        // k scrolls up within the 3 filtered lines, clamped at len-1.
        a.motion_up(1);
        assert_eq!(a.view_offset, 1);
        a.motion_up(5);
        assert_eq!(a.view_offset, 2, "clamped to filtered len - 1");
        a.motion_down(10);
        assert_eq!(a.view_offset, 0);
        // gg jumps to the oldest FILTERED line at the viewport top.
        a.goto_top();
        assert_eq!(a.view_offset, 1); // 3 - 2 viewport rows
        // G resumes follow-tail on the filtered tail.
        a.motion_up(1);
        a.goto_end();
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        // A filter matching nothing: motions are no-ops (follow-tail state is
        // untouched — there is no view to scroll).
        a.filter_pattern = Some("zzz".into());
        a.motion_up(3);
        assert_eq!(a.view_offset, 0);
        assert!(a.follow_tail);
        a.goto_top();
        assert_eq!(a.view_offset, 0);
    }

    #[test]
    fn search_within_filtered_view_jumps_by_view_positions() {
        let mut a = filtered_app();
        a.filter_pattern = Some("err".into());
        a.viewport_rows = 2;
        a.search_prompt = Some("two".into());
        a.exec_search();
        // "two" lives on absolute line 2 = filtered position 1.
        assert_eq!(a.last_match, Some(1));
        assert!(!a.follow_tail);
        assert_eq!(
            a.view_offset, 0,
            "viewport scrolled so the match is visible"
        );
        // n wraps within the filtered matches of "two" (a single one).
        a.search_step(true);
        assert_eq!(a.last_match, Some(1));
        // A search pattern that only matches filtered-out lines finds nothing.
        a.search_prompt = Some("ok".into());
        a.exec_search();
        assert!(a.last_error.clone().unwrap().contains("no match"));
    }

    #[test]
    fn eviction_with_active_filter_rebases_by_matching_lines_only() {
        let mut a = app();
        a.scrollback_cap = 4;
        let t = SystemTime::now();
        // Matching lines: "hit a", "hit b"; non-matching: "nope1/2".
        a.push_line(b"hit a".to_vec(), t, LineFlags::default());
        a.push_line(b"nope1".to_vec(), t, LineFlags::default());
        a.push_line(b"hit b".to_vec(), t, LineFlags::default());
        a.push_line(b"nope2".to_vec(), t, LineFlags::default());
        a.filter_pattern = Some("hit".into());
        a.search_prompt = Some("hit".into());
        a.exec_search(); // first match = filtered position 0 ("hit a")
        assert_eq!(a.last_match, Some(0));
        // Push a NON-matching line: it evicts "hit a" (a MATCHING line) from
        // the buffer, so the filtered list loses its front element — the
        // rebase counts it, clearing the match (its view slot is gone) and
        // rebasing/clamping the offset by one.
        a.follow_tail = false;
        a.view_offset = 1; // pinned at the oldest filtered line ("hit a")
        a.push_line(b"nope3".to_vec(), t, LineFlags::default());
        assert_eq!(
            a.view_len(),
            1,
            "filtered list: [hit b] — only hit a was visible and evicted"
        );
        assert_eq!(a.last_match, None, "the match line itself left the view");
        assert_eq!(a.view_offset, 0, "clamped to filtered len - 1");
        // Now push a MATCHING line: the eviction takes the oldest buffer line
        // ("nope1", NON-matching — no index/`last_match` rebase), but the
        // appended VISIBLE line moves the remaining content one line farther
        // from the tail, so the pinned offset grows by one and the viewport
        // still shows "hit b". (The old expectation of 0 was built on the
        // eviction rule; the tail-relative offset follows APPENDS.)
        a.push_line(b"hit c".to_vec(), t, LineFlags::default());
        assert_eq!(a.view_len(), 2, "filtered list: [hit b, hit c]");
        assert_eq!(
            a.view_offset, 1,
            "visible append: offset +1, viewport still pinned to \"hit b\""
        );
        let filtered = a.filtered_indices();
        assert_eq!(
            a.scrollback[filtered[filtered.len() - 1 - a.view_offset]].bytes,
            b"hit b".to_vec(),
            "viewport content is unchanged"
        );
        a.search_step(true); // no last_match: steps to the first match
        assert_eq!(a.last_match, Some(0));
        a.search_step(true);
        assert_eq!(a.last_match, Some(1), "n reaches the new tail match");
    }

    #[test]
    fn hl_state_is_independent_of_filter() {
        let mut a = filtered_app();
        a.hl_pattern = Some("one".into());
        a.filter_pattern = Some("err".into());
        assert!(a.hl_pattern.is_some() && a.filter_pattern.is_some());
        // Clearing one leaves the other active.
        a.hl_pattern = None;
        assert!(a.filter_pattern.is_some());
    }

    fn app_with_lines(n: usize) -> App {
        let mut a = app();
        let t = SystemTime::now();
        for i in 0..n {
            a.push_line(format!("line{i}").into_bytes(), t, LineFlags::default());
        }
        a
    }

    #[test]
    fn serial_config_display_fields() {
        use crate::serial::{NcomFlow, NcomParity, NcomStopBits, SerialConfig};
        let cfg = SerialConfig {
            path: "/dev/ttyUSB0".into(),
            baud: 115_200,
            data_bits: 8,
            parity: NcomParity::None,
            stop_bits: NcomStopBits::One,
            flow: NcomFlow::None,
        };
        assert_eq!(cfg.baud, 115_200);
    }
}
