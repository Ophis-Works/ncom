//! CLI parsing (clap), app bootstrap, terminal setup/teardown, and the event
//! loop: poll terminal input each iteration (`POLL_TIMEOUT`), then drain the
//! serial worker channel.

mod app;
mod command;
mod config;
mod input;
mod logger;
mod serial;
mod ui;

use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use crossterm::event::{self, Event, KeyEventKind};

use app::App;
use serial::{NcomFlow, NcomParity, NcomStopBits, SerialConfig};
/// How long each event-loop iteration waits for terminal input. Serial RX is
/// drained every iteration; this also bounds the 100 ms line idle-flush delay
/// (poll timeout + channel drain).
const POLL_TIMEOUT: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliParity {
    None,
    Even,
    Odd,
    Mark,
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliStopBits {
    #[value(name = "1")]
    One,
    #[value(name = "1.5")]
    OneAndHalf,
    #[value(name = "2")]
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliFlow {
    None,
    Xonz,
    Rtscts,
}

#[derive(Debug, Parser)]
#[command(name = "ncom", about = "Modal (vi-like) serial terminal")]
struct Args {
    /// Serial device to connect to at startup (e.g. /dev/ttyUSB0)
    #[arg(short, long)]
    port: Option<String>,
    #[arg(short, long, default_value_t = 115_200)]
    baud: u32,
    #[arg(short, long, default_value_t = 8)]
    databits: u8,
    #[arg(long, value_enum, default_value_t = CliParity::None)]
    parity: CliParity,
    #[arg(long, value_enum, default_value_t = CliStopBits::One)]
    stopbits: CliStopBits,
    #[arg(short = 'f', long, value_enum, default_value_t = CliFlow::None)]
    flow: CliFlow,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::load();
    let mut app = App::new(&config);

    // Starts connected if --port given; otherwise the user
    // runs `:ports` + `:connect`. Routed through command::connect so
    // a FAILED startup still records `port_cfg`: `:r` then retries the same
    // config instead of reporting "r: never connected".
    if let Some(path) = args.port {
        let cfg = SerialConfig {
            path,
            baud: args.baud,
            data_bits: args.databits,
            parity: parity(args.parity),
            stop_bits: stop_bits(args.stopbits),
            flow: flow(args.flow),
        };
        command::connect(&mut app, cfg);
    }

    run(app)
}

fn parity(p: CliParity) -> NcomParity {
    match p {
        CliParity::None => NcomParity::None,
        CliParity::Even => NcomParity::Even,
        CliParity::Odd => NcomParity::Odd,
        CliParity::Mark => NcomParity::Mark,
        CliParity::Space => NcomParity::Space,
    }
}

fn stop_bits(s: CliStopBits) -> NcomStopBits {
    match s {
        CliStopBits::One => NcomStopBits::One,
        CliStopBits::OneAndHalf => NcomStopBits::OneAndHalf,
        CliStopBits::Two => NcomStopBits::Two,
    }
}

fn flow(f: CliFlow) -> NcomFlow {
    match f {
        CliFlow::None => NcomFlow::None,
        CliFlow::Xonz => NcomFlow::XonXoff,
        CliFlow::Rtscts => NcomFlow::RtsCts,
    }
}

fn run(app: App) -> anyhow::Result<()> {
    install_panic_hook();
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, app);
    ratatui::restore();
    result
}

/// Restore the terminal before a panic unwinds (terminal setup/teardown).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous(info);
    }));
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, mut app: App) -> anyhow::Result<()> {
    loop {
        // 1. Drain serial events: RX -> scrollback (always, even when the view
        //    is paused), disconnects -> port DOWN.
        app.drain();

        // 1b. Advance an in-progress `:sendfile` by one chunk so the file read
        //     never sits on the UI critical path (see App::pump_sendfile).
        if let Some(tx) = app.tx.clone() {
            app.pump_sendfile(&tx);
        }

        // 2. Handle terminal input within the poll window.
        if event::poll(POLL_TIMEOUT)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Status messages are transient (vim-like): the keystroke
                    // that produced the current one (e.g. the Enter that ran
                    // the command) already rendered it, so the NEXT keypress
                    // clears it. Clear BEFORE routing so this key is handled
                    // against a clean status line.
                    app.clear_status();
                    // NORMAL motions/search work with no port open; the TX
                    // channel is only needed by INSERT/COMMAND send paths.
                    let tx = app.tx.clone();
                    if input::handle_key(&mut app, key, tx.as_ref()) == input::KeyResult::Quit {
                        break;
                    }
                }
                Event::Resize(_, _) => {} // next draw() repaints
                _ => {}
            }
        }

        // 3. Auto-expire a stale status message (the ~25 ms poll timeout
        //    bounds how often this runs; see App::tick_status), then render
        //    (follow-tail pause freezes the view, not ingestion).
        app.tick_status(Instant::now());
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        if app.quit {
            break;
        }
    }
    // Final drain before exiting (covers both quit paths — `KeyResult::Quit`
    // and `app.quit`): RX queued since the last iteration's drain still
    // reaches the session log; `drain` takes/restores `self.rx` itself.
    app.drain();
    Ok(())
}
