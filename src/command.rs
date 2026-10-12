//! `:` command parser and execution (ex-style).
//!
//! Parsing: the command name is the first whitespace-separated token and is
//! case-sensitive lowercase (vi-consistent: `:Q` is not `:q`); keyword
//! arguments are case-insensitive; file paths and `send`/`sendhex` payloads
//! are verbatim (trimmed, interior whitespace preserved). Unknown/invalid
//! commands error in the status line and Enter always returns to NORMAL.
//! Values the serialport crate cannot represent (`parity mark|space`,
//! `stopbits 1.5`) are rejected before `App::port_cfg` changes, via the
//! mapping functions in `serial.rs`.
//!
//! `connect`/`r` respawn the worker through the same `SerialWorker::spawn`
//! path main.rs uses for `--port`; reconnecting drops `App::tx`/`App::rx`,
//! and the old worker stops itself within one read timeout (no explicit stop
//! message).
//!
//! `:log`, `sendfile` and `:ports` semantics live with their implementations:
//! `App::log_open` (ordering rules in logger.rs), `send_file` (capped at
//! `SENDFILE_MAX`), `scan_ports`.

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::app::{App, InsertStyle, SENDFILE_MAX};
use crate::serial::{
    NcomFlow, NcomParity, NcomStopBits, SerialConfig, SerialWorker, ToWorker, TxNewline,
    decode_lossy_caret, map_data_bits, map_flow, map_parity, map_stop_bits,
};

/// Default `:ports` scan roots (`/dev/serial/*`, `/sys/class/tty`).
pub fn default_port_roots() -> (Vec<PathBuf>, PathBuf) {
    (
        vec![
            PathBuf::from("/dev/serial/by-id"),
            PathBuf::from("/dev/serial/by-path"),
        ],
        PathBuf::from("/sys/class/tty"),
    )
}

/// One parsed `:` command. `parse` validates syntax; argument values are kept
/// in their app-level form (`NcomParity`, ...) after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `q` (force=false, refuses while a log is active) / `q!` (force=true).
    Quit {
        force: bool,
    },
    /// `w <file>` — dump scrollback to `file`, refusing to overwrite an
    /// existing non-empty file; `w! <file>` — overwrite without asking.
    Write {
        path: String,
        force: bool,
    },
    /// `log <path>` / `log off`.
    Log {
        off: bool,
        path: String,
    },
    /// `hex on|off` / `ts on|off` / `line on|off`.
    Toggle {
        flag: ToggleFlag,
        on: bool,
    },
    /// `txnl cr|lf|crlf`.
    Txnl(TxNewline),
    /// `baud [<n>]` — `None` = show current.
    Baud(Option<u32>),
    DataBits(u8),
    Parity(NcomParity),
    StopBits(NcomStopBits),
    Flow(NcomFlow),
    Dtr(bool),
    Rts(bool),
    /// `ports` — list candidate serial devices.
    Ports,
    /// `connect <dev> [<baud>]`.
    Connect {
        dev: String,
        baud: Option<u32>,
    },
    /// `r` — reconnect with the last port + settings.
    Reconnect,
    /// `send <text>` — text plus the `:txnl` ending is transmitted.
    Send(String),
    /// `sendhex <hex>` — decoded bytes are transmitted raw.
    SendHex(Vec<u8>),
    /// `sendfile <path>` — the file's raw bytes are transmitted verbatim
    /// (no `:txnl` ending appended; unlike `send`).
    SendFile(String),
    /// `hl <pattern>` — highlight matches in the text view; `hl off` clears;
    /// `hl` (no arg) shows the current pattern. `None` = no-argument query
    /// form (`off` is kept as the literal argument and interpreted by the
    /// executor, so the parser stays a pure syntax check).
    Highlight(Option<String>),
    /// `echo on|off` — local echo: show successfully sent TX lines in the
    /// scrollback; `echo` (no arg) shows the current state. `None` =
    /// no-argument query form (`on`/`off` are kept as literal arguments and
    /// validated by the executor, so the parser stays a pure syntax check;
    /// only exact lowercase `off` disables — same rule as `:hl`/`:filter`).
    Echo(Option<String>),
    /// `filter <pattern>` — show only matching lines (a view over the
    /// scrollback; the buffer itself is retained); `filter off` clears;
    /// `filter` (no arg) shows the current pattern.
    Filter(Option<String>),
    /// `help` — open the centered command-list popup (any key closes it).
    Help,
    /// `noh` — clear ONLY the `/` search state (`last_search`/`last_match`);
    /// `hl`/`filter` patterns, the view position and everything else are
    /// untouched (vi `:noh[lsearch]`).
    Noh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleFlag {
    Hex,
    Ts,
    Line,
}

/// Parse a command line (without the leading `:`). See module docs for the
/// case/whitespace decisions.
pub fn parse(line: &str) -> Result<Command, String> {
    let line = line.trim();
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (line, ""),
    };
    match word {
        "q" => no_args(rest).map(|_| Command::Quit { force: false }),
        "q!" => no_args(rest).map(|_| Command::Quit { force: true }),
        "w" => required_arg(rest, "w <file>").map(|f| Command::Write {
            path: f.into(),
            force: false,
        }),
        "w!" => required_arg(rest, "w! <file>").map(|f| Command::Write {
            path: f.into(),
            force: true,
        }),
        "log" => {
            let path = required_arg(rest, "log <path>|log off")?;
            // Only the exact lowercase `off` is the toggle; `OFF`, `Off` etc.
            // are legitimate (case-sensitive) filenames.
            Ok(Command::Log {
                off: path == "off",
                path: path.into(),
            })
        }
        "hex" => toggle(rest).map(|on| Command::Toggle {
            flag: ToggleFlag::Hex,
            on,
        }),
        "ts" => toggle(rest).map(|on| Command::Toggle {
            flag: ToggleFlag::Ts,
            on,
        }),
        "line" => toggle(rest).map(|on| Command::Toggle {
            flag: ToggleFlag::Line,
            on,
        }),
        "txnl" => {
            let v = required_arg(rest, "txnl cr|lf|crlf")?;
            match v.to_ascii_lowercase().as_str() {
                "cr" => Ok(Command::Txnl(TxNewline::Cr)),
                "lf" => Ok(Command::Txnl(TxNewline::Lf)),
                "crlf" => Ok(Command::Txnl(TxNewline::CrLf)),
                other => Err(format!(
                    "txnl: invalid value '{other}' (expected cr|lf|crlf)"
                )),
            }
        }
        "baud" => match rest.parse::<u32>() {
            Ok(n) if n > 0 => Ok(Command::Baud(Some(n))),
            Ok(_) => Err("baud: rate must be > 0".into()),
            Err(_) if rest.is_empty() => Ok(Command::Baud(None)),
            Err(_) => Err(format!("baud: invalid rate '{rest}'")),
        },
        "databits" => {
            let v = required_arg(rest, "databits 5|6|7|8")?;
            v.parse::<u8>()
                .ok()
                .and_then(|d| map_data_bits(d).ok().map(|_| d))
                .map(Command::DataBits)
                .ok_or_else(|| format!("databits: invalid value '{v}' (expected 5-8)"))
        }
        "parity" => {
            let v = required_arg(rest, "parity none|even|odd|mark|space")?;
            match v.to_ascii_lowercase().as_str() {
                "none" => Ok(Command::Parity(NcomParity::None)),
                "even" => Ok(Command::Parity(NcomParity::Even)),
                "odd" => Ok(Command::Parity(NcomParity::Odd)),
                "mark" => Ok(Command::Parity(NcomParity::Mark)),
                "space" => Ok(Command::Parity(NcomParity::Space)),
                other => Err(format!("parity: invalid value '{other}'")),
            }
        }
        "stopbits" => {
            let v = required_arg(rest, "stopbits 1|1.5|2")?;
            match v.to_ascii_lowercase().as_str() {
                "1" => Ok(Command::StopBits(NcomStopBits::One)),
                "1.5" => Ok(Command::StopBits(NcomStopBits::OneAndHalf)),
                "2" => Ok(Command::StopBits(NcomStopBits::Two)),
                other => Err(format!("stopbits: invalid value '{other}'")),
            }
        }
        "flow" => {
            let v = required_arg(rest, "flow none|xonz|rtscts")?;
            match v.to_ascii_lowercase().as_str() {
                "none" => Ok(Command::Flow(NcomFlow::None)),
                "xonz" => Ok(Command::Flow(NcomFlow::XonXoff)),
                "rtscts" => Ok(Command::Flow(NcomFlow::RtsCts)),
                other => Err(format!("flow: invalid value '{other}'")),
            }
        }
        "dtr" => bool_arg(rest, "dtr").map(Command::Dtr),
        "rts" => bool_arg(rest, "rts").map(Command::Rts),
        "ports" => no_args(rest).map(|_| Command::Ports),
        // `c`: exact-lowercase alias for `connect` (vi-style brevity, same
        // behavior).
        "c" | "connect" => {
            let mut it = rest.split_whitespace();
            let dev = it.next().ok_or("usage: connect <dev> [<baud>]")?;
            let baud = match it.next() {
                None => None,
                Some(b) => Some(
                    b.parse::<u32>()
                        .ok()
                        .filter(|&n| n > 0)
                        .ok_or_else(|| format!("connect: invalid baud '{b}'"))?,
                ),
            };
            if it.next().is_some() {
                return Err("usage: connect <dev> [<baud>]".into());
            }
            Ok(Command::Connect {
                dev: dev.into(),
                baud,
            })
        }
        "r" => no_args(rest).map(|_| Command::Reconnect),
        "send" => {
            let text = required_arg(rest, "send <text>")?;
            Ok(Command::Send(text.into()))
        }
        "sendhex" => {
            let hex = required_arg(rest, "sendhex <hex>")?;
            parse_hex(hex).map(Command::SendHex)
        }
        "sendfile" => required_arg(rest, "sendfile <path>").map(|p| Command::SendFile(p.into())),
        "hl" => Ok(Command::Highlight(pattern_arg(rest))),
        "echo" => Ok(Command::Echo(pattern_arg(rest))),
        "filter" => Ok(Command::Filter(pattern_arg(rest))),
        "help" => no_args(rest).map(|_| Command::Help),
        "noh" => no_args(rest).map(|_| Command::Noh),
        "" => Err("empty command".into()),
        other => Err(format!("not an editor command: {other}")),
    }
}

fn no_args(rest: &str) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!("trailing argument '{rest}'"))
    }
}

/// Trailing argument for `hl`/`filter`/`echo`: an empty (or whitespace-only,
/// already trimmed by `parse`) remainder is the no-argument query form
/// (`None`); anything else is the argument verbatim. `off` stays a literal
/// here and is interpreted as "clear" by the executor (same
/// exact-lowercase-`off` rule as `:log`, so `OFF`/`Off` are legitimate
/// case-sensitive patterns).
fn pattern_arg(rest: &str) -> Option<String> {
    if rest.is_empty() {
        None
    } else {
        Some(rest.into())
    }
}

fn required_arg<'a>(rest: &'a str, usage: &str) -> Result<&'a str, String> {
    if rest.is_empty() {
        Err(format!("usage: {usage}"))
    } else {
        Ok(rest)
    }
}

/// `on|off`, case-insensitive; anything else (incl. missing) is an error.
fn toggle(rest: &str) -> Result<bool, String> {
    match rest.to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected on|off".into()),
    }
}

/// `0|1` modem-line level; missing arg is an error.
fn bool_arg(rest: &str, name: &str) -> Result<bool, String> {
    match rest {
        "0" => Ok(false),
        "1" => Ok(true),
        "" => Err(format!("usage: {name} 0|1")),
        other => Err(format!("{name}: invalid level '{other}' (expected 0|1)")),
    }
}

/// Decode a hex string, tolerating whitespace between digits. Requires an even
/// number of hex digits; any other character (including any non-ASCII
/// character) is an error.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.is_ascii() {
        return Err(format!("sendhex: invalid hex digits '{s}'"));
    }
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err("sendhex: odd number of hex digits".into());
    }
    let bytes = cleaned.as_bytes();
    // Length is even, so the remainder slice is empty.
    let (pairs, rest) = bytes.as_chunks::<2>();
    debug_assert!(rest.is_empty());
    pairs
        .iter()
        .map(|pair| {
            let hex = std::str::from_utf8(pair).expect("input is UTF-8");
            u8::from_str_radix(hex, 16).map_err(|_| format!("sendhex: invalid hex digits '{hex}'"))
        })
        .collect()
}

/// Execute a parsed command. `tx` mirrors `App::tx` (`None` until a port is
/// open); `connect`/`r` may install a fresh channel pair on the app.
pub fn execute(cmd: Command, app: &mut App, tx: Option<&mpsc::Sender<ToWorker>>) {
    // Freshness: a new command replaces the previous command's output — the
    // informational status AND any stale error are dropped up front. Every
    // failing arm below (audited) sets its OWN error after this clear, so a
    // failing command keeps its error visible — even when it repeats the
    // previous failure verbatim (e.g. `:send x` twice with the port down).
    app.last_error = None;
    app.clear_status();
    match cmd {
        Command::Quit { force } => {
            if !force && app.log_on {
                app.set_error("log active — use :q! to quit");
            } else {
                app.quit = true;
            }
        }
        Command::Write { path, force } => write_scrollback(app, &path, force),
        Command::Log { off, path } => {
            if off {
                if app.log_on {
                    app.log_off();
                    app.set_status("log: off");
                } else {
                    app.set_status("log: not active");
                }
            } else {
                match app.log_open(&path) {
                    Ok(()) => app.set_status(format!("log: {path}")),
                    Err(e) => app.set_error(format!("log {path}: {e}")),
                }
            }
        }
        Command::Toggle { flag, on } => match flag {
            ToggleFlag::Hex => app.hex_on = on,
            ToggleFlag::Ts => app.ts_on = on,
            ToggleFlag::Line => {
                app.insert_style = if on {
                    InsertStyle::Line
                } else {
                    InsertStyle::Raw
                }
            }
        },
        Command::Txnl(nl) => app.txnl = nl,
        Command::Baud(None) => match &app.port_cfg {
            Some(cfg) => app.set_status(format!("baud: {}", cfg.baud)),
            None => app.set_error("baud: not connected"),
        },
        Command::Baud(Some(n)) => reconfigure(app, tx, "baud", |cfg| {
            cfg.baud = n;
            None
        }),
        Command::DataBits(d) => reconfigure(app, tx, "databits", |cfg| {
            cfg.data_bits = d;
            None
        }),
        Command::Parity(p) => reconfigure(app, tx, "parity", |cfg| {
            if let Err(e) = map_parity(p) {
                return Some(e);
            }
            cfg.parity = p;
            None
        }),
        Command::StopBits(s) => reconfigure(app, tx, "stopbits", |cfg| {
            if let Err(e) = map_stop_bits(s) {
                return Some(e);
            }
            cfg.stop_bits = s;
            None
        }),
        Command::Flow(f) => reconfigure(app, tx, "flow", |cfg| {
            if let Err(e) = map_flow(f) {
                return Some(e);
            }
            cfg.flow = f;
            None
        }),
        Command::Dtr(level) => pin(app, tx, ToWorker::Dtr(level), "dtr"),
        Command::Rts(level) => pin(app, tx, ToWorker::Rts(level), "rts"),
        Command::Ports => {
            let (dev_dirs, sysfs) = default_port_roots();
            let ports = scan_ports(&dev_dirs, &sysfs);
            app.set_status(if ports.is_empty() {
                "ports: none found".to_string()
            } else {
                format!("ports: {}", ports.join(" "))
            });
        }
        Command::Connect { dev, baud } => {
            let mut cfg = app.port_cfg.clone().unwrap_or_else(default_serial_config);
            cfg.path = dev;
            if let Some(b) = baud {
                cfg.baud = b;
            }
            connect(app, cfg);
        }
        Command::Reconnect => match app.port_cfg.clone() {
            Some(cfg) => connect(app, cfg),
            None => app.set_error("r: never connected"),
        },
        Command::Send(text) => match (app.connected, tx) {
            (true, Some(tx)) => {
                let bytes = app.txnl.encode_line(&text);
                app.send_tx(tx, bytes);
            }
            (true, None) => app.set_error("send: no port open"),
            (false, _) => app.set_error("send: not connected"),
        },
        Command::SendHex(bytes) => match (app.connected, tx) {
            (true, Some(tx)) => {
                app.send_tx(tx, bytes);
            }
            (true, None) => app.set_error("sendhex: no port open"),
            (false, _) => app.set_error("sendhex: not connected"),
        },
        Command::SendFile(path) => match (app.connected, tx) {
            (true, Some(_tx)) => send_file(app, &path),
            (true, None) => app.set_error("sendfile: no port open"),
            (false, _) => app.set_error("sendfile: not connected"),
        },
        Command::Help => {
            app.help_visible = true;
            app.help_page = 0;
        }
        Command::Noh => {
            // vi `:nohlsearch`: clear only the `/` search state — the current
            // search pattern and its match position. `hl`/`filter` patterns,
            // the view offset and everything else stay untouched.
            app.last_search = None;
            app.last_match = None;
            app.set_status("noh: search cleared");
        }
        Command::Highlight(Some(p)) => {
            // Only exact lowercase `off` clears (same rule as `:log`); an
            // empty pattern (unreachable through `parse`, which trims — kept
            // as a guard for directly-constructed commands) is rejected: the
            // empty pattern matches nothing and would silently blank the view.
            if p == "off" {
                app.hl_pattern = None;
                app.set_status("hl: off");
            } else if p.is_empty() {
                app.set_error("hl: empty pattern");
            } else {
                app.hl_pattern = Some(p.clone());
                app.set_status(format!("hl: {p}"));
            }
        }
        Command::Highlight(None) => app.set_status(match &app.hl_pattern {
            Some(p) => format!("hl: {p}"),
            None => "hl: off".to_string(),
        }),
        Command::Echo(Some(v)) => {
            // Only exact lowercase `on`/`off` (same rule as `:hl`/`:filter`
            // for `off`); anything else — including `ON`/`OFF` — is invalid:
            // echo has no patterns, so there is no case-variant meaning.
            match v.as_str() {
                "on" => {
                    app.echo_on = true;
                    app.set_status("echo: on");
                }
                "off" => {
                    app.echo_on = false;
                    app.set_status("echo: off");
                }
                other => app.set_error(format!(
                    "echo: invalid value '{other}' (expected on|off)"
                )),
            }
        }
        Command::Echo(None) => app.set_status(if app.echo_on {
            "echo: on"
        } else {
            "echo: off"
        }),
        Command::Filter(Some(p)) => {
            if p == "off" {
                // Leaving the filtered view switches `last_match` coordinates
                // (filtered-view position -> absolute scrollback index), so a
                // real transition (filter was on) drops it. `last_search` is
                // KEPT: pressing `n` re-runs the search in the restored
                // (unfiltered) view coordinates. `set_filter` is the single
                // `filter_pattern` mutation point — it rebuilds the filtered
                // index cache in the same step.
                let had_filter = app.filter_pattern.is_some();
                app.set_filter(None);
                if had_filter {
                    app.last_match = None;
                }
                // `filter off` restores the FULL view; snap back to the live
                // tail so a stale tail-relative offset cannot point past the
                // (now larger) line list.
                app.goto_end();
                app.set_status("filter: off");
            } else if p.is_empty() {
                app.set_error("filter: empty pattern");
            } else {
                // Entering or changing a filter rebuilds the filtered view, so
                // a previous `last_match` is in the wrong coordinates — a real
                // transition (filter was off, or the pattern value changed)
                // drops it. Re-issuing the SAME pattern is not a transition.
                // `last_search` is KEPT: pressing `n` re-runs the search
                // inside the new filtered view coordinates.
                let changed = app.filter_pattern.as_deref() != Some(p.as_str());
                app.set_filter(Some(p.clone()));
                if changed {
                    app.last_match = None;
                }
                // Entering a filter snaps to the filtered tail: the previous
                // tail-relative offset belonged to the unfiltered list.
                app.goto_end();
                app.set_status(format!("filter: {p} ({} lines)", app.view_len()));
            }
        }
        Command::Filter(None) => app.set_status(match &app.filter_pattern {
            Some(p) => format!("filter: {p}"),
            None => "filter: off".to_string(),
        }),
    }
}

/// Settings used by `connect` when nothing was configured before: the same
/// defaults as the CLI (`--baud 115200 --databits 8 --parity none
/// --stopbits 1 --flow none`).
fn default_serial_config() -> SerialConfig {
    SerialConfig {
        path: String::new(),
        baud: 115_200,
        data_bits: 8,
        parity: NcomParity::None,
        stop_bits: NcomStopBits::One,
        flow: NcomFlow::None,
    }
}

/// Apply a mutation to `App::port_cfg` through `f`; `f` returns `Some(err)` to
/// reject the change (unsupported value). On success the new settings go to
/// the worker as `ToWorker::Config` when connected; `App::port_cfg` is only
/// updated after validation AND after the worker send succeeded, so neither
/// unsupported values nor a dead worker channel ever change the top bar
/// (`name` is used in the failure message, e.g. `baud: port not available`).
fn reconfigure(
    app: &mut App,
    tx: Option<&mpsc::Sender<ToWorker>>,
    name: &str,
    f: impl FnOnce(&mut SerialConfig) -> Option<String>,
) {
    let Some(mut cfg) = app.port_cfg.clone() else {
        app.set_error(format!("{name}: not connected"));
        return;
    };
    if let Some(err) = f(&mut cfg) {
        app.set_error(err);
        return;
    }
    if app.connected {
        if let Some(tx) = tx {
            if tx
                .send(ToWorker::Config {
                    baud: cfg.baud,
                    data_bits: cfg.data_bits,
                    parity: cfg.parity,
                    stop_bits: cfg.stop_bits,
                    flow: cfg.flow,
                })
                .is_err()
            {
                // Worker gone: keep the old config so the top bar stays
                // truthful about what the port is actually running.
                app.set_error(format!("{name}: port not available"));
                return;
            }
        } else {
            app.set_error("no port open");
            return;
        }
    }
    app.port_cfg = Some(cfg);
}

/// Send a modem-line message; requires an open port. A failed send (receiver
/// dropped / worker gone) surfaces an error instead of being discarded.
fn pin(app: &mut App, tx: Option<&mpsc::Sender<ToWorker>>, msg: ToWorker, name: &str) {
    match (app.connected, tx) {
        (true, Some(tx)) => {
            if tx.send(msg).is_err() {
                app.set_error(format!("{name}: port not available"));
            }
        }
        (true, None) => app.set_error(format!("{name}: no port open")),
        (false, _) => app.set_error(format!("{name}: not connected")),
    }
}

/// (Re)connect: drop any existing worker channel pair (see module docs — the
/// old worker stops itself within one read timeout) and spawn a fresh one via
/// the same `SerialWorker::spawn` path main.rs uses for `--port`.
fn connect(app: &mut App, cfg: SerialConfig) {
    app.tx = None;
    app.rx = None;
    app.connected = false;
    // The old worker channel is gone, so an in-flight sendfile must not
    // continue its remaining chunks onto the NEW port (pump_sendfile doc):
    // abort it here.
    if app.pending_sendfile.take().is_some() {
        app.set_status("sendfile: aborted (reconnect)");
    }
    match SerialWorker::spawn(cfg.clone()) {
        Ok((tx, rx)) => {
            app.port_cfg = Some(cfg);
            app.tx = Some(tx);
            app.rx = Some(rx);
            app.connected = true;
            // A successful (re)connect clears the stale disconnect error so
            // the status line reflects the current state.
            app.last_error = None;
        }
        Err(e) => {
            app.port_cfg = Some(cfg);
            app.set_error(format!("connect failed: {e}"));
        }
    }
}

/// `w <file>`: dump the scrollback, one decoded line (caret notation, same
/// decode as the renderer) + `\n` each. Dumps the FULL retained scrollback —
/// an active `:filter` view never affects what `:w` writes (the filter is a
/// view; the buffer is untouched). An existing file is refused (use `w!` to
/// overwrite) — the guard is `OpenOptions::create_new`, so the check and the
/// open are one atomic step with no check-then-create TOCTOU window. `w!`
/// opens with `create(true).truncate(true)` instead. The path is opened
/// normally, so it follows an existing symlink vi-style (the target is
/// written, not replaced). On failure the OS error goes to the status line.
/// Note: a write error partway through the loop leaves a partially-written
/// file — bytes already flushed cannot be retracted.
fn write_scrollback(app: &mut App, path: &str, force: bool) {
    let file = if force {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let file = match file {
        Ok(f) => f,
        // Atomic create_new hit an existing file: the same refusal guard the
        // old metadata() pre-check produced.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            app.set_error(format!("w {path}: exists — use :w! to overwrite"));
            return;
        }
        Err(e) => {
            app.set_error(format!("w {path}: {e}"));
            return;
        }
    };
    let mut out = std::io::BufWriter::new(file);
    let mut n = 0usize;
    let mut err = None;
    for line in &app.scrollback {
        let res = out
            .write_all(decode_lossy_caret(&line.bytes).as_bytes())
            .and_then(|()| out.write_all(b"\n"));
        if let Err(e) = res {
            err = Some(e);
            break;
        }
        n += 1;
    }
    let res = match err {
        Some(e) => Err(e),
        None => out.flush(),
    };
    if let Err(e) = res {
        app.set_error(format!("w {path}: {e}"));
        return;
    }
    app.set_status(format!("w: wrote {n} lines to {path}"));
}

/// `sendfile <path>`: set up a pending transfer of `path`'s contents as RAW
/// BYTES (binary-safe: no newline translation, no `:txnl` ending appended —
/// unlike `:send`). The actual sending happens off the UI critical path: this
/// only opens the file and installs [`crate::app::PendingSendFile`] state on
/// the app; main.rs pumps one 4 KiB chunk per main-loop iteration via
/// [`App::pump_sendfile`], so a multi-megabyte transfer never stalls
/// rendering. Chunks go through [`App::send_tx`], so each chunk is recorded by
/// the session logger at send time via the same ordering hook as
/// send/sendhex — the log byte order matches the send order.
///
/// Nothing is sent (or logged) unless every precondition holds: a missing or
/// unreadable file is an error; a file larger than [`SENDFILE_MAX`] is
/// rejected outright; a transfer already in progress refuses a second
/// `:sendfile` rather than interleaving. A read error mid-file aborts the
/// transfer (bytes already queued stay queued — there is no unsend); an empty
/// file completes on the first pump reporting 0 bytes.
fn send_file(app: &mut App, path: &str) {
    if app.pending_sendfile.is_some() {
        app.set_error("sendfile: already in progress");
        return;
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            app.set_error(format!("sendfile {path}: {e}"));
            return;
        }
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            app.set_error(format!("sendfile {path}: {e}"));
            return;
        }
    };
    if len > SENDFILE_MAX {
        app.set_error(format!(
            "sendfile {path}: too large (> {} MiB)",
            SENDFILE_MAX / (1024 * 1024)
        ));
        return;
    }
    app.pending_sendfile = Some(crate::app::PendingSendFile {
        file,
        remaining: len,
        sent: 0,
        path: path.into(),
    });
}

/// List candidate serial devices from udev by-id/by-path dirs (resolve each
/// symlink to the `/dev` node name) and the sysfs tty class dir (entries with
/// a `device` link). Sorted, deduplicated. Errors on individual roots (missing
/// dir, unreadable entry) are skipped — a missing `/dev/serial` on a machine
/// without udev symlinks is normal.
pub fn scan_ports(dev_link_dirs: &[PathBuf], sysfs_tty_dir: &Path) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    for dir in dev_link_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Canonicalize the LINK ITSELF: udev's by-id/by-path entries are
            // symlinks with RELATIVE targets (`../../ttyUSB0`); canonicalizing
            // the read_link target would resolve it against the process CWD
            // and miss. `canonicalize(entry.path())` follows the link in place
            // to the real /dev node (dangling links just error and are
            // skipped).
            if let Ok(real) = std::fs::canonicalize(entry.path())
                && let Some(name) = real.file_name()
            {
                found.insert(format!("/dev/{}", name.to_string_lossy()));
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(sysfs_tty_dir) {
        for entry in entries.flatten() {
            if is_real_tty(&entry.path()) {
                found.insert(format!("/dev/{}", entry.file_name().to_string_lossy()));
            }
        }
    }
    found.into_iter().collect()
}

/// Filter predicate for `/sys/class/tty` entries: a hardware/driver-backed tty
/// exposes a `device` link in its sysfs directory; pure console/virtual entries
/// do not (same rule as serialport 4.10.1 `src/posix/enumerate.rs`).
pub fn is_real_tty(sysfs_entry: &Path) -> bool {
    sysfs_entry.join("device").symlink_metadata().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::mpsc;

    fn app() -> App {
        App::new(&Config::default())
    }

    fn connected_app() -> (App, mpsc::Receiver<ToWorker>, mpsc::Sender<ToWorker>) {
        let mut a = app();
        a.port_cfg = Some(SerialConfig {
            path: "/dev/ttyUSB0".into(),
            baud: 115_200,
            data_bits: 8,
            parity: NcomParity::None,
            stop_bits: NcomStopBits::One,
            flow: NcomFlow::None,
        });
        a.connected = true;
        let (tx, rx) = mpsc::channel();
        a.tx = Some(tx.clone());
        (a, rx, tx)
    }

    // ------------------------------------------------------------------
    // Parser: every command, bad args, case sensitivity
    // ------------------------------------------------------------------

    #[test]
    fn parses_quit_variants() {
        assert_eq!(parse("q"), Ok(Command::Quit { force: false }));
        assert_eq!(parse("q!"), Ok(Command::Quit { force: true }));
        assert!(parse("q x").is_err());
        assert!(parse("q! x").is_err());
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn parses_write_log_toggles_txnl() {
        assert_eq!(
            parse("w /tmp/dump.txt"),
            Ok(Command::Write {
                path: "/tmp/dump.txt".into(),
                force: false
            })
        );
        assert_eq!(
            parse("w! /tmp/dump.txt"),
            Ok(Command::Write {
                path: "/tmp/dump.txt".into(),
                force: true
            })
        );
        assert!(parse("w").is_err(), "w without arg must error");
        assert!(parse("w!").is_err(), "w! without arg must error");
        assert!(parse("w  ").is_err());

        assert_eq!(
            parse("log /tmp/session.log"),
            Ok(Command::Log {
                off: false,
                path: "/tmp/session.log".into()
            })
        );
        assert_eq!(
            parse("log off"),
            Ok(Command::Log {
                off: true,
                path: "off".into()
            })
        );
        // Only exact lowercase `off` toggles: `OFF`/`Off` are filenames.
        assert_eq!(
            parse("log OFF"),
            Ok(Command::Log {
                off: false,
                path: "OFF".into()
            })
        );
        assert_eq!(
            parse("log Off"),
            Ok(Command::Log {
                off: false,
                path: "Off".into()
            })
        );
        assert!(parse("log").is_err());

        assert_eq!(
            parse("hex on"),
            Ok(Command::Toggle {
                flag: ToggleFlag::Hex,
                on: true
            })
        );
        assert_eq!(
            parse("ts OFF"),
            Ok(Command::Toggle {
                flag: ToggleFlag::Ts,
                on: false
            })
        );
        assert_eq!(
            parse("line on"),
            Ok(Command::Toggle {
                flag: ToggleFlag::Line,
                on: true
            })
        );
        assert!(parse("hex").is_err());
        assert!(parse("hex maybe").is_err());

        assert_eq!(parse("txnl crlf"), Ok(Command::Txnl(TxNewline::CrLf)));
        assert!(parse("txnl").is_err());
        assert!(parse("txnl nl").is_err());
    }

    #[test]
    fn parses_serial_config_commands() {
        assert_eq!(parse("baud"), Ok(Command::Baud(None)));
        assert_eq!(parse("baud 9600"), Ok(Command::Baud(Some(9600))));
        assert!(parse("baud 0").is_err());
        assert!(parse("baud fast").is_err());

        assert_eq!(parse("databits 7"), Ok(Command::DataBits(7)));
        assert!(parse("databits 9").is_err(), "databits must validate 5-8");
        assert!(parse("databits").is_err());

        assert_eq!(parse("parity even"), Ok(Command::Parity(NcomParity::Even)));
        assert_eq!(parse("parity mark"), Ok(Command::Parity(NcomParity::Mark)));
        assert!(parse("parity").is_err());
        assert!(parse("parity flaky").is_err());

        assert_eq!(
            parse("stopbits 2"),
            Ok(Command::StopBits(NcomStopBits::Two))
        );
        assert_eq!(
            parse("stopbits 1.5"),
            Ok(Command::StopBits(NcomStopBits::OneAndHalf))
        );
        assert!(parse("stopbits").is_err());
        assert!(parse("stopbits 3").is_err());

        assert_eq!(parse("flow rtscts"), Ok(Command::Flow(NcomFlow::RtsCts)));
        assert!(parse("flow").is_err());
        assert!(parse("flow hardware").is_err());
    }

    #[test]
    fn parses_pins_ports_connect_reconnect() {
        assert_eq!(parse("dtr 1"), Ok(Command::Dtr(true)));
        assert_eq!(parse("dtr 0"), Ok(Command::Dtr(false)));
        assert!(parse("dtr").is_err());
        assert!(parse("dtr on").is_err());
        assert_eq!(parse("rts 0"), Ok(Command::Rts(false)));
        assert!(parse("rts").is_err());

        assert_eq!(parse("ports"), Ok(Command::Ports));
        assert!(parse("ports extra").is_err());

        assert_eq!(
            parse("connect /dev/ttyACM0"),
            Ok(Command::Connect {
                dev: "/dev/ttyACM0".into(),
                baud: None
            })
        );
        assert_eq!(
            parse("connect /dev/ttyACM0 921600"),
            Ok(Command::Connect {
                dev: "/dev/ttyACM0".into(),
                baud: Some(921_600)
            })
        );
        assert!(parse("connect").is_err());
        assert!(parse("connect dev fast").is_err());
        assert!(parse("connect dev 1 2").is_err());

        // `c` alias: identical parsing to `connect`.
        assert_eq!(
            parse("c /dev/ttyUSB0"),
            Ok(Command::Connect {
                dev: "/dev/ttyUSB0".into(),
                baud: None
            })
        );
        assert_eq!(
            parse("c /dev/ttyUSB0 9600"),
            Ok(Command::Connect {
                dev: "/dev/ttyUSB0".into(),
                baud: Some(9600)
            })
        );
        assert!(parse("c").is_err());

        assert_eq!(parse("r"), Ok(Command::Reconnect));
        assert!(parse("r now").is_err());
    }

    #[test]
    fn parses_send_and_sendhex() {
        assert_eq!(
            parse("send hello world"),
            Ok(Command::Send("hello world".into()))
        );
        assert!(parse("send").is_err());
        assert_eq!(
            parse("sendhex deadbeef"),
            Ok(Command::SendHex(vec![0xde, 0xad, 0xbe, 0xef]))
        );
        assert!(parse("sendhex").is_err());
    }

    #[test]
    fn parses_sendfile() {
        assert_eq!(
            parse("sendfile /tmp/firmware.bin"),
            Ok(Command::SendFile("/tmp/firmware.bin".into()))
        );
        assert!(
            parse("sendfile").is_err(),
            "sendfile without arg must error"
        );
        assert!(parse("sendfile  ").is_err());
        // Case-sensitive name like every other command.
        assert!(parse("SendFile x").is_err());
    }

    #[test]
    fn parser_is_case_sensitive_on_names_insensitive_on_values() {
        // Command names are lowercase-only (vi-consistent; documented).
        assert!(parse("Q").is_err());
        assert!(parse("BAUD 9600").is_err());
        assert!(parse("Ports").is_err());
        // Keyword values tolerate any case.
        assert_eq!(
            parse("hex ON"),
            Ok(Command::Toggle {
                flag: ToggleFlag::Hex,
                on: true
            })
        );
        assert_eq!(parse("txnl CR"), Ok(Command::Txnl(TxNewline::Cr)));
        assert_eq!(parse("parity EVEN"), Ok(Command::Parity(NcomParity::Even)));
    }

    #[test]
    fn parses_help() {
        assert_eq!(parse("help"), Ok(Command::Help));
        assert_eq!(parse("  help  "), Ok(Command::Help));
        assert!(parse("help foo").is_err(), "help takes no arguments");
        assert!(parse("Help").is_err(), "command names are case-sensitive");
    }

    #[test]
    fn exec_help_opens_popup() {
        let mut a = app();
        assert!(!a.help_visible);
        execute(Command::Help, &mut a, None);
        assert!(a.help_visible);
    }

    #[test]
    fn parser_tolerates_extra_whitespace_and_unknown_command() {
        assert_eq!(
            parse("  hex   on  "),
            Ok(Command::Toggle {
                flag: ToggleFlag::Hex,
                on: true
            })
        );
        assert_eq!(
            parse("connect   /dev/ttyUSB0    115200"),
            Ok(Command::Connect {
                dev: "/dev/ttyUSB0".into(),
                baud: Some(115_200)
            })
        );
        assert_eq!(
            parse("nope"),
            Err("not an editor command: nope".to_string())
        );
    }

    // ------------------------------------------------------------------
    // sendhex validation
    // ------------------------------------------------------------------

    #[test]
    fn sendhex_validates() {
        assert_eq!(parse_hex("deadBEEF"), Ok(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(parse_hex("de ad be ef"), Ok(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(parse_hex("  0a "), Ok(vec![0x0a]));
        assert_eq!(parse_hex(""), Ok(vec![]));
        assert!(parse_hex("abc").is_err(), "odd digit count must fail");
        assert!(parse_hex("zz").is_err(), "non-hex digits must fail");
        assert!(parse_hex("de-ad").is_err(), "punctuation must fail");
        assert!(
            parse_hex("é").is_err(),
            "non-ASCII must fail (no bytes sent)"
        );
        assert!(
            parse_hex("deé").is_err(),
            "non-ASCII must fail even at even byte length"
        );
        assert!(
            parse_hex("de ad é").is_err(),
            "non-ASCII among digits must fail"
        );
    }

    // ------------------------------------------------------------------
    // Execution
    // ------------------------------------------------------------------

    #[test]
    fn exec_toggles_and_txnl() {
        let mut a = app();
        execute(
            Command::Toggle {
                flag: ToggleFlag::Hex,
                on: true,
            },
            &mut a,
            None,
        );
        assert!(a.hex_on);
        execute(
            Command::Toggle {
                flag: ToggleFlag::Ts,
                on: true,
            },
            &mut a,
            None,
        );
        assert!(a.ts_on);
        execute(
            Command::Toggle {
                flag: ToggleFlag::Line,
                on: true,
            },
            &mut a,
            None,
        );
        assert_eq!(a.insert_style, InsertStyle::Line);
        execute(Command::Txnl(TxNewline::CrLf), &mut a, None);
        assert_eq!(a.txnl, TxNewline::CrLf);
    }

    #[test]
    fn exec_quit_with_log_guard() {
        let mut a = app();
        execute(Command::Quit { force: false }, &mut a, None);
        assert!(a.quit);
        let mut b = app();
        b.log_on = true;
        execute(Command::Quit { force: false }, &mut b, None);
        assert!(!b.quit, ":q must refuse while a log is active");
        assert!(b.last_error.clone().unwrap().contains(":q!"));
        execute(Command::Quit { force: true }, &mut b, None);
        assert!(b.quit);
    }

    #[test]
    fn exec_baud_no_arg_shows_current() {
        let (mut a, _rx, _tx) = connected_app();
        execute(Command::Baud(None), &mut a, None);
        assert_eq!(a.last_error, None);
        assert_eq!(a.status_text(), Some("baud: 115200"));

        let mut b = app();
        execute(Command::Baud(None), &mut b, None);
        assert!(b.last_error.clone().unwrap().contains("not connected"));
    }

    #[test]
    fn exec_config_updates_state_and_notifies_worker() {
        let (mut a, rx, tx) = connected_app();
        execute(parse("baud 9600").unwrap(), &mut a, Some(&tx));
        assert_eq!(a.port_cfg.as_ref().unwrap().baud, 9600);
        match rx.try_recv().unwrap() {
            ToWorker::Config { baud, .. } => assert_eq!(baud, 9600),
            other => panic!("unexpected message: {other:?}"),
        }

        execute(parse("databits 7").unwrap(), &mut a, Some(&tx));
        execute(parse("parity even").unwrap(), &mut a, Some(&tx));
        execute(parse("stopbits 2").unwrap(), &mut a, Some(&tx));
        execute(parse("flow xonz").unwrap(), &mut a, Some(&tx));
        let cfg = a.port_cfg.clone().unwrap();
        assert_eq!(
            cfg,
            SerialConfig {
                path: "/dev/ttyUSB0".into(),
                baud: 9600,
                data_bits: 7,
                parity: NcomParity::Even,
                stop_bits: NcomStopBits::Two,
                flow: NcomFlow::XonXoff,
            }
        );
        // Five Config messages total so far.
        let mut configs = 0;
        while rx.try_recv().is_ok() {
            configs += 1;
        }
        assert_eq!(configs, 4);
    }

    #[test]
    fn exec_unsupported_values_rejected_before_state_change() {
        let (mut a, rx, tx) = connected_app();
        execute(parse("parity mark").unwrap(), &mut a, Some(&tx));
        assert_eq!(a.port_cfg.as_ref().unwrap().parity, NcomParity::None);
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .contains("not supported by the serialport crate")
        );
        execute(parse("stopbits 1.5").unwrap(), &mut a, Some(&tx));
        assert_eq!(a.port_cfg.as_ref().unwrap().stop_bits, NcomStopBits::One);
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .contains("not supported by the serialport crate")
        );
        assert!(
            rx.try_recv().is_err(),
            "no worker message on rejected values"
        );
    }

    #[test]
    fn exec_config_without_connection_updates_locally() {
        let mut a = app();
        a.port_cfg = Some(default_serial_config());
        a.connected = false;
        let (tx, rx) = mpsc::channel();
        execute(parse("baud 4800").unwrap(), &mut a, Some(&tx));
        assert_eq!(a.port_cfg.as_ref().unwrap().baud, 4800);
        assert!(rx.try_recv().is_err(), "disconnected: no worker to notify");
    }

    #[test]
    fn exec_config_requires_known_port() {
        let mut a = app();
        let (tx, _rx) = mpsc::channel();
        execute(parse("baud 9600").unwrap(), &mut a, Some(&tx));
        assert_eq!(a.last_error.as_deref(), Some("baud: not connected"));
        assert_eq!(a.port_cfg, None);

        // The command name prefixes every reconfigure refusal (`parity`,
        // `stopbits`, ... likewise).
        let mut b = app();
        execute(parse("parity even").unwrap(), &mut b, Some(&tx));
        assert_eq!(b.last_error.as_deref(), Some("parity: not connected"));
    }

    #[test]
    fn exec_pins_require_connection_and_send_to_worker() {
        let mut a = app();
        let (tx, rx) = mpsc::channel();
        execute(Command::Dtr(true), &mut a, Some(&tx));
        assert!(a.last_error.clone().unwrap().contains("not connected"));
        assert!(rx.try_recv().is_err());

        let (mut b, rx, tx) = connected_app();
        execute(Command::Rts(false), &mut b, Some(&tx));
        match rx.try_recv().unwrap() {
            ToWorker::Rts(on) => assert!(!on),
            other => panic!("unexpected message: {other:?}"),
        }
        execute(Command::Dtr(true), &mut b, Some(&tx));
        assert!(matches!(rx.try_recv().unwrap(), ToWorker::Dtr(true)));
    }

    // ------------------------------------------------------------------
    // ITEM 5: failed worker-channel sends are truthful
    // ------------------------------------------------------------------

    #[test]
    fn exec_config_closed_channel_keeps_cfg_and_errors() {
        let (mut a, _rx, tx) = connected_app();
        drop(_rx); // worker gone; the sender still exists but sends fail
        execute(parse("baud 9600").unwrap(), &mut a, Some(&tx));
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .contains("baud: port not available"),
            "got: {:?}",
            a.last_error
        );
        assert_eq!(
            a.port_cfg.as_ref().unwrap().baud,
            115_200,
            "config must not change when the worker send fails"
        );
    }

    #[test]
    fn exec_pin_closed_channel_errors() {
        let (mut a, _rx, tx) = connected_app();
        drop(_rx);
        execute(Command::Dtr(true), &mut a, Some(&tx));
        assert_eq!(a.last_error.as_deref(), Some("dtr: port not available"));
        execute(Command::Rts(false), &mut a, Some(&tx));
        assert_eq!(a.last_error.as_deref(), Some("rts: port not available"));
    }

    // ------------------------------------------------------------------
    // ITEM 6: status freshness
    // ------------------------------------------------------------------

    #[test]
    fn exec_new_command_replaces_stale_status() {
        let mut a = app();
        execute(Command::Ports, &mut a, None);
        assert!(a.status_msg.is_some(), "ports sets a status message");
        // A new command starts fresh: the old status is gone even if the new
        // command itself sets none (plain toggle).
        execute(
            Command::Toggle {
                flag: ToggleFlag::Hex,
                on: true,
            },
            &mut a,
            None,
        );
        assert_eq!(a.status_msg, None);
        assert_eq!(a.last_error, None);
    }

    #[test]
    fn exec_failed_command_sets_error_and_no_status() {
        let mut a = app();
        // Preceding success leaves a status message...
        execute(Command::Ports, &mut a, None);
        assert!(a.status_msg.is_some());
        // ...the failing command clears it at start (freshness) and sets an
        // error, never a stale status.
        execute(
            Command::SendFile("/nonexistent-path/x".into()),
            &mut a,
            None,
        );
        assert!(a.status_msg.is_none());
        assert!(a.last_error.is_some());
        // And the command's own error is not clobbered by the success-clear.
        assert!(a.last_error.clone().unwrap().contains("sendfile"));
    }

    #[test]
    fn exec_same_failing_command_twice_shows_error_both_times() {
        let mut a = app(); // never connected: `send` hits the (false, _) arm
        let (tx, rx) = mpsc::channel();
        execute(parse("send x").unwrap(), &mut a, Some(&tx));
        assert_eq!(a.last_error.as_deref(), Some("send: not connected"));
        // The IDENTICAL failure repeated: the error must be visible again
        // (the old err_before equality check silently dropped the second one).
        execute(parse("send x").unwrap(), &mut a, Some(&tx));
        assert_eq!(
            a.last_error.as_deref(),
            Some("send: not connected"),
            "repeated identical failure must show its error again"
        );
        assert!(rx.try_recv().is_err(), "nothing sent while DOWN");
    }

    #[test]
    fn exec_send_appends_txnl_and_sendhex_raw() {
        let (mut a, rx, tx) = connected_app();
        execute(parse("send hello").unwrap(), &mut a, Some(&tx));
        match rx.try_recv().unwrap() {
            ToWorker::Tx(bytes) => assert_eq!(bytes, b"hello\r".to_vec()), // default txnl cr
            other => panic!("unexpected message: {other:?}"),
        }
        execute(parse("sendhex de ad ff").unwrap(), &mut a, Some(&tx));
        match rx.try_recv().unwrap() {
            ToWorker::Tx(bytes) => assert_eq!(bytes, vec![0xde, 0xad, 0xff]),
            other => panic!("unexpected message: {other:?}"),
        }

        let mut b = app();
        execute(parse("send x").unwrap(), &mut b, None);
        assert!(b.last_error.clone().unwrap().contains("not connected"));
    }

    /// Pump the app's pending sendfile until it completes; returns the status
    /// message that ended it.
    fn pump_to_completion(a: &mut App, tx: &mpsc::Sender<ToWorker>) -> String {
        for _ in 0..100_000 {
            a.pump_sendfile(tx);
            if let Some(msg) = a.status_text()
                && a.pending_sendfile.is_none()
            {
                return msg.to_string();
            }
        }
        panic!("sendfile pump never completed");
    }

    #[test]
    fn exec_sendfile_sets_up_chunked_pump_in_order() {
        let (mut a, rx, tx) = connected_app();
        let dir = std::env::temp_dir().join(format!("ncom-test-sendfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob.bin");
        // > 4 KiB so chunking is exercised: 3 bytes + 4096 filler + tail.
        let mut data = vec![0xde, 0xad, 0x0a];
        data.extend(std::iter::repeat_n(0x55u8, 4097));
        std::fs::write(&path, &data).unwrap();

        a.log_open(dir.join("session.log").to_string_lossy().as_ref())
            .unwrap();
        execute(
            Command::SendFile(path.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        // Nothing sent by the executor itself: the state is pending, the log
        // is still empty, the worker channel is still empty.
        assert!(
            a.pending_sendfile.is_some(),
            "executor installs pending state"
        );
        assert!(
            rx.try_recv().is_err(),
            "executor must not send synchronously"
        );

        // Driving the pump reassembles the exact file bytes, in order, across
        // several main-loop-iteration-sized chunks.
        let mut sent: Vec<Vec<u8>> = Vec::new();
        while let Ok(ToWorker::Tx(b)) = rx.try_recv() {
            sent.push(b);
        }
        let msg = pump_to_completion(&mut a, &tx);
        while let Ok(ToWorker::Tx(b)) = rx.try_recv() {
            sent.push(b);
        }
        assert_eq!(
            sent.concat(),
            data,
            "raw file bytes must arrive in order, no ending appended"
        );
        assert!(sent.len() >= 2, "4100 bytes must span multiple Tx chunks");
        // Status message names the byte count; no error.
        assert_eq!(msg, "sendfile: 4100 bytes");
        assert!(a.pending_sendfile.is_none());
        assert!(a.last_error.is_none());
        // The session log holds the same bytes in send order (send-time hook).
        assert_eq!(std::fs::read(dir.join("session.log")).unwrap(), data);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_sendfile_oversized_rejected_before_anything_is_sent() {
        let (mut a, rx, tx) = connected_app();
        let dir =
            std::env::temp_dir().join(format!("ncom-test-sendfile-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.bin");
        // Sparse file: metadata length is all the check reads.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(crate::app::SENDFILE_MAX + 1)
            .unwrap();

        a.log_open(dir.join("session.log").to_string_lossy().as_ref())
            .unwrap();
        execute(
            Command::SendFile(path.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .contains("too large (> 16 MiB)"),
            "got: {:?}",
            a.last_error
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing may be queued for an oversized file"
        );
        assert!(a.pending_sendfile.is_none());
        a.log_off();
        assert_eq!(
            std::fs::read(dir.join("session.log")).unwrap(),
            Vec::<u8>::new(),
            "no log bytes for an oversized file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_sendfile_while_pending_errors_and_does_not_interleave() {
        let (mut a, _rx, tx) = connected_app();
        let dir = std::env::temp_dir().join(format!("ncom-test-sendfile-2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("one.bin");
        let p2 = dir.join("two.bin");
        std::fs::write(&p1, vec![0xaa; 10_000]).unwrap();
        std::fs::write(&p2, vec![0xbb; 10_000]).unwrap();
        execute(
            Command::SendFile(p1.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        assert!(a.pending_sendfile.is_some());
        a.status_msg = None;
        execute(
            Command::SendFile(p2.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        assert_eq!(
            a.last_error.as_deref(),
            Some("sendfile: already in progress")
        );
        // Still the FIRST transfer pending, untouched.
        assert_eq!(
            a.pending_sendfile.as_ref().unwrap().path,
            p1.to_string_lossy()
        );
        pump_to_completion(&mut a, &tx);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pump_sendfile_aborts_when_port_goes_down() {
        let (mut a, _rx, tx) = connected_app();
        let dir = std::env::temp_dir().join(format!("ncom-test-sendfile-3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.bin");
        std::fs::write(&p, vec![0xcc; 10_000]).unwrap();
        execute(
            Command::SendFile(p.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        a.connected = false; // unplugged mid-transfer
        a.pump_sendfile(&tx);
        assert!(
            a.pending_sendfile.is_none(),
            "state must clear on disconnect"
        );
        assert!(a.last_error.clone().unwrap().contains("port went down"),);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connect_aborts_pending_sendfile_before_new_worker() {
        let (mut a, rx, tx) = connected_app();
        let dir = std::env::temp_dir().join(format!(
            "ncom-test-sendfile-reconnect-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("y.bin");
        std::fs::write(&p, vec![0xdd; 8192]).unwrap();
        execute(
            Command::SendFile(p.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        assert!(
            a.pending_sendfile.is_some(),
            "precondition: transfer pending"
        );

        // `:r` to a device that cannot be opened: either way the pending
        // transfer must be aborted, never carried onto the new connection.
        execute(
            Command::Connect {
                dev: "/dev/ncom-no-such-device".into(),
                baud: None,
            },
            &mut a,
            Some(&tx),
        );
        assert!(
            a.pending_sendfile.is_none(),
            "reconnect must abort a pending sendfile"
        );
        assert_eq!(
            a.status_text(),
            Some("sendfile: aborted (reconnect)")
        );
        // With the state cleared, pumping can send nothing to any worker.
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, 0, "no Tx may be queued across a reconnect abort");
        assert!(
            a.last_error.is_some(),
            "failed spawn still surfaces its own error"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_sendfile_missing_file_errors_and_sends_nothing() {
        let (mut a, rx, tx) = connected_app();
        let missing = std::env::temp_dir()
            .join(format!("ncom-sendfile-missing-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        execute(Command::SendFile(missing.clone()), &mut a, Some(&tx));
        assert!(
            rx.try_recv().is_err(),
            "nothing may be sent for a missing file"
        );
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .starts_with(&format!("sendfile {missing}:"))
        );
    }

    #[test]
    fn exec_sendfile_empty_file_sends_nothing() {
        let (mut a, rx, tx) = connected_app();
        let dir = std::env::temp_dir().join(format!("ncom-test-sendfile-e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        execute(
            Command::SendFile(path.to_string_lossy().into()),
            &mut a,
            Some(&tx),
        );
        assert!(rx.try_recv().is_err(), "empty file must send nothing");
        // Completes on the first pump with the 0-byte status.
        a.pump_sendfile(&tx);
        assert!(a.pending_sendfile.is_none());
        assert!(rx.try_recv().is_err());
        assert_eq!(a.status_text(), Some("sendfile: 0 bytes"));
        assert!(a.last_error.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_sendfile_down_port_errors_and_sends_nothing() {
        let mut a = app(); // never connected
        let (tx, rx) = mpsc::channel();
        execute(Command::SendFile("/tmp/x.bin".into()), &mut a, Some(&tx));
        assert!(rx.try_recv().is_err(), "no TX while DOWN");
        assert!(a.last_error.clone().unwrap().contains("not connected"));
        assert!(a.status_msg.is_none());
    }

    #[test]
    fn exec_log_opens_offs_and_fails_gracefully() {
        let mut a = app();
        let path = std::env::temp_dir()
            .join(format!("ncom-cmd-log-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        execute(parse("log off").unwrap(), &mut a, None);
        assert!(!a.log_on);
        assert_eq!(a.status_text(), Some("log: not active"));

        execute(parse(&format!("log {path}")).unwrap(), &mut a, None);
        assert!(a.log_on);
        assert!(a.logger.is_on());
        assert_eq!(
            a.status_text(),
            Some(format!("log: {path}").as_str())
        );

        // Bytes written while open land in the file.
        a.record_tx(b"hello\n");
        execute(parse("log off").unwrap(), &mut a, None);
        assert!(!a.log_on);
        assert_eq!(a.status_text(), Some("log: off"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\n".to_vec());

        // Open failure -> error, logging stays off.
        let bad = std::env::temp_dir()
            .join(format!("ncom-cmd-logblock-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        std::fs::write(&bad, b"x").unwrap();
        execute(parse(&format!("log {bad}/sub/log")).unwrap(), &mut a, None);
        assert!(!a.log_on);
        assert!(a.last_error.clone().unwrap().starts_with("log "));
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&bad).ok();
    }

    #[test]
    fn exec_write_dumps_scrollback_to_file() {
        let mut a = app();
        let t = std::time::SystemTime::now();
        a.push_line(b"alpha".to_vec(), t, crate::app::LineFlags::default());
        a.push_line(b"beta\x07!".to_vec(), t, crate::app::LineFlags::default());
        let dir = std::env::temp_dir().join(format!("ncom-test-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.txt");
        execute(
            Command::Write {
                path: path.to_string_lossy().into(),
                force: false,
            },
            &mut a,
            None,
        );
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "alpha\nbeta^G!\n");
        assert_eq!(
            a.status_text(),
            Some(format!("w: wrote 2 lines to {}", path.to_string_lossy()).as_str())
        );
        execute(
            Command::Write {
                path: "/nonexistent-root-dir/x".into(),
                force: false,
            },
            &mut a,
            None,
        );
        assert!(a.last_error.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_write_dumps_tx_echo_lines_raw_without_prefix() {
        // `:echo on` lines are stored with their RAW sent bytes (no `> `):
        // the prefix is render-only, so `:w` dumps stay byte-exact.
        let mut a = app();
        a.echo_on = true;
        let (tx_w, _rx_w) = mpsc::channel::<ToWorker>();
        a.send_tx(&tx_w, b"from ncom".to_vec());
        assert!(a.scrollback[0].flags.tx, "precondition: TX-flagged line");
        let dir = std::env::temp_dir().join(format!("ncom-test-w-echo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.txt");
        execute(
            Command::Write {
                path: path.to_string_lossy().into(),
                force: false,
            },
            &mut a,
            None,
        );
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            got, "from ncom\n",
            "raw bytes, no `> ` prefix in the dump"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_write_overwrite_guard() {
        let mut a = app();
        let t = std::time::SystemTime::now();
        a.push_line(b"line".to_vec(), t, crate::app::LineFlags::default());
        let dir = std::env::temp_dir().join(format!("ncom-test-wguard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.txt");
        let pstr = path.to_string_lossy().into_owned();

        // Existing non-empty file: plain `w` refuses, file untouched.
        std::fs::write(&path, b"original").unwrap();
        execute(
            Command::Write {
                path: pstr.clone(),
                force: false,
            },
            &mut a,
            None,
        );
        assert_eq!(
            a.last_error.as_deref(),
            Some(format!("w {pstr}: exists — use :w! to overwrite").as_str())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original".to_vec());
        a.last_error = None;

        // `w!` overwrites.
        execute(
            Command::Write {
                path: pstr.clone(),
                force: true,
            },
            &mut a,
            None,
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"line\n".to_vec());
        assert!(a.last_error.is_none());

        // Existing EMPTY file: plain `w` refuses too — the guard is atomic
        // create_new, which cannot see "empty" without the racy metadata()
        // pre-check; use `w!` to overwrite.
        std::fs::write(&path, b"").unwrap();
        execute(
            Command::Write {
                path: pstr.clone(),
                force: false,
            },
            &mut a,
            None,
        );
        assert_eq!(
            a.last_error.as_deref(),
            Some(format!("w {pstr}: exists — use :w! to overwrite").as_str())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"".to_vec());
        a.last_error = None;

        // New path: plain `w` proceeds.
        let fresh = dir.join("fresh.txt");
        execute(
            Command::Write {
                path: fresh.to_string_lossy().into(),
                force: false,
            },
            &mut a,
            None,
        );
        assert_eq!(std::fs::read(&fresh).unwrap(), b"line\n".to_vec());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------------
    // :hl / :filter — parse + execute
    // ------------------------------------------------------------------

    #[test]
    fn parses_hl_and_filter_all_forms() {
        assert_eq!(
            parse("hl error"),
            Ok(Command::Highlight(Some("error".into())))
        );
        assert_eq!(
            parse("hl multi word pat"),
            Ok(Command::Highlight(Some("multi word pat".into())))
        );
        assert_eq!(
            parse("hl"),
            Ok(Command::Highlight(None)),
            "no arg = query form"
        );
        assert_eq!(
            parse("hl   "),
            Ok(Command::Highlight(None)),
            "whitespace-only arg trims to the query form"
        );
        assert_eq!(parse("hl off"), Ok(Command::Highlight(Some("off".into()))));
        // Only exact lowercase `off` clears (executor); `OFF` is a pattern.
        assert_eq!(parse("hl OFF"), Ok(Command::Highlight(Some("OFF".into()))));

        assert_eq!(parse("filter err"), Ok(Command::Filter(Some("err".into()))));
        assert_eq!(parse("filter"), Ok(Command::Filter(None)));
        assert_eq!(parse("filter off"), Ok(Command::Filter(Some("off".into()))));
        // Case-sensitive command names like every other command.
        assert!(parse("HL x").is_err());
        assert!(parse("Filter x").is_err());
    }

    #[test]
    fn exec_hl_set_show_and_off() {
        let mut a = app();
        execute(parse("hl").unwrap(), &mut a, None);
        assert_eq!(a.status_text(), Some("hl: off"));
        execute(parse("hl error").unwrap(), &mut a, None);
        assert_eq!(a.hl_pattern.as_deref(), Some("error"));
        assert_eq!(a.status_text(), Some("hl: error"));
        execute(parse("hl").unwrap(), &mut a, None);
        assert_eq!(a.status_text(), Some("hl: error"));
        execute(parse("hl off").unwrap(), &mut a, None);
        assert_eq!(a.hl_pattern, None);
        assert_eq!(a.status_text(), Some("hl: off"));
        // `OFF` (any non-lowercase-off) is a pattern, not the clear toggle.
        execute(parse("hl OFF").unwrap(), &mut a, None);
        assert_eq!(a.hl_pattern.as_deref(), Some("OFF"));
        // Empty pattern rejected (directly-constructed command guard).
        execute(Command::Highlight(Some(String::new())), &mut a, None);
        assert!(a.last_error.clone().unwrap().contains("empty pattern"));
    }

    #[test]
    fn parses_echo_all_forms() {
        assert_eq!(parse("echo on"), Ok(Command::Echo(Some("on".into()))));
        assert_eq!(parse("echo off"), Ok(Command::Echo(Some("off".into()))));
        assert_eq!(parse("echo"), Ok(Command::Echo(None)), "no arg = query form");
        assert_eq!(
            parse("  echo   "),
            Ok(Command::Echo(None)),
            "whitespace-only arg trims to the query form"
        );
        // Case-sensitive keywords like every command value that is not routed
        // through `toggle`: `OFF`/`ON` are invalid values (executor errors).
        assert_eq!(parse("echo OFF"), Ok(Command::Echo(Some("OFF".into()))));
        assert_eq!(parse("echo ON"), Ok(Command::Echo(Some("ON".into()))));
        assert_eq!(
            parse("echo maybe"),
            Ok(Command::Echo(Some("maybe".into())))
        );
        assert!(parse("Echo on").is_err(), "command names are case-sensitive");
    }

    #[test]
    fn exec_echo_set_show_off_and_bad_value() {
        let mut a = app();
        // Query form on the default (off).
        execute(parse("echo").unwrap(), &mut a, None);
        assert!(!a.echo_on);
        assert_eq!(a.status_text(), Some("echo: off"));
        // On.
        execute(parse("echo on").unwrap(), &mut a, None);
        assert!(a.echo_on);
        assert_eq!(a.status_text(), Some("echo: on"));
        // Query form reflects the new state.
        execute(parse("echo").unwrap(), &mut a, None);
        assert_eq!(a.status_text(), Some("echo: on"));
        // Off again.
        execute(parse("echo off").unwrap(), &mut a, None);
        assert!(!a.echo_on);
        assert_eq!(a.status_text(), Some("echo: off"));
        // Only exact lowercase `off`/`on` are valid; anything else errors and
        // leaves the state untouched.
        execute(parse("echo OFF").unwrap(), &mut a, None);
        assert!(!a.echo_on);
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .contains("echo: invalid value 'OFF'"),
            "got: {:?}",
            a.last_error
        );
        execute(parse("echo maybe").unwrap(), &mut a, None);
        assert!(!a.echo_on);
        assert!(a.last_error.clone().unwrap().contains("invalid value"));
    }

    #[test]
    fn exec_filter_set_snaps_to_filtered_tail_off_restores() {
        let mut a = app();
        let t = std::time::SystemTime::now();
        for text in ["match one", "skip", "match two"] {
            a.push_line(
                text.as_bytes().to_vec(),
                t,
                crate::app::LineFlags::default(),
            );
        }
        // Pretend the user was scrolled back in the unfiltered view.
        a.follow_tail = false;
        a.view_offset = 2;
        execute(parse("filter").unwrap(), &mut a, None);
        assert_eq!(a.status_text(), Some("filter: off"));
        execute(parse("filter match").unwrap(), &mut a, None);
        assert_eq!(a.filter_pattern.as_deref(), Some("match"));
        assert_eq!(a.status_text(), Some("filter: match (2 lines)"));
        // Entering a filter snaps to the filtered tail (old tail-relative
        // offset belonged to the unfiltered list).
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        // Query form shows the current pattern.
        execute(parse("filter").unwrap(), &mut a, None);
        assert_eq!(a.status_text(), Some("filter: match"));
        // `filter off` restores the full view at the tail.
        execute(parse("filter off").unwrap(), &mut a, None);
        assert_eq!(a.filter_pattern, None);
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        assert_eq!(a.status_text(), Some("filter: off"));
        assert_eq!(a.view_len(), 3);
    }

    #[test]
    fn exec_filter_empty_pattern_errors() {
        let mut a = app();
        execute(Command::Filter(Some(String::new())), &mut a, None);
        assert!(a.last_error.clone().unwrap().contains("empty pattern"));
        assert_eq!(a.filter_pattern, None);
    }

    // ------------------------------------------------------------------
    // Filter transitions clear `last_match` (wrong coordinate system:
    // absolute index without a filter, filtered-view position with one).
    // `last_search` is kept so `n` re-runs it in the new view coordinates.
    // ------------------------------------------------------------------

    fn three_line_app() -> App {
        let mut a = app();
        let t = std::time::SystemTime::now();
        for text in ["err one", "ok line", "err two"] {
            a.push_line(
                text.as_bytes().to_vec(),
                t,
                crate::app::LineFlags::default(),
            );
        }
        a
    }

    #[test]
    fn exec_filter_transition_clears_last_match_and_n_reruns_in_view_coords() {
        let mut a = three_line_app();
        a.search_prompt = Some("err".into());
        a.exec_search(); // unfiltered: absolute match 0
        assert_eq!(a.last_match, Some(0));

        // Filter ON: the absolute match is meaningless in filtered-view
        // coordinates — the transition drops it and keeps `last_search`.
        execute(parse("filter err").unwrap(), &mut a, None);
        assert_eq!(a.filter_pattern.as_deref(), Some("err"));
        assert_eq!(a.last_match, None, "filter transition clears last_match");
        assert_eq!(a.last_search.as_deref(), Some("err"), "last_search kept");
        // `n` re-runs the search IN the filtered view: without a surviving
        // (wrong-coordinate) match it lands on the FIRST filtered match
        // (position 0 = "err one"), not a wrong jump to position 1.
        a.search_step(true);
        assert_eq!(
            a.last_match,
            Some(0),
            "filtered-view position of \"err one\""
        );

        // Step forward inside the filtered view, then filter OFF: the
        // view-position match must not leak into absolute coordinates.
        a.search_step(true);
        assert_eq!(a.last_match, Some(1), "\"err two\" = filtered position 1");
        execute(parse("filter off").unwrap(), &mut a, None);
        assert_eq!(a.filter_pattern, None);
        assert_eq!(
            a.last_match, None,
            "filter off clears the view-position match"
        );
        assert_eq!(a.last_search.as_deref(), Some("err"));
        // `n` re-runs the search in the restored view: absolute indices again.
        a.search_step(true);
        assert_eq!(a.last_match, Some(0), "absolute coordinate again");
    }

    #[test]
    fn exec_filter_unchanged_pattern_keeps_last_match() {
        let mut a = three_line_app();
        execute(parse("filter err").unwrap(), &mut a, None);
        a.search_prompt = Some("two".into());
        a.exec_search(); // "err two" = filtered position 1
        assert_eq!(a.last_match, Some(1));
        // Re-issuing the SAME pattern is not a transition: nothing clears.
        execute(parse("filter err").unwrap(), &mut a, None);
        assert_eq!(a.last_match, Some(1), "same value: no transition, no clear");
        // The query form never changes state either.
        execute(parse("filter").unwrap(), &mut a, None);
        assert_eq!(a.last_match, Some(1));
        // A CHANGED pattern value is a transition: clears.
        execute(parse("filter two").unwrap(), &mut a, None);
        assert_eq!(a.last_match, None, "pattern value change clears last_match");
    }

    // ------------------------------------------------------------------
    // :noh — clear only the / search state
    // ------------------------------------------------------------------

    #[test]
    fn parses_noh_no_args() {
        assert_eq!(parse("noh"), Ok(Command::Noh));
        assert_eq!(parse("  noh  "), Ok(Command::Noh));
        assert!(parse("noh extra").is_err(), "noh takes no arguments");
        assert!(parse("Noh").is_err(), "command names are case-sensitive");
    }

    #[test]
    fn exec_noh_clears_only_search_state() {
        let mut a = app();
        let t = std::time::SystemTime::now();
        for text in ["alpha hit", "beta", "gamma hit"] {
            a.push_line(
                text.as_bytes().to_vec(),
                t,
                crate::app::LineFlags::default(),
            );
        }
        execute(parse("hl HL").unwrap(), &mut a, None);
        execute(parse("filter a").unwrap(), &mut a, None);
        a.follow_tail = false;
        a.view_offset = 2;
        // `/` search state as `search_start` would leave it (search is a
        // NORMAL-mode key, not a `:` command).
        a.last_search = Some("hit".into());
        a.last_match = Some(0);
        assert!(a.last_search.is_some());
        assert!(a.last_match.is_some());

        execute(parse("noh").unwrap(), &mut a, None);
        assert_eq!(a.last_search, None, "noh clears the / search pattern");
        assert_eq!(a.last_match, None, "noh clears the / search match");
        assert_eq!(
            a.status_text(),
            Some("noh: search cleared"),
            "informational status, no error"
        );
        assert_eq!(a.hl_pattern.as_deref(), Some("HL"), "hl pattern untouched");
        assert_eq!(
            a.filter_pattern.as_deref(),
            Some("a"),
            "filter pattern untouched"
        );
        assert_eq!(a.view_offset, 2, "view position untouched");
        assert!(!a.follow_tail, "follow-tail untouched");
        assert!(a.last_error.is_none());
    }

    #[test]
    fn exec_write_dumps_full_scrollback_with_filter_active() {
        let mut a = app();
        let t = std::time::SystemTime::now();
        for text in ["alpha keep", "beta drop", "gamma keep"] {
            a.push_line(
                text.as_bytes().to_vec(),
                t,
                crate::app::LineFlags::default(),
            );
        }
        execute(parse("filter alpha").unwrap(), &mut a, None);
        let dir = std::env::temp_dir().join(format!("ncom-test-w-filter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.txt");
        execute(
            Command::Write {
                path: path.to_string_lossy().into(),
                force: false,
            },
            &mut a,
            None,
        );
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            got, "alpha keep\nbeta drop\ngamma keep\n",
            ":w dumps the FULL scrollback, not the filtered view"
        );
        assert_eq!(
            a.status_text()
                .map(|s| s.starts_with("w: wrote 3 lines")),
            Some(true)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------------
    // Ports scanner (fake roots injected)
    // ------------------------------------------------------------------

    #[test]
    fn scan_ports_fake_roots_and_filter() {
        let base = std::env::temp_dir().join(format!("ncom-test-ports-{}", std::process::id()));
        let by_id = base.join("by-id");
        let real_dev = base.join("dev");
        let sysfs = base.join("class/tty");
        std::fs::create_dir_all(&by_id).unwrap();
        std::fs::create_dir_all(&real_dev).unwrap();
        // sysfs style: entry with `device` link is real; without it is not.
        std::fs::create_dir_all(sysfs.join("ttyV0/device")).unwrap();
        std::fs::create_dir_all(sysfs.join("ttyPCM")).unwrap(); // no device dir
        // by-id style: symlink entry -> a real file (stands in for /dev node).
        let node = real_dev.join("ttyFAKE0");
        std::fs::write(&node, b"").unwrap();
        std::os::unix::fs::symlink(&node, by_id.join("usb-foo")).unwrap();
        // udev's real-world shape: a symlink with a RELATIVE target
        // (`../../ttyX` style) — canonicalizing the read_link TARGET would
        // resolve it against the CWD and miss; canonicalizing the link itself
        // must find it.
        let rel_node = real_dev.join("ttyREL");
        std::fs::write(&rel_node, b"").unwrap();
        std::os::unix::fs::symlink("../dev/ttyREL", by_id.join("usb-rel")).unwrap();
        // A dangling symlink (device vanished) is skipped.
        std::os::unix::fs::symlink(real_dev.join("gone"), by_id.join("dangling")).unwrap();

        let ports = scan_ports(std::slice::from_ref(&by_id), &sysfs);
        assert_eq!(ports, vec!["/dev/ttyFAKE0", "/dev/ttyREL", "/dev/ttyV0"]);

        // Missing roots are skipped, not fatal.
        assert!(scan_ports(&[base.join("nope")], &base.join("nope2")).is_empty());
        // Filter predicate directly.
        assert!(is_real_tty(&sysfs.join("ttyV0")));
        assert!(!is_real_tty(&sysfs.join("ttyPCM")));
        std::fs::remove_dir_all(&base).ok();
    }
}
