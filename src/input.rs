//! Key handling per mode (NORMAL / INSERT / COMMAND) plus the `/` search
//! prompt.
//!
//! Modifier hygiene: the only CONTROL keys with meaning are the global
//! `Ctrl-C` quit (in `handle_key`) and the NORMAL page motions
//! `Ctrl-d`/`Ctrl-u`/`Ctrl-f`/`Ctrl-b` (plus `Ctrl-u`/`Ctrl-w` in line-mode
//! INSERT). Every other `Char` carrying CONTROL or ALT is deliberately inert
//! in every mode: crossterm delivers e.g. `Ctrl-N` as `Char('n')` + CONTROL,
//! and without the gates it would act as a plain keypress.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::mpsc;

use crate::app::{App, InsertStyle, Mode, PendingKey};
use crate::command;
use crate::serial::ToWorker;

/// Outcome of handling a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyResult {
    Handled,
    Quit,
}

/// Route one key press: the `/` prompt captures input ahead of mode handling;
/// otherwise dispatch to the current mode's handler. `tx` is `None` until a
/// port is opened (motions/search still work disconnected).
pub fn handle_key(app: &mut App, key: KeyEvent, tx: Option<&mpsc::Sender<ToWorker>>) -> KeyResult {
    // Release is unreachable in production (main.rs filters events to Press)
    // — the guard is defense-in-depth for direct callers/tests.
    if key.kind == crossterm::event::KeyEventKind::Release {
        return KeyResult::Handled;
    }
    // Global safety exit — the app quits via :q / ZZ; Ctrl-C is the standard
    // panic hatch so a broken mode state can never wedge the TUI.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return KeyResult::Quit;
    }
    if app.search_prompt.is_some() {
        return handle_search_prompt(app, key);
    }
    // The `:help` popup is a MODE-INDEPENDENT modal: while it is open every
    // key is captured before mode dispatch — it never reaches mode handling,
    // so it can never be sent as TX or change modes. The popup has two pages
    // (`App::help_page`), navigated vim-style: `j`, Ctrl-F, `G` page forward,
    // `k`, Ctrl-B, `g` (as in gg) page back, clamped at both ends (no wrap).
    // Every OTHER key closes the popup (deliberately including Esc and `q`,
    // the standard dismissal keys, which also match vim's quit reflex).
    if app.help_visible {
        // Bitflags constants cannot be used as match patterns, so the
        // modifier check is a guard (`contains`) instead.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let next = match key.code {
            KeyCode::Char('j') if !ctrl => Some(app.help_page + 1),
            KeyCode::Char('G') if !ctrl => Some(app.help_page + 1),
            KeyCode::Char('f') if ctrl => Some(app.help_page + 1),
            KeyCode::Char('k') if !ctrl => Some(app.help_page.saturating_sub(1)),
            KeyCode::Char('g') if !ctrl => Some(app.help_page.saturating_sub(1)),
            KeyCode::Char('b') if ctrl => Some(app.help_page.saturating_sub(1)),
            _ => None,
        };
        match next {
            // Page keys clamp at both ends (no wrap) and keep the popup open.
            Some(p) if p < crate::app::HELP_PAGE_COUNT => app.help_page = p,
            Some(_) => {}
            None => {
                app.help_visible = false;
                app.help_page = 0;
            }
        }
        return KeyResult::Handled;
    }
    match app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::Insert => {
            handle_insert(app, key, tx);
            KeyResult::Handled
        }
        Mode::Command => {
            handle_command(app, key, tx);
            KeyResult::Handled
        }
    }
}

/// `/` search prompt: typing builds the pattern, Enter executes, Esc cancels.
fn handle_search_prompt(app: &mut App, key: KeyEvent) -> KeyResult {
    match key.code {
        KeyCode::Esc => app.search_prompt = None,
        KeyCode::Enter => app.exec_search(),
        KeyCode::Backspace => {
            app.search_prompt.as_mut().map(String::pop);
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(p) = app.search_prompt.as_mut() {
                p.push(c);
            }
        }
        // Char with CONTROL/ALT: deliberately ignored (same gate as
        // handle_insert_raw) so e.g. Ctrl-A cannot type an `a` into the
        // pattern.
        KeyCode::Char(_) => {}
        _ => {}
    }
    KeyResult::Handled
}

/// NORMAL mode: mode changes, motions, search nav, follow toggle, `ZZ` quit.
fn handle_normal(app: &mut App, key: KeyEvent) -> KeyResult {
    // Two-key sequences first: any other key cancels the pending state.
    if let Some(pending) = app.pending {
        app.pending = None;
        return match (pending, key.code) {
            (PendingKey::G, KeyCode::Char('g')) => {
                app.goto_top();
                KeyResult::Handled
            }
            // ZZ follows the same rules as `:q` ("quit (same rules as
            // :q)"): refused with an error while a log is active; `:q!` (or
            // Ctrl-C) is the override.
            (PendingKey::Z, KeyCode::Char('Z')) => {
                if app.log_on {
                    app.set_error("log active — use :q! to quit");
                    KeyResult::Handled
                } else {
                    KeyResult::Quit
                }
            }
            _ => KeyResult::Handled,
        };
    }
    let half = (app.viewport_rows / 2).max(1);
    let full = app.viewport_rows.max(1);
    match key.code {
        // Deliberate CONTROL keys first (page motions): they must precede the
        // CONTROL/ALT gate below, which would otherwise swallow them.
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.motion_down(half)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.motion_up(half),
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.motion_down(full)
        }
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => app.motion_up(full),
        // Gate: any OTHER Char carrying CONTROL or ALT is inert. crossterm
        // reports e.g. Ctrl-N as Char('n') + CONTROL; without this arm it
        // would act as a plain `n` (motion/search/...). Ctrl-C never gets here
        // (global quit in handle_key); see the module docs for the full list
        // of intended ctrl keys.
        KeyCode::Char(_)
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
        KeyCode::Char('i') => {
            app.mode = Mode::Insert;
        }
        KeyCode::Char(':') => {
            app.command_line.clear();
            app.mode = Mode::Command;
        }
        KeyCode::Char('j') => app.motion_down(1),
        KeyCode::Char('k') => app.motion_up(1),
        KeyCode::Char('g') => app.pending = Some(PendingKey::G),
        KeyCode::Char('Z') => app.pending = Some(PendingKey::Z),
        KeyCode::Char('G') => app.goto_end(),
        KeyCode::Char('F' | 'p') => app.toggle_follow(),
        KeyCode::Char('/') => app.search_prompt = Some(String::new()),
        // `?` (NORMAL) opens the same help popup as `:help` (trivial alias:
        // one arm setting the same flag, mirroring vim's `:help` / `?`
        // coexistence for discoverability).
        KeyCode::Char('?') => {
            app.help_visible = true;
            app.help_page = 0;
        }
        KeyCode::Char('n') => app.search_step(true),
        KeyCode::Char('N') => app.search_step(false),
        _ => {}
    }
    KeyResult::Handled
}

/// INSERT mode dispatch: raw passthrough (`:line off`) or local input line
/// (`:line on`).
fn handle_insert(app: &mut App, key: KeyEvent, tx: Option<&mpsc::Sender<ToWorker>>) {
    if app.insert_style == InsertStyle::Line {
        handle_insert_line(app, key, tx);
    } else {
        handle_insert_raw(app, key, tx);
    }
}

/// INSERT raw passthrough (`:line off`, default). Every keypress is sent
/// immediately: Enter per `:txnl`, Backspace = DEL 0x7f, Tab = 0x09, printable
/// chars as UTF-8. Esc returns to NORMAL.
fn handle_insert_raw(app: &mut App, key: KeyEvent, tx: Option<&mpsc::Sender<ToWorker>>) {
    let bytes: Option<Vec<u8>> = match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            None
        }
        KeyCode::Enter => Some(app.txnl.ending().to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![0x09]),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c.to_string().into_bytes())
        }
        _ => None,
    };
    if let Some(bytes) = bytes {
        // Port DOWN (or never connected): TX must not silently vanish — every
        // keypress surfaces the error instead (`:r` reconnects).
        if !app.connected {
            app.set_error("port is DOWN — :r to reconnect");
        } else if let Some(tx) = tx {
            app.send_tx(tx, bytes);
        }
    }
}

/// INSERT line mode (`:line on`): keys edit the local input bar (rendered
/// above the status line by ui.rs). Printable chars insert at the cursor;
/// Backspace deletes; `Ctrl-u` clears; `Ctrl-w` deletes a word back;
/// Left/Right move the cursor; Up/Down walk the in-memory history (newest
/// 100). Enter submits: appends to history and sends line + `:txnl` ending.
/// `Esc` → NORMAL *keeping* the pending text; `i` in NORMAL re-enters INSERT
/// and editing resumes (`line_input`/`line_cursor` are never cleared on Esc).
///
/// crossterm 0.29 `KeyCode::Left/Right/Up/Down`
/// (docs.rs/crossterm/0.29.0/crossterm/event/enum.KeyCode.html).
fn handle_insert_line(app: &mut App, key: KeyEvent, tx: Option<&mpsc::Sender<ToWorker>>) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => app.mode = Mode::Normal,
        (KeyCode::Enter, _) => app.line_submit(tx),
        (KeyCode::Backspace, m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            app.line_backspace()
        }
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => app.line_clear(),
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => app.line_delete_word_back(),
        (KeyCode::Left, _) => app.line_left(),
        (KeyCode::Right, _) => app.line_right(),
        (KeyCode::Up, _) => app.line_history_prev(),
        (KeyCode::Down, _) => app.line_history_next(),
        (KeyCode::Tab, m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            app.line_insert_char('\t')
        }
        (KeyCode::Char(c), m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            app.line_insert_char(c)
        }
        _ => {}
    }
}

/// COMMAND mode: capture typed chars into the command line; Enter executes,
/// Esc aborts back to NORMAL.
fn handle_command(app: &mut App, key: KeyEvent, tx: Option<&mpsc::Sender<ToWorker>>) {
    match key.code {
        KeyCode::Esc => {
            app.command_line.clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut app.command_line);
            execute_command(app, &line, tx);
        }
        KeyCode::Backspace => {
            app.command_line.pop();
        }
        // Char with CONTROL/ALT is ignored (same gate as handle_insert_raw /
        // handle_search_prompt) so e.g. Ctrl-U cannot type a `u` into the
        // command line.
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.command_line.push(c)
        }
        _ => {}
    }
}

/// `:` command execution — parse via `command::parse`, execute via
/// `command::execute` (connect/reconnect spawn the worker there). After Enter
/// the app always returns to NORMAL (vi-consistent, see command.rs module
/// docs); parse errors land in the status line. An empty (or all-whitespace)
/// line is silently ignored — vim behaviour: `:` + Enter just closes the line,
/// it is not a parse error.
fn execute_command(app: &mut App, line: &str, tx: Option<&mpsc::Sender<ToWorker>>) {
    app.mode = Mode::Normal;
    if line.trim().is_empty() {
        return;
    }
    match command::parse(line) {
        Ok(cmd) => command::execute(cmd, app, tx),
        Err(e) => app.set_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, InsertStyle, LineFlags, Mode};
    use crate::config::Config;
    use crate::serial::{FromWorker, ToWorker, TxNewline};
    use std::sync::mpsc;
    use std::time::SystemTime;

    fn app() -> App {
        App::new(&Config::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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
    fn normal_i_enters_insert_colon_enters_command() {
        let mut a = app();
        handle_key(&mut a, key(KeyCode::Char('i')), None);
        assert_eq!(a.mode, Mode::Insert);
        handle_key(&mut a, key(KeyCode::Esc), None);
        assert_eq!(a.mode, Mode::Normal);
        handle_key(&mut a, key(KeyCode::Char(':')), None);
        assert_eq!(a.mode, Mode::Command);
        assert!(a.command_line.is_empty());
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let mut a = app();
        a.mode = Mode::Insert;
        assert_eq!(
            handle_key(&mut a, ctrl(KeyCode::Char('c')), None),
            KeyResult::Quit
        );
    }

    #[test]
    fn insert_raw_passthrough_encodings() {
        let mut a = app();
        a.connected = true; // TX only flows while the port is UP
        a.mode = Mode::Insert;
        let (tx, rx) = mpsc::channel::<ToWorker>();

        handle_key(&mut a, key(KeyCode::Char('h')), Some(&tx));
        handle_key(&mut a, key(KeyCode::Char('i')), Some(&tx));
        handle_key(&mut a, key(KeyCode::Tab), Some(&tx));
        handle_key(&mut a, key(KeyCode::Backspace), Some(&tx));
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx)); // default :txnl cr

        let sent: Vec<Vec<u8>> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|m| match m {
                ToWorker::Tx(b) => b,
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(sent[0], b"h".to_vec());
        assert_eq!(sent[1], b"i".to_vec());
        assert_eq!(sent[2], vec![0x09]);
        assert_eq!(sent[3], vec![0x7f]);
        assert_eq!(sent[4], b"\r".to_vec());
        assert_eq!(a.mode, Mode::Insert);
    }

    #[test]
    fn insert_esc_returns_to_normal() {
        let mut a = app();
        a.mode = Mode::Insert;
        handle_key(&mut a, key(KeyCode::Esc), None);
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn insert_txnl_variants() {
        let mut a = app();
        a.connected = true;
        a.mode = Mode::Insert;
        a.txnl = TxNewline::Lf;
        let (tx, rx) = mpsc::channel::<ToWorker>();
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        match rx.try_recv().unwrap() {
            ToWorker::Tx(b) => assert_eq!(b, b"\n".to_vec()),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn insert_ignores_ctrl_combinations_except_quit() {
        let mut a = app();
        a.mode = Mode::Insert;
        let (tx, rx) = mpsc::channel::<ToWorker>();
        handle_key(&mut a, ctrl(KeyCode::Char('d')), Some(&tx));
        assert!(rx.try_recv().is_err(), "ctrl combos must not be sent raw");
        assert_eq!(a.mode, Mode::Insert);
    }

    #[test]
    fn command_captures_esc_aborts_enter_executes() {
        let mut a = app();
        a.mode = Mode::Command;
        let (tx, _rx) = mpsc::channel();
        for c in "q!".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        assert_eq!(a.command_line, "q!");
        handle_key(&mut a, key(KeyCode::Backspace), Some(&tx));
        assert_eq!(a.command_line, "q");
        handle_key(&mut a, key(KeyCode::Esc), Some(&tx));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.command_line.is_empty());

        // Enter executes and returns to NORMAL; unknown commands leave an
        // error on the status line.
        a.mode = Mode::Command;
        for c in "bogus".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert_eq!(a.mode, Mode::Normal);
        assert!(
            a.last_error
                .clone()
                .unwrap()
                .contains("not an editor command: bogus")
        );

        // A valid command runs (hex on) with no error left behind.
        a.last_error = None;
        a.mode = Mode::Command;
        for c in "hex on".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.hex_on);
        assert!(a.last_error.is_none());
    }

    #[test]
    fn insert_style_default_is_raw() {
        let a = app();
        assert_eq!(a.insert_style, InsertStyle::Raw);
    }

    // ------------------------------------------------------------------
    // INSERT line mode (`:line on`)
    // ------------------------------------------------------------------

    fn line_app() -> App {
        let mut a = app();
        a.insert_style = InsertStyle::Line;
        a.mode = Mode::Insert;
        a.connected = true;
        a
    }

    fn drain_tx(rx: &mpsc::Receiver<ToWorker>) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|m| match m {
                ToWorker::Tx(b) => b,
                other => panic!("unexpected message: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn line_editing_insert_backspace_cursor() {
        let mut a = line_app();
        let (tx, _rx) = mpsc::channel::<ToWorker>();
        for c in "hello".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        assert_eq!(a.line_input, "hello");
        assert_eq!(a.line_cursor, 5);
        // Left twice, insert in the middle.
        handle_key(&mut a, key(KeyCode::Left), Some(&tx));
        handle_key(&mut a, key(KeyCode::Left), Some(&tx));
        handle_key(&mut a, key(KeyCode::Char('X')), Some(&tx));
        assert_eq!(a.line_input, "helXlo");
        assert_eq!(a.line_cursor, 4);
        // Backspace deletes before the cursor.
        handle_key(&mut a, key(KeyCode::Backspace), Some(&tx));
        assert_eq!(a.line_input, "hello");
        assert_eq!(a.line_cursor, 3);
        // Cursor bounds: Left at 0 and Right at end are no-ops.
        for _ in 0..10 {
            handle_key(&mut a, key(KeyCode::Left), Some(&tx));
        }
        assert_eq!(a.line_cursor, 0);
        handle_key(&mut a, key(KeyCode::Backspace), Some(&tx));
        assert_eq!(a.line_input, "hello"); // no-op at start
        for _ in 0..10 {
            handle_key(&mut a, key(KeyCode::Right), Some(&tx));
        }
        assert_eq!(a.line_cursor, 5);
    }

    #[test]
    fn line_ctrl_u_clears_and_ctrl_w_deletes_word() {
        let mut a = line_app();
        let (tx, _rx) = mpsc::channel::<ToWorker>();
        for c in "foo bar  baz".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, ctrl(KeyCode::Char('w')), Some(&tx));
        assert_eq!(a.line_input, "foo bar  ");
        assert_eq!(a.line_cursor, 9);
        handle_key(&mut a, ctrl(KeyCode::Char('w')), Some(&tx)); // eats the gap too
        assert_eq!(a.line_input, "foo ");
        // Ctrl-u clears everything, cursor home.
        for c in "more".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, ctrl(KeyCode::Char('u')), Some(&tx));
        assert_eq!(a.line_input, "");
        assert_eq!(a.line_cursor, 0);
    }

    #[test]
    fn line_history_navigation_and_bounds() {
        let mut a = line_app();
        let (tx, rx) = mpsc::channel::<ToWorker>();
        for (text, end) in [("one", true), ("two", true), ("three", false)] {
            for c in text.chars() {
                handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
            }
            if end {
                handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
            }
        }
        // "three" is still pending (not submitted).
        assert_eq!(a.line_input, "three");
        assert_eq!(a.line_history, ["one", "two"]);
        // Up walks backward through history; loading moves cursor to end.
        handle_key(&mut a, key(KeyCode::Up), Some(&tx));
        assert_eq!(a.line_input, "two");
        assert_eq!(a.line_cursor, 3);
        handle_key(&mut a, key(KeyCode::Up), Some(&tx));
        assert_eq!(a.line_input, "one");
        handle_key(&mut a, key(KeyCode::Up), Some(&tx));
        assert_eq!(a.line_input, "one"); // bounded at oldest
        handle_key(&mut a, key(KeyCode::Down), Some(&tx));
        assert_eq!(a.line_input, "two");
        handle_key(&mut a, key(KeyCode::Down), Some(&tx));
        assert_eq!(a.line_input, "three"); // draft restored
        handle_key(&mut a, key(KeyCode::Down), Some(&tx));
        assert_eq!(a.line_input, "three"); // bounded at draft
        // Up then Enter sends the history entry text.
        handle_key(&mut a, key(KeyCode::Up), Some(&tx));
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        let sent = drain_tx(&rx);
        assert_eq!(
            sent,
            vec![b"one\r".to_vec(), b"two\r".to_vec(), b"two\r".to_vec()]
        );
        assert_eq!(a.line_input, "");
        assert_eq!(a.line_hist_pos, None);
    }

    #[test]
    fn line_history_capped_at_100() {
        let mut a = line_app();
        let (tx, _rx) = mpsc::channel::<ToWorker>();
        for i in 0..120 {
            for c in format!("line{i}").chars() {
                handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
            }
            handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        }
        assert_eq!(a.line_history.len(), crate::app::LINE_HISTORY_CAP);
        assert_eq!(a.line_history.front().map(String::as_str), Some("line20"));
        assert_eq!(a.line_history.back().map(String::as_str), Some("line119"));
    }

    #[test]
    fn line_pending_text_survives_esc_then_i() {
        let mut a = line_app();
        let (tx, _rx) = mpsc::channel::<ToWorker>();
        for c in "keep me".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, key(KeyCode::Left), Some(&tx));
        handle_key(&mut a, key(KeyCode::Left), Some(&tx));
        handle_key(&mut a, key(KeyCode::Esc), Some(&tx));
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.line_input, "keep me"); // preserved
        assert_eq!(a.line_cursor, 5); // cursor position preserved
        handle_key(&mut a, key(KeyCode::Char('i')), Some(&tx));
        assert_eq!(a.mode, Mode::Insert);
        assert_eq!(a.line_input, "keep me");
        // Editing resumes at the same cursor position.
        handle_key(&mut a, key(KeyCode::Char('!')), Some(&tx));
        assert_eq!(a.line_input, "keep !me");
        assert_eq!(a.line_cursor, 6);
    }

    #[test]
    fn line_submit_encodes_per_txnl_and_records_ordering() {
        let mut a = line_app();
        a.txnl = TxNewline::CrLf;
        let (tx, rx) = mpsc::channel::<ToWorker>();
        for c in "hi".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        let sent = drain_tx(&rx);
        assert_eq!(sent, vec![b"hi\r\n".to_vec()]);
        assert!(a.line_input.is_empty());
        // Empty Enter still sends the bare ending (raw-Enter parity).
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert_eq!(drain_tx(&rx), vec![b"\r\n".to_vec()]);
        // Empty line was NOT recorded in history.
        assert_eq!(a.line_history, ["hi"]);
    }

    #[test]
    fn line_mode_printable_utf8_and_ignored_ctrls() {
        let mut a = line_app();
        let (tx, rx) = mpsc::channel::<ToWorker>();
        handle_key(&mut a, key(KeyCode::Char('é')), Some(&tx));
        handle_key(&mut a, key(KeyCode::Char('日')), Some(&tx));
        assert_eq!(a.line_input, "é日");
        assert_eq!(a.line_cursor, 2); // char-wise, not bytes
        handle_key(&mut a, ctrl(KeyCode::Char('d')), Some(&tx));
        assert!(drain_tx(&rx).is_empty(), "line mode sends only on Enter");
        assert_eq!(a.line_input, "é日");
    }

    #[test]
    fn line_esc_back_to_normal_i_resumes_pending_text() {
        let mut a = line_app();
        a.line_input = "pending".into();
        a.line_cursor = 3;
        handle_key(&mut a, key(KeyCode::Esc), None);
        assert_eq!(a.mode, Mode::Normal);
        handle_key(&mut a, key(KeyCode::Char('i')), None);
        assert_eq!(a.mode, Mode::Insert);
        assert_eq!(a.line_input, "pending");
        assert_eq!(a.line_cursor, 3);
    }

    // ------------------------------------------------------------------
    // Motions, follow toggle, pending gg/ZZ, search prompt
    // ------------------------------------------------------------------

    #[test]
    fn help_popup_captures_keys_no_tx_any_key_closes() {
        let mut a = app();
        a.mode = Mode::Insert; // help is mode-independent: INSERT included
        a.help_visible = true;
        let (tx, rx) = mpsc::channel::<ToWorker>();

        // A printable key while the popup is open: closes it, sends nothing,
        // leaves the mode untouched (the key never reaches INSERT handling).
        assert_eq!(
            handle_key(&mut a, key(KeyCode::Char('x')), Some(&tx)),
            KeyResult::Handled
        );
        assert!(!a.help_visible);
        assert!(rx.try_recv().is_err(), "no TX may flow while help is open");
        assert_eq!(a.mode, Mode::Insert);

        // Re-open, then Esc also closes (any non-page key closes).
        a.help_visible = true;
        handle_key(&mut a, key(KeyCode::Esc), Some(&tx));
        assert!(!a.help_visible);
        assert_eq!(a.mode, Mode::Insert);
        assert!(rx.try_recv().is_err());

        // `q` closes too (the vim quit reflex; not a page key here).
        a.help_visible = true;
        handle_key(&mut a, key(KeyCode::Char('q')), Some(&tx));
        assert!(!a.help_visible);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn help_popup_page_navigation_j_k_clamped() {
        let mut a = app();
        a.help_visible = true;
        assert_eq!(a.help_page, 0);

        // `j` pages forward; past the last page it clamps and the popup stays
        // open (no wrap).
        handle_key(&mut a, key(KeyCode::Char('j')), None);
        assert_eq!(a.help_page, 1);
        assert!(a.help_visible);
        handle_key(&mut a, key(KeyCode::Char('j')), None);
        assert_eq!(a.help_page, 1, "clamped at last page");
        assert!(a.help_visible);

        // `k` pages back; past the first page it clamps and stays open.
        handle_key(&mut a, key(KeyCode::Char('k')), None);
        assert_eq!(a.help_page, 0);
        assert!(a.help_visible);
        handle_key(&mut a, key(KeyCode::Char('k')), None);
        assert_eq!(a.help_page, 0, "clamped at first page");
        assert!(a.help_visible);

        // Any other key closes from a non-zero page and resets the page.
        handle_key(&mut a, key(KeyCode::Char('j')), None);
        handle_key(&mut a, key(KeyCode::Esc), None);
        assert!(!a.help_visible);
        assert_eq!(a.help_page, 0, "page resets on close");
    }

    #[test]
    fn help_popup_page_navigation_ctrl_f_b_and_g_g() {
        let mut a = app();
        a.help_visible = true;

        // Ctrl-F pages forward (like vim's page-down), G jumps toward the end.
        handle_key(&mut a, ctrl(KeyCode::Char('f')), None);
        assert_eq!(a.help_page, 1);
        // G clamps at the last page.
        handle_key(&mut a, key(KeyCode::Char('G')), None);
        assert_eq!(a.help_page, 1);

        // Ctrl-B pages back (vim page-up); `g` (as in gg) pages back too.
        handle_key(&mut a, ctrl(KeyCode::Char('b')), None);
        assert_eq!(a.help_page, 0);
        // g clamps at the first page.
        handle_key(&mut a, key(KeyCode::Char('g')), None);
        assert_eq!(a.help_page, 0);
        assert!(a.help_visible);
    }

    #[test]
    fn help_opens_via_command_and_question_mark() {
        let mut a = app();
        a.mode = Mode::Command;
        let (tx, _rx) = mpsc::channel();
        for c in "help".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert!(a.help_visible, ":help opens the popup");
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.help_page, 0);

        // The page index resets to the first page on every (re)open.
        handle_key(&mut a, key(KeyCode::Char('j')), None);
        assert_eq!(a.help_page, 1);
        handle_key(&mut a, key(KeyCode::Esc), None);
        handle_key(&mut a, key(KeyCode::Char('?')), None);
        assert!(a.help_visible);
        assert_eq!(a.help_page, 0, "reopen resets to page 1");

        let mut b = app();
        handle_key(&mut b, key(KeyCode::Char('?')), None);
        assert!(b.help_visible, "? in NORMAL opens the popup");
        assert_eq!(b.mode, Mode::Normal);
    }

    #[test]
    fn motions_j_k_and_page_keys() {
        let mut a = app_with_lines(20);
        a.viewport_rows = 4;
        handle_key(&mut a, key(KeyCode::Char('k')), None);
        assert!(!a.follow_tail);
        assert_eq!(a.view_offset, 1);
        handle_key(&mut a, key(KeyCode::Char('j')), None);
        assert_eq!(a.view_offset, 0);
        for _ in 0..5 {
            handle_key(&mut a, key(KeyCode::Char('k')), None);
        }
        assert_eq!(a.view_offset, 5);
        handle_key(&mut a, ctrl(KeyCode::Char('u')), None); // half = 2 up
        assert_eq!(a.view_offset, 7);
        handle_key(&mut a, ctrl(KeyCode::Char('f')), None); // full = 4 down
        assert_eq!(a.view_offset, 3);
        handle_key(&mut a, ctrl(KeyCode::Char('b')), None); // full = 4 up
        assert_eq!(a.view_offset, 7);
        handle_key(&mut a, key(KeyCode::Char('j')), None); // line down
        assert_eq!(a.view_offset, 6);
        // j saturates at the tail and does not re-enable follow-tail.
        for _ in 0..10 {
            handle_key(&mut a, key(KeyCode::Char('j')), None);
        }
        assert_eq!(a.view_offset, 0);
        assert!(!a.follow_tail);
    }

    #[test]
    fn gg_pending_and_g() {
        let mut a = app_with_lines(30);
        a.viewport_rows = 5;
        handle_key(&mut a, key(KeyCode::Char('G')), None);
        assert!(a.follow_tail);
        assert_eq!(a.view_offset, 0);
        a.motion_up(4);
        // pending g: first g sets pending, second jumps to top.
        handle_key(&mut a, key(KeyCode::Char('g')), None);
        assert_eq!(a.pending, Some(PendingKey::G));
        assert_eq!(a.view_offset, 4);
        handle_key(&mut a, key(KeyCode::Char('g')), None);
        assert_eq!(a.pending, None);
        assert!(!a.follow_tail);
        assert_eq!(a.view_offset, 25); // 30 - 5 viewport rows
        // Pending cancelled by an unrelated key.
        a.motion_up(1);
        handle_key(&mut a, key(KeyCode::Char('g')), None);
        handle_key(&mut a, key(KeyCode::Char('x')), None);
        assert_eq!(a.pending, None);
        assert_eq!(a.view_offset, 26); // x did not move (25 after gg, +1 up)
        // gg on an empty buffer is a no-op.
        let mut b = app();
        handle_key(&mut b, key(KeyCode::Char('g')), None);
        handle_key(&mut b, key(KeyCode::Char('g')), None);
        assert!(b.follow_tail);
    }

    #[test]
    fn zz_quits_and_f_p_toggle_follow() {
        let mut a = app_with_lines(5);
        assert_eq!(
            handle_key(&mut a, key(KeyCode::Char('Z')), None),
            KeyResult::Handled
        );
        assert_eq!(a.pending, Some(PendingKey::Z));
        assert_eq!(
            handle_key(&mut a, key(KeyCode::Char('Z')), None),
            KeyResult::Quit
        );
        // Single Z cancelled by another key does not quit.
        let mut b = app();
        handle_key(&mut b, key(KeyCode::Char('Z')), None);
        assert_eq!(
            handle_key(&mut b, key(KeyCode::Char('x')), None),
            KeyResult::Handled
        );

        let mut c = app_with_lines(5);
        c.motion_up(2);
        assert!(!c.follow_tail);
        handle_key(&mut c, key(KeyCode::Char('F')), None);
        assert!(c.follow_tail);
        assert_eq!(c.view_offset, 0);
        handle_key(&mut c, key(KeyCode::Char('p')), None);
        assert!(!c.follow_tail);
    }

    #[test]
    fn zz_refused_with_active_log() {
        let mut a = app_with_lines(5);
        a.log_on = true;
        handle_key(&mut a, key(KeyCode::Char('Z')), None);
        assert_eq!(
            handle_key(&mut a, key(KeyCode::Char('Z')), None),
            KeyResult::Handled
        );
        assert!(!a.quit);
        assert!(a.last_error.clone().unwrap().contains(":q!"));
        // With the log closed ZZ quits again.
        a.log_on = false;
        handle_key(&mut a, key(KeyCode::Char('Z')), None);
        assert_eq!(
            handle_key(&mut a, key(KeyCode::Char('Z')), None),
            KeyResult::Quit
        );
    }

    // ------------------------------------------------------------------
    // Port-DOWN UX — TX attempts surface an error, never vanish
    // ------------------------------------------------------------------

    #[test]
    fn raw_insert_while_down_errors_and_sends_nothing() {
        let mut a = app(); // connected = false (never connected / after DOWN)
        a.mode = Mode::Insert;
        let (tx, rx) = mpsc::channel::<ToWorker>();
        handle_key(&mut a, key(KeyCode::Char('x')), Some(&tx));
        assert!(rx.try_recv().is_err(), "no TX may flow while DOWN");
        assert!(a.last_error.clone().unwrap().contains(":r to reconnect"));
    }

    #[test]
    fn line_submit_while_down_errors_and_preserves_input() {
        let mut a = line_app();
        a.connected = false; // simulate a disconnect event
        let (tx, rx) = mpsc::channel::<ToWorker>();
        for c in "keepme".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), Some(&tx));
        }
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert!(rx.try_recv().is_err(), "no TX may flow while DOWN");
        assert!(a.last_error.clone().unwrap().contains(":r to reconnect"));
        assert_eq!(a.line_input, "keepme", "pending input must be preserved");
        assert!(a.line_history.is_empty());
    }

    #[test]
    fn disconnect_event_then_reconnect_clears_error() {
        let mut a = line_app();
        let (tx, rx) = mpsc::channel();
        tx.send(FromWorker::Disconnected("usb unplug".into()))
            .unwrap();
        drop(tx);
        a.drain_worker_at(&rx, std::time::Instant::now());
        assert!(!a.connected);
        assert!(a.last_error.is_some());
        // `:r`-style successful reconnect (command::connect path, tested in
        // command.rs against real spawn) clears the stale error; here just the
        // UI-relevant invariant: DOWN is visible, so the top bar can flip.
        assert_eq!(crate::ui::connection_info(&a), "disconnected");
    }

    #[test]
    fn search_prompt_flow_and_cancel() {
        let mut a = app_with_lines(6);
        for (i, line) in a.scrollback.iter_mut().enumerate() {
            if i == 4 {
                line.bytes = b"FINDME here".to_vec();
            }
        }
        handle_key(&mut a, key(KeyCode::Char('/')), None);
        assert_eq!(a.search_prompt.as_deref(), Some(""));
        for c in "findme".chars() {
            handle_key(&mut a, key(KeyCode::Char(c)), None);
        }
        assert_eq!(a.search_prompt.as_deref(), Some("findme"));
        handle_key(&mut a, key(KeyCode::Enter), None);
        assert_eq!(a.search_prompt, None);
        assert_eq!(a.last_match, Some(4));
        assert!(!a.follow_tail);
        // n re-steps with the stored pattern (wraps to itself, single match).
        handle_key(&mut a, key(KeyCode::Char('n')), None);
        assert_eq!(a.last_match, Some(4));

        // Esc cancels the prompt without searching.
        handle_key(&mut a, key(KeyCode::Char('/')), None);
        handle_key(&mut a, key(KeyCode::Char('x')), None);
        handle_key(&mut a, key(KeyCode::Esc), None);
        assert_eq!(a.search_prompt, None);
        assert_eq!(a.last_search.as_deref(), Some("findme")); // unchanged

        // Empty pattern Enter is a no-op: prompt closes, saved search intact.
        handle_key(&mut a, key(KeyCode::Char('/')), None);
        handle_key(&mut a, key(KeyCode::Enter), None);
        assert_eq!(a.search_prompt, None);
        assert_eq!(
            a.last_search.as_deref(),
            Some("findme"),
            "empty-pattern Enter must not clear or change the saved search"
        );
    }

    #[test]
    fn search_n_backward_wrap() {
        let mut a = app_with_lines(9); // pattern "5" matches line5 only... use "line": all
        a.last_search = Some("line".into());
        a.last_match = Some(0);
        handle_key(&mut a, key(KeyCode::Char('N')), None);
        assert_eq!(a.last_match, Some(8)); // wraps backward
        handle_key(&mut a, key(KeyCode::Char('n')), None);
        assert_eq!(a.last_match, Some(0)); // wraps forward
    }

    // ------------------------------------------------------------------
    // Modifier hygiene: Ctrl/Alt combos are inert outside the deliberate keys
    // ------------------------------------------------------------------

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn normal_ctrl_and_alt_letters_are_inert_except_page_motions() {
        let mut a = app_with_lines(10);
        a.viewport_rows = 4;
        a.motion_up(3); // offset 3, follow_tail off
        assert!(!a.follow_tail);

        // crossterm reports e.g. Ctrl-N as Char('n') + CONTROL: none of these
        // may act as their plain-key counterparts.
        handle_key(&mut a, ctrl(KeyCode::Char('j')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('k')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('n')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('N')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('G')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('g')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('i')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('Z')), None);
        handle_key(&mut a, ctrl(KeyCode::Char(':')), None);
        handle_key(&mut a, alt(KeyCode::Char('k')), None);
        assert_eq!(a.view_offset, 3, "Ctrl/Alt combos must not scroll");
        assert_eq!(a.pending, None, "Ctrl-G must not start gg");
        assert!(!a.follow_tail, "Ctrl-G must not run goto_end");
        assert_eq!(a.mode, Mode::Normal, "Ctrl-I must not enter INSERT");

        // The deliberate page motions still fire (arms precede the gate).
        handle_key(&mut a, ctrl(KeyCode::Char('u')), None); // half = 2 up
        assert_eq!(a.view_offset, 5);
        handle_key(&mut a, ctrl(KeyCode::Char('d')), None); // half = 2 down
        assert_eq!(a.view_offset, 3);
    }

    #[test]
    fn search_and_command_prompts_reject_ctrl_alt_chars() {
        let mut a = app();
        // `/` prompt: Ctrl-A / Alt-A type nothing.
        handle_key(&mut a, key(KeyCode::Char('/')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('a')), None);
        handle_key(&mut a, alt(KeyCode::Char('a')), None);
        assert_eq!(a.search_prompt.as_deref(), Some(""));
        // Plain chars still type into the pattern.
        handle_key(&mut a, key(KeyCode::Char('x')), None);
        assert_eq!(a.search_prompt.as_deref(), Some("x"));
        handle_key(&mut a, key(KeyCode::Esc), None);

        // `:` line: Ctrl-U / Ctrl-Q type nothing (they used to push a literal
        // `u` / `q`).
        handle_key(&mut a, key(KeyCode::Char(':')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('u')), None);
        handle_key(&mut a, ctrl(KeyCode::Char('q')), None);
        assert!(a.command_line.is_empty());
        // Plain chars still type into the command line.
        handle_key(&mut a, key(KeyCode::Char('q')), None);
        assert_eq!(a.command_line, "q");
    }

    #[test]
    fn empty_command_line_enter_is_silently_ignored() {
        let mut a = app();
        let (tx, _rx) = mpsc::channel::<ToWorker>();

        // `:` + Enter closes the line without a parse error (vim behaviour).
        a.mode = Mode::Command;
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.last_error.is_none());
        assert!(a.command_line.is_empty());

        // All-whitespace line: same silent treatment.
        a.mode = Mode::Command;
        a.command_line.push_str("   ");
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.last_error.is_none());

        // Contrast: a bogus NON-empty line still surfaces the parse error.
        a.mode = Mode::Command;
        a.command_line.push_str("bogus");
        handle_key(&mut a, key(KeyCode::Enter), Some(&tx));
        assert!(a.last_error.is_some());
    }
}
