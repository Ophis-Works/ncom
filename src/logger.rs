//! Session log writer (`:log`): appends raw RX+TX bytes — no annotations — to
//! `path` in append mode (created if missing, never truncated); `:log off`
//! closes it.
//!
//! Ordering & durability: the main loop is single-threaded, so file order equals
//! stream order by construction — TX is written by [`Logger::write`] from
//! `App::record_tx` after the worker-channel send succeeded (bytes are logged
//! iff they were successfully QUEUED to the serial worker; a later
//! worker-side write failure can still mean they never reached the wire —
//! there is no wire-level acknowledgement. A failed send logs nothing), RX
//! from the drain path at arrival, before the line splitter. Writes go
//! straight to the open file handle (no buffering layer), so a crash/kill
//! loses nothing already handed to the logger; data reaches the OS page cache
//! only — there is no fsync.

use std::fs::OpenOptions;
use std::io::Write;

#[derive(Default)]
pub struct Logger {
    writer: Option<std::fs::File>,
}

impl Logger {
    /// Open `path` for appending (create if missing). A failed open leaves the
    /// previous state untouched (logging stays off / the old file stays open).
    pub fn open(&mut self, path: &str) -> std::io::Result<()> {
        self.writer = Some(OpenOptions::new().append(true).create(true).open(path)?);
        Ok(())
    }

    /// Append raw bytes, unbuffered (see module docs). A no-op while
    /// logging is off; write errors are swallowed (a dead disk must not
    /// take down the terminal — the session continues without logging).
    pub fn write(&mut self, bytes: &[u8]) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.write_all(bytes);
        }
    }

    /// Close the log file (idempotent); further writes are dropped.
    pub fn close(&mut self) {
        self.writer = None;
    }

    /// Whether a log file is currently open (test-visible; the app tracks the
    /// same state in `App::log_on`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_on(&self) -> bool {
        self.writer.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("ncom-log-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn append_preserves_rx_tx_interleave_order() {
        let path = temp_path("order");
        let mut log = Logger::default();
        log.open(&path).unwrap();
        log.write(b"tx1>"); // TX at send time
        log.write(b"rx1\n"); // RX at arrival
        log.write(b"tx2>");
        log.write(b"rx2\n");
        log.close();
        assert_eq!(std::fs::read(&path).unwrap(), b"tx1>rx1\ntx2>rx2\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn open_appends_does_not_truncate() {
        let path = temp_path("append");
        std::fs::write(&path, b"old").unwrap();
        let mut log = Logger::default();
        log.open(&path).unwrap();
        log.write(b"new");
        log.close();
        assert_eq!(std::fs::read(&path).unwrap(), b"oldnew");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn open_failure_leaves_state_untouched() {
        let mut log = Logger::default();
        // Opening a path *under* a regular file fails (ENOTDIR) -> stays off.
        let bad = temp_path("blocker");
        std::fs::write(&bad, b"x").unwrap();
        assert!(log.open(&format!("{bad}/sub/log")).is_err());
        assert!(!log.is_on());
        log.write(b"dropped"); // still off: nothing written anywhere

        // An open log survives a failed re-open (state untouched).
        let path = temp_path("keep");
        log.open(&path).unwrap();
        assert!(log.open("/nonexistent-root-dir/x").is_err());
        assert!(log.is_on());
        log.write(b"kept\n");
        log.close();
        assert_eq!(std::fs::read(&path).unwrap(), b"kept\n");
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&bad).ok();
    }

    #[test]
    fn close_stops_writing_and_is_idempotent() {
        let path = temp_path("close");
        let mut log = Logger::default();
        log.open(&path).unwrap();
        log.write(b"before");
        log.close();
        log.close(); // idempotent
        log.write(b"after");
        assert!(!log.is_on());
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write_while_off_is_noop() {
        let mut log = Logger::default();
        log.write(b"ignored");
        assert!(!log.is_on());
    }
}
