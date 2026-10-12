# Architecture

ncom is a modal, vi-like serial terminal TUI. A single-threaded event loop
owns all application state; one background worker thread owns the serial
port, and the two communicate over `std::sync::mpsc` channels. There is no
async runtime and no locking; messages cross the boundary.

## Module layout

All modules live in `src/`, wired up in `main.rs`:

- `main.rs` — clap CLI parsing, terminal setup/teardown, and the event loop:
  poll crossterm input each iteration, then drain the serial worker channel.
- `app.rs` — the application state machine: mode (NORMAL / INSERT / COMMAND),
  scrollback ring buffer of raw lines (`Vec<u8>` + arrival timestamp + flags),
  view offset, search and highlight state. RX enters via `App::feed_rx`,
  which runs the line splitter (`\n` or a 100 ms idle flush).
- `serial.rs` — `SerialWorker`: owns the `serialport::SerialPort`, runs the
  reader thread, and serves main-thread commands (connect, reconfigure,
  write TX). A short port read timeout keeps the thread responsive to
  commands and USB unplug; non-timeout errors are treated as disconnects.
- `input.rs` — key handling per mode, plus the `/` search prompt. Control and
  alt-modified character keys are ignored outside a small whitelist, so
  terminal control sequences are not interpreted as plain keypresses.
- `command.rs` — the ex-style `:` command line: parser and execution
  (`connect`, live port reconfiguration, hex view, highlight, logging,
  send/sendhex, help). Case-sensitive lowercase names, verbatim payloads.
- `ui.rs` — ratatui rendering: top bar (view flags, connection info, mode
  chip), scrollback (with search/highlight/hex views), search prompt,
  line-input bar, and bottom command row with feedback.
- `config.rs` — loads and validates the XDG config file; defines the
  `Theme` struct (24-bit truecolour mapped to ratatui `Color::Rgb`).
- `logger.rs` — `:log` session writer, appending raw RX+TX bytes to a file.
  The single-threaded main loop keeps file order equal to stream order.

## Data flow

1. The reader thread in `serial.rs` blocks on `read()` with a short timeout
   and batches of bytes into `RxChunk` messages on an mpsc channel.
2. The main loop drains the channel each iteration; `App::feed_rx` appends
   bytes to the partial line, flushing on `\n` or after the 100 ms idle
   timeout, and pushes completed lines into the scrollback ring buffer.
3. TX takes the opposite path: commands from input/command handling call into
   `App`, which sends a write message to the worker and records the bytes
   for the logger only if the send succeeded.
4. Rendering is immediate-mode each iteration via ratatui on the crossterm
   backend, decoding raw scrollback lines according to the current view
   flags (timestamps, filter, hex, search, highlight).

## Design decisions

- **One worker thread, message passing.** The port is owned exclusively by
  `SerialWorker`; the UI thread never blocks on I/O, and there is no shared
  mutable state and therefore no mutexes.
- **Raw bytes in, decode at render.** Scrollback stores raw lines rather
  than formatted text, so view options (hex, timestamps, filters) can change
  without re-capturing.
- **Disconnects are fatal to the connection, not the app.** Any
  non-timeout read error drops the connection and reports it in the status
  row; the UI keeps running for reconnect or inspection.
- **Minimal dependencies.** Default features are trimmed on ratatui,
  crossterm, serialport (no `libudev`) and chrono; the release profile
  strips, LTOs and optimises for size, and the README documents a musl
  static build.
