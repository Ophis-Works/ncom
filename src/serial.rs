//! SerialWorker: owns serialport::SerialPort, reader thread, mpsc channels.
//!
//! Invariants: a short port timeout (see `READ_TIMEOUT`) keeps `read()`
//! returning regularly so the reader can serve main-thread commands and
//! notice USB unplug; any non-timeout read error is treated as a disconnect,
//! because after unplug the fd only ever reports POLLHUP/EIO and retrying
//! cannot make progress.

use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Idle time after which a partial RX line is flushed as a line of its own.
pub const LINE_IDLE_FLUSH: Duration = Duration::from_millis(100);

/// Partial-line cap (64 KiB): a chatty device that never sends `\n` would
/// otherwise grow `partial` without bound (memory + every later render/decode
/// of one gigantic line). When `partial` reaches the cap it is force-flushed as
/// a line flagged `truncated`; the remainder keeps accumulating (also flagged
/// when it finally terminates — see `LineSplitter::feed`).
pub const MAX_PARTIAL_LINE: usize = 64 * 1024;

/// Short port timeout used at open so `read()` returns regularly: the reader
/// thread stays free to serve main-thread commands and notice USB unplug
/// instead of blocking forever. On the Linux read path this timeout is the
/// `ppoll()` deadline; timeout surfaces as `ErrorKind::TimedOut`, hangup as
/// `ErrorKind::BrokenPipe`.
const READ_TIMEOUT: Duration = Duration::from_millis(25);

// ---------------------------------------------------------------------------
// Hardware-independent config types + mapping to serialport enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NcomParity {
    None,
    Even,
    Odd,
    Mark,
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NcomStopBits {
    One,
    OneAndHalf,
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NcomFlow {
    None,
    XonXoff,
    RtsCts,
}

/// Connection settings as the app talks about them (`:baud`, `:parity`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialConfig {
    pub path: String,
    pub baud: u32,
    pub data_bits: u8, // 5..=8
    pub parity: NcomParity,
    pub stop_bits: NcomStopBits,
    pub flow: NcomFlow,
}

/// Map our parity to `serialport::Parity`.
///
/// serialport 4.10.1 `Parity` has only `None | Odd | Even` (src/lib.rs:205).
/// `Mark`/`Space` parity is not representable, so the mapping fails instead of
/// silently substituting something else.
pub fn map_parity(p: NcomParity) -> Result<serialport::Parity, String> {
    match p {
        NcomParity::None => Ok(serialport::Parity::None),
        NcomParity::Even => Ok(serialport::Parity::Even),
        NcomParity::Odd => Ok(serialport::Parity::Odd),
        NcomParity::Mark => Err("mark parity not supported by the serialport crate".into()),
        NcomParity::Space => Err("space parity not supported by the serialport crate".into()),
    }
}

/// Map our stop bits to `serialport::StopBits`.
///
/// serialport 4.10.1 `StopBits` has only `One | Two` (src/lib.rs:231) — there
/// is no 1.5-stop-bit mode, so `OneAndHalf` fails the mapping.
pub fn map_stop_bits(s: NcomStopBits) -> Result<serialport::StopBits, String> {
    match s {
        NcomStopBits::One => Ok(serialport::StopBits::One),
        NcomStopBits::Two => Ok(serialport::StopBits::Two),
        NcomStopBits::OneAndHalf => {
            Err("1.5 stop bits not supported by the serialport crate".into())
        }
    }
}

/// Map our flow control to `serialport::FlowControl`
/// (`None | Software | Hardware`, src/lib.rs:272).
pub fn map_flow(f: NcomFlow) -> Result<serialport::FlowControl, String> {
    match f {
        NcomFlow::None => Ok(serialport::FlowControl::None),
        NcomFlow::XonXoff => Ok(serialport::FlowControl::Software),
        NcomFlow::RtsCts => Ok(serialport::FlowControl::Hardware),
    }
}

/// Map our data bits (5-8) to `serialport::DataBits` (`Five..Eight`,
/// src/lib.rs:144).
pub fn map_data_bits(d: u8) -> Result<serialport::DataBits, String> {
    match d {
        5 => Ok(serialport::DataBits::Five),
        6 => Ok(serialport::DataBits::Six),
        7 => Ok(serialport::DataBits::Seven),
        8 => Ok(serialport::DataBits::Eight),
        other => Err(format!("invalid databits {other} (expected 5-8)")),
    }
}

// ---------------------------------------------------------------------------
// Pure RX helpers: line splitting and decoding
// ---------------------------------------------------------------------------

/// Accumulates raw RX bytes and emits lines on `\n`, or on an idle timeout
/// flush. Lone `\r` immediately before `\n` is stripped; a `\r` not followed
/// by `\n` is kept (it may be a pending CRLF split across reads, or a lone CR).
///
/// Every emitted line carries a `truncated` flag: `true` when the line was cut
/// short by the [`MAX_PARTIAL_LINE`] cap (force-flushed) OR is the tail of a
/// logical line whose earlier part was cut off — i.e. any line produced from a
/// partial that had previously been force-flushed. The flag resets once a line
/// terminates normally after an unterminated run.
pub struct LineSplitter {
    partial: Vec<u8>,
    /// Set while `partial` is the tail of a line that was force-flushed at the
    /// cap, so the eventual terminator (or idle flush) inherits the flag.
    partial_truncated: bool,
    last_byte: Option<Instant>,
    idle: Duration,
}

impl LineSplitter {
    pub fn new(idle: Duration) -> Self {
        LineSplitter {
            partial: Vec::new(),
            partial_truncated: false,
            last_byte: None,
            idle,
        }
    }

    /// Test-only convenience (nothing in the app path reads it); gated rather
    /// than `allow`ed so dead-code analysis still covers the rest of the type.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.partial.is_empty()
    }

    /// Feed a received chunk; returns every complete line as
    /// `(bytes, truncated)` (bytes without `\n`).
    pub fn feed(&mut self, chunk: &[u8], now: Instant) -> Vec<(Vec<u8>, bool)> {
        let mut lines = Vec::new();
        for &b in chunk {
            self.last_byte = Some(now);
            if b == b'\n' {
                let mut line = std::mem::take(&mut self.partial);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                lines.push((line, self.partial_truncated));
                self.partial_truncated = false;
            } else {
                if self.partial.len() >= MAX_PARTIAL_LINE {
                    let full = std::mem::take(&mut self.partial);
                    lines.push((full, true));
                    self.partial_truncated = true;
                }
                self.partial.push(b);
            }
        }
        lines
    }

    /// If a partial line has been idle for at least `idle`, flush it as a
    /// line. Returns `None` while the buffer is empty or still warm.
    pub fn flush_idle(&mut self, now: Instant) -> Option<(Vec<u8>, bool)> {
        if self.partial.is_empty() {
            return None;
        }
        let last = self.last_byte?;
        if now.duration_since(last) >= self.idle {
            let truncated = self.partial_truncated;
            self.partial_truncated = false;
            return Some((std::mem::take(&mut self.partial), truncated));
        }
        None
    }
}

/// Lossy-decode received bytes to `String`: invalid UTF-8 becomes U+FFFD and
/// control bytes are shown as caret notation (`0x1b` -> `^[`, `0x7f` -> `^?`).
/// Tab (0x09) is kept as a literal tab; `\n` never reaches this (lines are
/// already split) and a lone `\r` renders as `^M`.
pub fn decode_lossy_caret(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Caret notation: C0 controls except TAB (and CR/LF never arrive here,
        // but if they do they are caret-encoded too), plus DEL.
        if (b < 0x20 && b != b'\t') || b == 0x7f {
            out.push('^');
            out.push((b ^ 0x40) as char);
            i += 1;
            continue;
        }
        // Decode up to the NEXT control byte: a whole-slice from_utf8 would
        // otherwise succeed across embedded controls and skip their caret
        // encoding (controls are valid UTF-8).
        let run_end = bytes[i..]
            .iter()
            .position(|&b| (b < 0x20 && b != b'\t') || b == 0x7f)
            .map_or(bytes.len(), |p| i + p);
        match std::str::from_utf8(&bytes[i..run_end]) {
            Ok(s) => {
                out.push_str(s);
                if run_end == bytes.len() {
                    break;
                }
                i = run_end; // next byte is a control: caret-encode it
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    // The error guarantees bytes[..valid] is valid UTF-8.
                    out.push_str(std::str::from_utf8(&bytes[i..i + valid]).unwrap());
                }
                // error_len()/valid_up_to() are relative to the sliced input;
                // an incomplete trailing sequence has error_len() == None and
                // consumes the rest of the slice.
                let bad = e.error_len().unwrap_or(run_end - i - valid).max(1);
                for _ in 0..bad {
                    out.push('\u{FFFD}');
                }
                i += valid + bad;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Pure TX helper: `:txnl` line-ending encoding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TxNewline {
    #[default]
    Cr,
    Lf,
    CrLf,
}

impl TxNewline {
    pub fn ending(self) -> &'static [u8] {
        match self {
            TxNewline::Cr => b"\r",
            TxNewline::Lf => b"\n",
            TxNewline::CrLf => b"\r\n",
        }
    }

    /// Encode one transmitted line (without its terminator) per `:txnl`.
    pub fn encode_line(self, line: &str) -> Vec<u8> {
        let mut v = line.as_bytes().to_vec();
        v.extend_from_slice(self.ending());
        v
    }
}

// ---------------------------------------------------------------------------
// Worker: port ownership, reader loop, main<->worker messages
// ---------------------------------------------------------------------------

/// Main thread -> worker messages.
#[derive(Debug, Clone)]
pub enum ToWorker {
    /// Raw bytes to transmit.
    Tx(Vec<u8>),
    /// Live reconfiguration; applied in place, with reopen as fallback.
    Config {
        baud: u32,
        data_bits: u8,
        parity: NcomParity,
        stop_bits: NcomStopBits,
        flow: NcomFlow,
    },
    Dtr(bool),
    Rts(bool),
}

/// Worker -> main thread events.
#[derive(Debug)]
pub enum FromWorker {
    /// Received chunk with its arrival time.
    Data(Vec<u8>, Instant),
    /// The port died (read error / USB unplug). The worker thread has stopped.
    Disconnected(String),
}

pub struct SerialWorker {
    /// The open port; briefly `None` inside [`SerialWorker::reopen`] while the
    /// old handle is closed and a fresh one is opened (see its docs).
    port: Option<Box<dyn serialport::SerialPort>>,
    cfg: SerialConfig,
    to_main: mpsc::Sender<FromWorker>,
    from_main: mpsc::Receiver<ToWorker>,
}

impl SerialWorker {
    /// Open `cfg` and start the worker loop on a dedicated thread. Returns a
    /// handle plus the event receiver for the main thread.
    pub fn spawn(
        cfg: SerialConfig,
    ) -> anyhow::Result<(mpsc::Sender<ToWorker>, mpsc::Receiver<FromWorker>)> {
        let port = open_port(&cfg)?;
        let (to_main, from_worker_rx) = mpsc::channel::<FromWorker>();
        let (to_worker_tx, from_main) = mpsc::channel::<ToWorker>();
        let worker = SerialWorker {
            port: Some(port),
            cfg,
            to_main,
            from_main,
        };
        std::thread::Builder::new()
            .name("serial-worker".into())
            .spawn(move || worker.run())?;
        Ok((to_worker_tx, from_worker_rx))
    }

    /// Worker loop: block on `port.read()` (short port timeout so this returns
    /// regularly), forward chunks, and serve main-thread messages on EVERY
    /// read outcome — after each forwarded chunk and on the zero-byte and
    /// timeout paths alike — so a device streaming without a >=25 ms gap
    /// cannot starve TX/reconfig/pin commands. Any non-timeout read error ends
    /// the loop with a `Disconnected` event.
    fn run(mut self) {
        let mut buf = [0u8; 4096];
        // `port` is a short-lived reborrow of `self.port`; by the time
        // `serve_commands` runs in any arm it is no longer used (the data path
        // only touches the disjoint `to_main` field first), so `serve_commands`
        // (and a reopen that swaps the port out) can borrow `self` again. When
        // reopen has taken the port out and failed, the loop ends on the next
        // iteration.
        while let Some(port) = self.port.as_mut() {
            match port.read(&mut buf) {
                Ok(0) => {
                    // Read ready but no data: serve commands here too, or a
                    // port reporting Ok(0) forever would busy-spin while
                    // starving main-thread commands.
                    if self.serve_commands() {
                        break;
                    }
                }
                Ok(n) => {
                    let now = Instant::now();
                    if self
                        .to_main
                        .send(FromWorker::Data(buf[..n].to_vec(), now))
                        .is_err()
                    {
                        break; // main thread is gone
                    }
                    // Data forwarded: still drain commands, or a device
                    // streaming with no >=25 ms gap would never hit the
                    // TimedOut branch below and starve TX/reconfig/pins.
                    if self.serve_commands() {
                        break;
                    }
                }
                Err(e) if e.kind() == IoErrorKind::TimedOut => {
                    // Port idle: handle pending commands, then read again.
                    if self.serve_commands() {
                        break;
                    }
                }
                Err(e) => {
                    // USB unplug / hangup / device gone: report and stop.
                    let _ = self.to_main.send(FromWorker::Disconnected(e.to_string()));
                    break;
                }
            }
        }
    }

    /// Drain pending main-thread messages. Returns true if the worker must
    /// stop (main thread gone / fatal error applying config).
    fn serve_commands(&mut self) -> bool {
        loop {
            match self.from_main.try_recv() {
                Ok(ToWorker::Tx(bytes)) => match self.port.as_mut() {
                    Some(port) => {
                        if let Err(e) = port.write_all(&bytes) {
                            let _ = self.to_main.send(FromWorker::Disconnected(e.to_string()));
                            return true;
                        }
                        let _ = port.flush();
                    }
                    None => return true,
                },
                Ok(ToWorker::Config {
                    baud,
                    data_bits,
                    parity,
                    stop_bits,
                    flow,
                }) => {
                    self.apply_config(baud, data_bits, parity, stop_bits, flow);
                }
                Ok(ToWorker::Dtr(on)) => {
                    if self.set_pin(|port, level| port.write_data_terminal_ready(level), on) {
                        return true;
                    }
                }
                Ok(ToWorker::Rts(on)) => {
                    if self.set_pin(|port, level| port.write_request_to_send(level), on) {
                        return true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => return true,
            }
        }
    }

    /// Apply a modem-line setter (DTR/RTS); if it fails in place, reopen the
    /// port (which re-asserts the pin per the OS default) and apply the setter
    /// once more on the fresh handle. Returns true if the worker must stop:
    /// reopen failure already reported `Disconnected`; a setter that STILL
    /// fails after a successful reopen is reported the same way (`:dtr 0`
    /// must not silently leave DTR high) and stops the worker.
    fn set_pin(
        &mut self,
        setter: fn(&mut Box<dyn serialport::SerialPort>, bool) -> serialport::Result<()>,
        on: bool,
    ) -> bool {
        if self
            .port
            .as_mut()
            .is_some_and(|port| setter(port, on).is_ok())
        {
            return false;
        }
        if self.reopen().is_err() {
            // reopen() already sent FromWorker::Disconnected.
            return true;
        }
        match self.port.as_mut().map(|port| setter(port, on)) {
            Some(Ok(())) => false,
            // Unreachable after a successful reopen (`port` is Some), but
            // treat a missing handle like any other persistent failure.
            Some(Err(e)) => {
                let _ = self.to_main.send(FromWorker::Disconnected(e.to_string()));
                true
            }
            None => {
                let _ = self
                    .to_main
                    .send(FromWorker::Disconnected("port not open".into()));
                true
            }
        }
    }

    /// Apply new line settings in place; if any setter fails, reopen the port
    /// with the full new configuration (some drivers/adversaries only accept
    /// settings at open time).
    fn apply_config(
        &mut self,
        baud: u32,
        data_bits: u8,
        parity: NcomParity,
        stop_bits: NcomStopBits,
        flow: NcomFlow,
    ) {
        self.cfg.baud = baud;
        self.cfg.data_bits = data_bits;
        self.cfg.parity = parity;
        self.cfg.stop_bits = stop_bits;
        self.cfg.flow = flow;

        if self.apply_in_place().is_err() && self.reopen().is_err() {
            // Keep the new cfg; main thread will see the port DOWN via the
            // next read error (or the reopen error sent as Disconnected).
        }
    }

    /// Apply the settings currently stored in `self.cfg` to the open port
    /// in place. If the port is not open, returns an error so the caller's
    /// reopen fallback runs (mirroring `set_pin`), instead of panicking.
    fn apply_in_place(&mut self) -> Result<(), String> {
        let port = match self.port.as_mut() {
            Some(port) => port,
            None => return Err("port not open".into()),
        };
        port.set_baud_rate(self.cfg.baud)
            .map_err(|e| e.to_string())?;
        port.set_data_bits(map_data_bits(self.cfg.data_bits)?)
            .map_err(|e| e.to_string())?;
        port.set_parity(map_parity(self.cfg.parity)?)
            .map_err(|e| e.to_string())?;
        port.set_stop_bits(map_stop_bits(self.cfg.stop_bits)?)
            .map_err(|e| e.to_string())?;
        port.set_flow_control(map_flow(self.cfg.flow)?)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Reopen the port with the current settings (used when an in-place
    /// setter fails).
    ///
    /// The old handle **must be closed before opening**: serialport 4.10.1
    /// opens ports in exclusive mode by default — TIOCEXCL *and* an exclusive
    /// `flock` (`src/posix/tty.rs:131-136`; builder default
    /// `exclusive: true`, `src/lib.rs:1057` — with
    /// `SerialPortBuilder::exclusive` the opt-out, `src/lib.rs:424-432`) — so
    /// reopening the same device while the old fd is still open fails with
    /// `EWOULDBLOCK` every time. Closing first means a failed reopen also
    /// loses the old handle; that is fine because on reopen failure the
    /// worker reports `Disconnected` and stops either way.
    fn reopen(&mut self) -> anyhow::Result<()> {
        self.port = None; // release the exclusive lock held by the old handle
        match open_port(&self.cfg) {
            Ok(port) => {
                self.port = Some(port);
                Ok(())
            }
            Err(e) => {
                let _ = self.to_main.send(FromWorker::Disconnected(e.to_string()));
                Err(e)
            }
        }
    }
}

fn open_port(cfg: &SerialConfig) -> anyhow::Result<Box<dyn serialport::SerialPort>> {
    // serialport::new(path, baud) then builder methods; `timeout` sets the
    // read/write wait timeout used by the backend poll loop (module docs).
    let parity = map_parity(cfg.parity).map_err(anyhow::Error::msg)?;
    let stop_bits = map_stop_bits(cfg.stop_bits).map_err(anyhow::Error::msg)?;
    let flow = map_flow(cfg.flow).map_err(anyhow::Error::msg)?;
    let data_bits = map_data_bits(cfg.data_bits).map_err(anyhow::Error::msg)?;
    Ok(serialport::new(&cfg.path, cfg.baud)
        .data_bits(data_bits)
        .parity(parity)
        .stop_bits(stop_bits)
        .flow_control(flow)
        .timeout(READ_TIMEOUT)
        .open()?)
}

// ---------------------------------------------------------------------------
// Tests (hardware-independent)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitter_splits_on_newline_and_strips_cr() {
        let mut s = LineSplitter::new(LINE_IDLE_FLUSH);
        let now = Instant::now();
        let lines = s.feed(b"hello\r\nworld\n", now);
        assert_eq!(
            lines,
            vec![(b"hello".to_vec(), false), (b"world".to_vec(), false)]
        );
        assert!(s.is_empty());
        assert_eq!(s.flush_idle(now), None);
    }

    #[test]
    fn splitter_handles_crlf_across_chunks() {
        let mut s = LineSplitter::new(LINE_IDLE_FLUSH);
        let now = Instant::now();
        assert!(s.feed(b"abc\r", now).is_empty());
        assert_eq!(s.feed(b"\ndef", now), vec![(b"abc".to_vec(), false)]);
        assert_eq!(s.partial, b"def");
    }

    #[test]
    fn splitter_keeps_lone_cr() {
        let mut s = LineSplitter::new(LINE_IDLE_FLUSH);
        let now = Instant::now();
        let lines = s.feed(b"a\rb\n", now);
        assert_eq!(lines, vec![(b"a\rb".to_vec(), false)]);
    }

    // Intentional semantics: a `\r` that idle-flushes becomes a line of its
    // own, KEEPING its trailing `\r` (only `\n`-terminated lines strip the
    // CR — see `feed`; the renderer shows it as `^M`); the FOLLOWING `\n`
    // then starts a FRESH line rather than terminating an empty one. We pin
    // this here so a later refactor does not silently change it.
    #[test]
    fn splitter_cr_idle_flush_then_newline_starts_fresh_line() {
        let mut s = LineSplitter::new(Duration::from_millis(100));
        let t0 = Instant::now();
        // "abc" + "\r" arrives, no `\n` follows.
        assert_eq!(s.feed(b"abc\r", t0), Vec::<(Vec<u8>, bool)>::new());
        // Idle flush commits the partial as its own line, trailing `\r` kept.
        assert_eq!(
            s.flush_idle(t0 + Duration::from_millis(150)),
            Some((b"abc\r".to_vec(), false))
        );
        assert!(s.is_empty());
        // The next `\n` finds an empty partial: it closes an empty line and
        // "def" starts a fresh partial.
        assert_eq!(
            s.feed(b"\ndef", t0 + Duration::from_millis(200)),
            vec![(Vec::new(), false)]
        );
        assert_eq!(s.partial, b"def");
        assert_eq!(
            s.feed(b"\n", t0 + Duration::from_millis(250)),
            vec![(b"def".to_vec(), false)]
        );
    }

    #[test]
    fn splitter_idle_flush() {
        let mut s = LineSplitter::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(s.feed(b"partial", t0).is_empty());
        // Still warm at +50 ms.
        assert_eq!(s.flush_idle(t0 + Duration::from_millis(50)), None);
        // Flushed at +150 ms.
        assert_eq!(
            s.flush_idle(t0 + Duration::from_millis(150)),
            Some((b"partial".to_vec(), false))
        );
        assert!(s.is_empty());
        // Nothing more to flush.
        assert_eq!(s.flush_idle(t0 + Duration::from_millis(300)), None);
    }

    #[test]
    fn splitter_restarts_timer_on_new_bytes() {
        let mut s = LineSplitter::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(s.feed(b"a", t0).is_empty());
        assert!(s.feed(b"b", t0 + Duration::from_millis(80)).is_empty());
        // Last byte arrived at +80 ms, so +150 ms is not yet idle.
        assert_eq!(s.flush_idle(t0 + Duration::from_millis(150)), None);
        assert_eq!(
            s.flush_idle(t0 + Duration::from_millis(181)),
            Some((b"ab".to_vec(), false))
        );
    }

    #[test]
    fn splitter_caps_partial_line_and_flags_truncation() {
        let mut s = LineSplitter::new(LINE_IDLE_FLUSH);
        let now = Instant::now();
        // MAX_PARTIAL_LINE + 1 bytes with no `\n`: exactly one capped line
        // comes out, the partial stays bounded by the cap.
        let n = MAX_PARTIAL_LINE + 1;
        let lines = s.feed(&vec![b'x'; n], now);
        assert_eq!(lines.len(), 1);
        let (bytes, truncated) = &lines[0];
        assert_eq!(bytes.len(), MAX_PARTIAL_LINE);
        assert!(*truncated, "force-flushed segment must be flagged");
        assert_eq!(s.partial.len(), 1, "remainder stays bounded by the cap");
        // The tail terminates on `\n` and inherits the truncation flag.
        let tail = s.feed(b"tail\n", now);
        assert_eq!(tail, vec![(b"xtail".to_vec(), true)]);
        // ...and a normal line afterwards is unflagged again.
        assert_eq!(s.feed(b"ok\n", now), vec![(b"ok".to_vec(), false)]);
    }

    #[test]
    fn splitter_idle_flush_of_capped_tail_keeps_flag() {
        let mut s = LineSplitter::new(Duration::from_millis(100));
        let t0 = Instant::now();
        // Force a cap flush, then go silent: the idle flush carries the flag.
        assert_eq!(s.feed(&vec![b'y'; MAX_PARTIAL_LINE + 2], t0).len(), 1);
        let (bytes, truncated) = s.flush_idle(t0 + Duration::from_millis(150)).unwrap();
        assert!(truncated);
        assert_eq!(bytes.len(), 2);
        // Flag cleared for the next line.
        assert!(s.feed(b"z", t0 + Duration::from_millis(200)).is_empty());
        assert_eq!(
            s.flush_idle(t0 + Duration::from_millis(400)),
            Some((b"z".to_vec(), false))
        );
    }

    #[test]
    fn decode_caret_notation() {
        assert_eq!(decode_lossy_caret(b"\x1b[0m"), "^[[0m");
        assert_eq!(decode_lossy_caret(b"\x07"), "^G");
        assert_eq!(decode_lossy_caret(b"\x7f"), "^?");
        assert_eq!(decode_lossy_caret(b"\r"), "^M");
        assert_eq!(decode_lossy_caret(b"\t"), "\t");
        assert_eq!(decode_lossy_caret(b"ok"), "ok");
        // Controls embedded after valid text are caret-encoded too.
        assert_eq!(decode_lossy_caret(b"beta\x07!"), "beta^G!");
        assert_eq!(decode_lossy_caret(b"a\x1bb"), "a^[b");
    }

    #[test]
    fn decode_lossy_utf8() {
        // "héllo" with the é truncated mid-sequence.
        assert_eq!(decode_lossy_caret(b"h\xc3\xa9llo"), "héllo");
        assert_eq!(decode_lossy_caret(b"h\xc3"), "h\u{FFFD}");
        assert_eq!(decode_lossy_caret(b"\xff\xfe"), "\u{FFFD}\u{FFFD}");
        // Valid multi-byte sequence preserved, mixed with controls.
        assert_eq!(decode_lossy_caret("\u{1F600}".as_bytes()), "\u{1F600}");
        assert_eq!(decode_lossy_caret(b"a\xffb\xc3\xa9c"), "a\u{FFFD}béc");
    }

    #[test]
    fn txnl_encoding() {
        assert_eq!(TxNewline::Cr.encode_line("hi"), b"hi\r".to_vec());
        assert_eq!(TxNewline::Lf.encode_line("hi"), b"hi\n".to_vec());
        assert_eq!(TxNewline::CrLf.encode_line("hi"), b"hi\r\n".to_vec());
        assert_eq!(TxNewline::default(), TxNewline::Cr);
    }

    #[test]
    fn config_mapping() {
        assert_eq!(map_parity(NcomParity::None), Ok(serialport::Parity::None));
        assert_eq!(map_parity(NcomParity::Even), Ok(serialport::Parity::Even));
        assert_eq!(map_parity(NcomParity::Odd), Ok(serialport::Parity::Odd));
        assert!(map_parity(NcomParity::Mark).is_err());
        assert!(map_parity(NcomParity::Space).is_err());

        assert_eq!(
            map_stop_bits(NcomStopBits::One),
            Ok(serialport::StopBits::One)
        );
        assert_eq!(
            map_stop_bits(NcomStopBits::Two),
            Ok(serialport::StopBits::Two)
        );
        assert!(map_stop_bits(NcomStopBits::OneAndHalf).is_err());

        assert_eq!(map_flow(NcomFlow::None), Ok(serialport::FlowControl::None));
        assert_eq!(
            map_flow(NcomFlow::XonXoff),
            Ok(serialport::FlowControl::Software)
        );
        assert_eq!(
            map_flow(NcomFlow::RtsCts),
            Ok(serialport::FlowControl::Hardware)
        );

        assert_eq!(map_data_bits(5), Ok(serialport::DataBits::Five));
        assert_eq!(map_data_bits(8), Ok(serialport::DataBits::Eight));
        assert!(map_data_bits(4).is_err());
        assert!(map_data_bits(9).is_err());
    }

    // ------------------------------------------------------------------
    // Task 9: end-to-end worker test against a real kernel pty pair
    // ------------------------------------------------------------------

    /// Create a pty master/slave with `openpty()` (libc; the slave is a real
    /// tty, so `serialport` opens it through the same TTYPort path used for
    /// `/dev/ttyUSB*`, serialport 4.10.1 `src/posix/tty.rs`). Returns the
    /// blocking master fd-wrapped File and the slave's `/dev/pts/N` path
    /// (resolved via `/proc/self/fd` — Linux-first per spec decision 4).
    fn pty_pair() -> (std::fs::File, String) {
        use std::os::fd::FromRawFd;
        let mut master: i32 = -1;
        let mut slave: i32 = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        let path = std::fs::read_link(format!("/proc/self/fd/{slave}"))
            .expect("readlink /proc/self/fd/<slave>")
            .to_string_lossy()
            .into_owned();
        assert!(
            path.starts_with("/dev/pts/"),
            "unexpected slave tty path: {path}"
        );
        let master_file = unsafe { std::fs::File::from_raw_fd(master) };
        // The slave fd is opened independently by serialport; close ours.
        unsafe { libc::close(slave) };
        (master_file, path)
    }

    fn pty_cfg(path: String) -> SerialConfig {
        SerialConfig {
            path,
            baud: 115_200,
            data_bits: 8,
            parity: NcomParity::None,
            stop_bits: NcomStopBits::One,
            flow: NcomFlow::None,
        }
    }

    /// Poll `f` until it returns Some or the timeout elapses (the worker runs
    /// on its own thread with a 25 ms read timeout, so events take a few
    /// iterations to surface).
    fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for worker event"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn worker_rx_tx_disconnect_over_pty() {
        let (mut master, slave_path) = pty_pair();
        let (to_worker, from_worker) =
            SerialWorker::spawn(pty_cfg(slave_path)).expect("spawn on pty");

        // RX: master -> worker -> Data event.
        use std::io::Write;
        master.write_all(b"hello\r\n").unwrap();
        let chunk = wait_for(Duration::from_secs(2), || {
            from_worker.try_recv().ok().map(|ev| match ev {
                FromWorker::Data(bytes, _) => bytes,
                FromWorker::Disconnected(e) => panic!("unexpected disconnect: {e}"),
            })
        });
        assert_eq!(chunk, b"hello\r\n".to_vec());

        // TX: Tx message -> readable from the master.
        to_worker.send(ToWorker::Tx(b"ping\n".to_vec())).unwrap();
        let mut buf = [0u8; 16];
        let n = wait_for(Duration::from_secs(2), || {
            use std::io::Read;
            master.read(&mut buf).ok().filter(|&n| n > 0)
        });
        assert_eq!(&buf[..n], b"ping\n");

        // Disconnect: closing the master ends the slave reads with EIO (not a
        // timeout) -> the worker reports Disconnected and stops.
        drop(master);
        let err = wait_for(Duration::from_secs(3), || match from_worker.try_recv() {
            Ok(FromWorker::Disconnected(e)) => Some(e),
            _ => None,
        });
        assert!(!err.is_empty(), "disconnect must carry the OS error text");
        // Dropping the sender lets the worker's command poll see the closed
        // channel; the thread has already exited its run loop by now.
        drop(to_worker);
    }

    #[test]
    fn worker_survives_reconfig_but_reports_pin_failure_over_pty() {
        let (mut master, slave_path) = pty_pair();
        let (to_worker, from_worker) =
            SerialWorker::spawn(pty_cfg(slave_path)).expect("spawn on pty");

        // Live reconfig: the pts supports the termios setters, so this must
        // NOT stop the worker.
        to_worker
            .send(ToWorker::Config {
                baud: 9600,
                data_bits: 7,
                parity: NcomParity::Even,
                stop_bits: NcomStopBits::One,
                flow: NcomFlow::None,
            })
            .unwrap();

        // Prove the worker is still UP by round-tripping a frame.
        std::thread::sleep(Duration::from_millis(100));
        to_worker.send(ToWorker::Tx(b"ok".to_vec())).unwrap();
        let mut buf = [0u8; 2];
        let n = wait_for(Duration::from_secs(2), || {
            use std::io::Read;
            master.read(&mut buf).ok().filter(|&n| n > 0)
        });
        assert_eq!(&buf[..n], b"ok");
        assert!(
            !matches!(from_worker.try_recv(), Ok(FromWorker::Disconnected(_))),
            "no disconnect may be reported while the pty is alive"
        );

        // Modem lines are a DIFFERENT story: a pty has no DTR/RTS lines —
        // TIOCMBIS/TIOCMBIC fail on the slave fd (serialport 4.10.1
        // `src/posix/tty.rs:258`, `set_pin` -> `ioctl::tiocmbis/tiocmbic`),
        // and after the reopen fallback the setter STILL fails. `set_pin`
        // must then report the failure and stop instead of silently leaving
        // the pin untouched (the `:dtr 0`-left-DTR-high bug this closes; the
        // old code swallowed the failure whenever reopen succeeded).
        to_worker.send(ToWorker::Dtr(true)).unwrap();
        let ev = wait_for(Duration::from_secs(2), || from_worker.try_recv().ok());
        assert!(
            matches!(ev, FromWorker::Disconnected(_)),
            "persistent pin failure must be reported: {ev:?}"
        );
        // And the worker really stopped: a later command send errors.
        std::thread::sleep(Duration::from_millis(100));
        assert!(to_worker.send(ToWorker::Tx(b"x".to_vec())).is_err());
        drop(to_worker);
    }

    /// A device streaming with no >=25 ms gap never lets `port.read` hit the
    /// TimedOut branch, so commands must be served on the `Ok(n)` data path
    /// (and the `Ok(0)` path — not reachable on a pty, covered by the same
    /// `serve_commands` call in `run`). This test streams from the master in a
    /// tight loop while issuing `ToWorker::Tx` and `ToWorker::Config` and
    /// asserts both are applied within a bounded deadline: the TX bytes reach
    /// the master, and the baud change is visible in the pty termios (master
    /// and slave share one termios; serialport applies baud via `cfsetspeed` +
    /// `tcsetattr(TCSANOW)`, serialport 4.10.1 `src/posix/termios.rs:90,195`).
    #[test]
    fn worker_serves_commands_while_streaming_over_pty() {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (mut master, slave_path) = pty_pair();
        let (to_worker, from_worker) =
            SerialWorker::spawn(pty_cfg(slave_path)).expect("spawn on pty");

        // serialport opened the slave in raw mode (`cfmakeraw` disables ECHO,
        // serialport 4.10.1 src/posix/tty.rs:145-155), so the master sees only
        // the worker's TX bytes. Belt-and-braces: clear ECHO via the master
        // too (c_cflag untouched, so the applied baud is not disturbed).
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(master.as_raw_fd(), &mut tio) }, 0);
        tio.c_lflag &= !libc::ECHO;
        assert_eq!(
            unsafe { libc::tcsetattr(master.as_raw_fd(), libc::TCSANOW, &tio) },
            0
        );

        // Discard Data events (and fail on a disconnect) so the unbounded
        // channel does not grow while the worker streams into it.
        let drain_events = || {
            while let Ok(ev) = from_worker.try_recv() {
                assert!(
                    !matches!(ev, FromWorker::Disconnected(_)),
                    "pty must stay alive for the whole test"
                );
            }
        };

        // Writer thread on a dup()ed master fd: keep the pty input queue full
        // (4 KiB writes in a tight loop, no sleeps) so the worker's reads
        // return Ok(n) back-to-back with no TimedOut gap.
        let writer_fd = unsafe { libc::dup(master.as_raw_fd()) };
        assert!(writer_fd >= 0, "dup(master) failed");
        let mut writer_master = unsafe { std::fs::File::from_raw_fd(writer_fd) };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = stop.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let chunk = [b'.'; 4096];
            while !stop_writer.load(Ordering::Relaxed) {
                if writer_master.write_all(&chunk).is_err() {
                    break;
                }
            }
        });

        // TX while streaming: must reach the master promptly (bounded
        // deadline), not only after the stream goes idle.
        let marker = b"ping-while-streaming\n";
        to_worker.send(ToWorker::Tx(marker.to_vec())).unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rbuf = [0u8; 256];
        while !got.windows(marker.len()).any(|w| w == marker) {
            assert!(
                Instant::now() < deadline,
                "TX starved while streaming (old code served commands only on \
                 the TimedOut branch); got so far: {} bytes",
                got.len()
            );
            drain_events();
            use std::io::Read;
            match master.read(&mut rbuf) {
                Ok(0) => std::thread::sleep(Duration::from_millis(2)),
                Ok(n) => got.extend_from_slice(&rbuf[..n]),
                Err(e) => panic!("master read failed: {e}"),
            }
        }

        // Config while streaming: baud 9600 must be applied despite there
        // being no read gaps. Polled against a bounded deadline via the
        // master-side termios.
        to_worker
            .send(ToWorker::Config {
                baud: 9600,
                data_bits: 8,
                parity: NcomParity::None,
                stop_bits: NcomStopBits::One,
                flow: NcomFlow::None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            drain_events();
            let mut tio: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(master.as_raw_fd(), &mut tio) }, 0);
            let ospeed = unsafe { libc::cfgetospeed(&tio) };
            if ospeed == libc::B9600 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Config starved while streaming (ospeed still {ospeed})"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // Stop the writer while the worker is still draining (a blocked
        // write_all would otherwise never observe `stop`), then let the
        // worker stop via the closed command channel.
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        drop(to_worker);
    }

    #[test]
    fn apply_in_place_without_open_port_returns_err_not_panic() {
        // SerialWorker with `port: None` (e.g. after a failed reopen). The
        // old `expect("port open outside reopen")` would panic here; the
        // graceful path returns Err so the caller's reopen fallback runs.
        let (to_main, _to_main_rx) = mpsc::channel::<FromWorker>();
        let (_to_worker_tx, from_main) = mpsc::channel::<ToWorker>();
        let mut worker = SerialWorker {
            port: None,
            cfg: pty_cfg("unused".into()),
            to_main,
            from_main,
        };
        assert!(worker.apply_in_place().is_err());
        // Also via the config path: apply_config must not panic with no port
        // (its reopen fallback fails and reports Disconnected on a dead
        // sender, which is fine).
        worker.apply_config(9600, 8, NcomParity::None, NcomStopBits::One, NcomFlow::None);
    }

    #[test]
    fn set_pin_reapplies_setter_after_successful_reopen() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        // The in-place attempt is skipped (no open handle), so the FIRST
        // call is the re-apply against the freshly reopened handle: it
        // succeeds there.
        fn succeed_first_call(
            _: &mut Box<dyn serialport::SerialPort>,
            _: bool,
        ) -> serialport::Result<()> {
            if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err(serialport::Error::new(
                    serialport::ErrorKind::Io(std::io::ErrorKind::Other),
                    "forced failure",
                ))
            }
        }
        let (_master, slave_path) = pty_pair();
        let (to_main, from_main_rx) = mpsc::channel::<FromWorker>();
        let (_to_worker_tx, from_main) = mpsc::channel::<ToWorker>();
        let mut worker = SerialWorker {
            port: None,
            cfg: pty_cfg(slave_path),
            to_main,
            from_main,
        };
        assert!(
            !worker.set_pin(succeed_first_call, true),
            "worker must continue"
        );
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "setter ran once, against the reopened handle"
        );
        assert!(worker.port.is_some(), "reopened handle stays installed");
        assert!(
            from_main_rx.try_recv().is_err(),
            "no Disconnected on the re-apply success path"
        );
    }

    #[test]
    fn set_pin_reports_persistent_failure_after_reopen() {
        fn always_fail(_: &mut Box<dyn serialport::SerialPort>, _: bool) -> serialport::Result<()> {
            Err(serialport::Error::new(
                serialport::ErrorKind::Io(std::io::ErrorKind::Other),
                "forced failure",
            ))
        }
        let (_master, slave_path) = pty_pair();
        let (to_main, from_main_rx) = mpsc::channel::<FromWorker>();
        let (_to_worker_tx, from_main) = mpsc::channel::<ToWorker>();
        let mut worker = SerialWorker {
            port: None,
            cfg: pty_cfg(slave_path),
            to_main,
            from_main,
        };
        // Reopen succeeds (the pty is openable), but the setter STILL fails:
        // the worker must stop and report it — `:dtr 0` must not silently
        // leave the pin at whatever level the fresh handle came up with.
        assert!(worker.set_pin(always_fail, true), "worker must stop");
        match from_main_rx.try_recv().unwrap() {
            FromWorker::Disconnected(msg) => assert!(msg.contains("forced failure"), "{msg}"),
            other => panic!("unexpected message: {other:?}"),
        }
    }
}
