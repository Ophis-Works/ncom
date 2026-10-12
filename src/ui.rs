//! ratatui rendering: top bar, scrollback, search prompt, command line.
//!
//! Layout: 1-row top bar (view flags, connection info, mode chip); scrollback
//! (fill); optional 1-row `/` search prompt and line-mode input bar; 1-row
//! bottom command row — the `:` line in COMMAND mode, otherwise command
//! feedback (port DOWN, last error, warnings, status messages). Commands and
//! their feedback only; view flags live on the top bar.
//!
//! Theming: the top bar uses the exact theme `topbar` colour; all body areas
//! are painted `theme.body` (a shade darker than the bar by default) before
//! widgets render.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RtLine, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, InsertStyle, Line, Mode};
use crate::config::Theme;
use crate::serial::{NcomFlow, NcomParity, NcomStopBits, decode_lossy_caret};

/// Render one frame. Takes `&mut App` only to record the scrollback viewport
/// height (`viewport_rows`) for the NORMAL-mode page motions.
pub fn render(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    // Top bar keeps the exact theme `topbar` colour; the body areas are
    // repainted
    // with the darkened variant below (body a shade darker than the bar).
    let area = frame.area();
    let frame_style = Style::new().fg(theme.foreground).bg(theme.topbar);
    frame.buffer_mut().set_style(area, frame_style);

    let base = Style::new().fg(theme.foreground).bg(theme.topbar);
    // Body base style: same foreground, the configured body colour (a shade
    // darker than the bar by default — see the `body` theme key).
    let body_bg = theme.body;
    let base_body = Style::new().fg(theme.foreground).bg(body_bg);
    // The `/` search prompt row sits between the scrollback and the bottom
    // row; the line-mode input bar (`:line on`, INSERT only) sits just above
    // the bottom row too. The COMMAND-mode `:` line lives on the bottom row
    // itself (bottom row = command row).
    let show_prompt = app.search_prompt.is_some();
    let show_input_bar = app.insert_style == InsertStyle::Line && app.mode == Mode::Insert;
    let mut chunks = vec![
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ];
    if show_prompt {
        chunks.insert(2, Constraint::Length(1));
    }
    if show_input_bar {
        chunks.insert(chunks.len() - 1, Constraint::Length(1));
    }
    let areas = Layout::vertical(chunks).split(frame.area());
    let (top, body, tail) = (areas[0], areas[1], areas[areas.len() - 1]);
    app.viewport_rows = body.height as usize;

    // Paint the non-top-bar rows with the darker body background up front, so
    // rows without spans (blank scrollback, empty prompt/status) are dark too.
    frame.buffer_mut().set_style(body, base_body);
    if show_prompt {
        frame.buffer_mut().set_style(areas[2], base_body);
    }
    if show_input_bar {
        frame
            .buffer_mut()
            .set_style(areas[areas.len() - 2], base_body);
    }
    frame.buffer_mut().set_style(tail, base_body);

    render_top_bar(frame, top, app, &theme, base);
    render_body(frame, body, app, &theme, base_body);
    if show_prompt {
        render_prompt_line(frame, areas[2], app, &theme, base_body);
    }
    if show_input_bar {
        let bar = areas[areas.len() - 2];
        render_input_bar(frame, bar, app, &theme, base_body);
    }
    // Bottom row = command row: the `:` command line in COMMAND mode, command
    // feedback (DOWN / error / status / warnings / paused) otherwise.
    if app.mode == Mode::Command {
        render_command_line(frame, tail, app, &theme, base_body);
    } else {
        render_status_line(frame, tail, app, &theme, base_body);
    }

    // The `:help` popup is a modal overlay: rendered LAST so it draws over
    // everything, and `Clear` wipes the cells underneath (borders/text behind
    // it do not bleed through). While it is open, input.rs `handle_key`
    // captures every key before mode dispatch — no TX, no mode changes.
    if app.help_visible {
        render_help_popup(frame, area, app.help_page);
    }
}

// ---------------------------------------------------------------------------
// `:help` popup (centred, light grey, mode-independent modal)
// ---------------------------------------------------------------------------

/// Fixed popup palette, deliberately NOT theme-derived: the light-grey card on
/// a dark terminal must stay readable under every configured theme, so the
/// background is a fixed light grey (#d3d3d3, the classic HTML
/// `lightgray`), text/borders are fixed dark greys for contrast.
const HELP_BG: Color = Color::Rgb(0xd3, 0xd3, 0xd3);
const HELP_FG: Color = Color::Rgb(0x30, 0x30, 0x30);
const HELP_BORDER: Color = Color::Rgb(0x50, 0x50, 0x50);

/// Width cap for the `:help` popup: 64 leaves comfortable headroom over the
/// longest help entry (47 chars) plus the 2 border columns, so the 42-char
/// title and every description fit without wrapping; the popup is
/// additionally clamped down on narrow terminals by `render_help_popup`.
pub(crate) const HELP_POPUP_MAX_WIDTH: u16 = 64;

/// Size policy (roughly square, always fits the terminal):
/// - width: `min(area.width - 4, HELP_POPUP_MAX_WIDTH)` — the cap fits the
///   longest help entry plus the 2 border columns (see
///   [`HELP_POPUP_MAX_WIDTH`]); the `-4` leaves a visible margin. Clamped to
///   at least 10 and at most `area.width` for tiny terminals.
/// - height: SHARED by both pages — `max over pages p of
///   help_page_lines(p).count()` + 2 (one border row per edge), so
///   switching pages does not resize the popup; the taller page is page 1
///   (27 content lines vs page 2's 22). Clamped to `area.height`.
///
/// Layout decision: a single column at the fixed
/// 64-column width is kept, split across TWO PAGES instead of one long
/// popup so the full content fits common terminal heights. A two-column
/// split would need the longest
/// `:` command line (43 chars) to fit half the interior (31 columns), which
/// means rewriting the command descriptions; a wider popup would not help
/// height, which is the binding constraint. Correctness on small terminals
/// is prioritised: the height simply clamps to `area.height`, so the popup
/// never overflows the
/// frame — on terminals shorter than the shared page height the tail lines
/// are clipped, which trades completeness on very short terminals for a
/// guaranteed, non-panicking, centred layout.
///
/// Page indicator + navigation hint: the top border title is
/// ` help [1/2]  j: next  k: prev  esc: close ` (1-based page, hint on BOTH
/// pages so users on page 1 also see how to navigate/close; the bottom border
/// is plain). The full title is 42 chars, fitting the 62-column interior at
/// the 64-column width cap. On narrow terminals where the popup clamps
/// smaller, ratatui clips the overflowing title at the right border — no
/// panic, and the leading ` help ` prefix stays visible; the hint is simply
/// truncated (graceful degradation, simplest correct handling).
///
/// Centering: ratatui 0.30 `Layout::vertical/horizontal([Constraint::Length(n)])
/// .flex(Flex::Center)` — `Flex::Center` centres the single Length-constrained
/// chunk inside the given area (docs.rs/ratatui/0.30.2 — Layout::flex,
/// ratatui-core `layout::Flex`, verified in the vendored crate source:
/// ratatui-core-0.1.2/src/layout/flex.rs, layout.rs `pub const fn flex`).
fn render_help_popup(frame: &mut Frame, area: Rect, page: usize) {
    let width = (area.width.saturating_sub(4))
        .clamp(10, HELP_POPUP_MAX_WIDTH)
        .min(area.width);
    // One box for both pages: height derives from the TALLER page so paging
    // j/k never resizes the popup (page 1 is the taller one).
    let content = (0..crate::app::HELP_PAGE_COUNT)
        .map(|p| crate::app::help_page_lines(p).count())
        .max()
        .unwrap_or(0) as u16;
    let height = (content + 2).min(area.height);
    let [popup] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(popup);
    frame.render_widget(Clear, popup);
    let style = Style::new().fg(HELP_FG).bg(HELP_BG);
    let title_style = Style::new().fg(HELP_BORDER).bg(HELP_BG);
    let block = Block::bordered()
        .title(format!(
            " help [{}/{}]  j: next  k: prev  esc: close ",
            page + 1,
            crate::app::HELP_PAGE_COUNT
        ))
        .border_style(title_style)
        .style(style);
    let lines: Vec<RtLine> = crate::app::help_page_lines(page)
        .map(|l| RtLine::from(help_line_spans(l, style)))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Left),
        popup,
    );
}

/// Split one help line into styled spans: the leading name (up to the first
/// run of 2+ spaces separating it from the description) renders BOLD in the
/// popup style, the description in the plain popup style. Section headers
/// (lines ending in `:`, e.g. ` commands:`) render entirely BOLD. Lines with
/// no separator render plain — the serial-mode lead-in prose by design, and
/// the serial-mode continuation lines that ARE indented to the description
/// column but carry no name before their indent run (an empty name is not a
/// name separator) — and so do the empty [`crate::app::HELP_SECTION_SPACERS`]
/// lines: the 2+-space scan finds nothing in an empty line, so it falls
/// through to one plain empty span.
fn help_line_spans(l: &'static str, style: Style) -> Vec<Span<'static>> {
    if l.ends_with(':') {
        return vec![Span::styled(l, style.add_modifier(Modifier::BOLD))];
    }
    let sep = l
        .as_bytes()
        .windows(2)
        .position(|w| w == b"  ")
        .map(|i| i + 2);
    match sep {
        // A name separator needs an actual name before the run; a run of
        // spaces with nothing before it is a continuation-line indent.
        Some(end) if !l[..end].trim_end().is_empty() => {
            let (name, rest) = l.split_at(end);
            // Trim trailing spaces off the name span; keep the run itself in
            // the description span so column alignment is unchanged.
            let name = name.trim_end();
            let mut spans = vec![
                Span::styled(name, style.add_modifier(Modifier::BOLD)),
                Span::styled(rest, style),
            ];
            if name.len() != end {
                spans.insert(1, Span::styled(&l[name.len()..end], style));
            }
            spans
        }
        // No separator at all (prose, empty spacers), or a separator run with
        // no name before it (a continuation-line indent): render the whole
        // line plain.
        _ => vec![Span::styled(l, style)],
    }
}

// ---------------------------------------------------------------------------
// Top bar
// ---------------------------------------------------------------------------

fn render_top_bar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    let style = base;
    // View flags live on the top bar (moved off the bottom status line, which
    // is now command-only). Style choice: ONLY active flags are shown —
    // `ncom  line hex ts` when all on, bare `ncom` when all off — rendered
    // dimmed in the theme foreground so they read as secondary information
    // next to the app name without stealing focus from the connection info;
    // theme-consistent because it reuses the theme's own fg/top-bar colours.
    let mut flags: Vec<&'static str> = Vec::new();
    if app.insert_style == InsertStyle::Line {
        flags.push("line");
    }
    if app.hex_on {
        flags.push("hex");
    }
    if app.ts_on {
        flags.push("ts");
    }
    if app.log_on {
        flags.push("log");
    }
    if app.hl_pattern.is_some() {
        flags.push("hl");
    }
    if app.filter_pattern.is_some() {
        flags.push("filter");
    }
    let mut left_spans = vec![Span::styled("ncom", style)];
    if !flags.is_empty() {
        left_spans.push(Span::styled(
            format!("  {}", flags.join(" ")),
            style.add_modifier(Modifier::DIM),
        ));
    }
    let left = Paragraph::new(RtLine::from(left_spans)).alignment(Alignment::Left);
    let center = Paragraph::new(RtLine::from(Span::styled(connection_info(app), style)))
        .alignment(Alignment::Center);
    let mode_style = base
        .fg(mode_color(theme, app.mode))
        .add_modifier(Modifier::BOLD);
    let right = Paragraph::new(RtLine::from(Span::styled(mode_label(app.mode), mode_style)))
        .alignment(Alignment::Right);
    frame.render_widget(left, area);
    frame.render_widget(center, area);
    frame.render_widget(right, area);
}

/// Top-right mode chip labels. Display-only renames over the internal
/// `Mode` enum (which keeps its vi identifiers): NORMAL shows as
/// "COMMAND MODE" (the vi-like motion/search mode where commands are typed)
/// and INSERT as "SERIAL MODE" (keystrokes go to the serial port). The `:`
/// command line itself stays "CMD": it is deliberately short for narrow
/// terminals and distinct from the chip's "COMMAND MODE", which would
/// otherwise collide in meaning (the chip labels the keymap mode, CMD labels
/// the ex command line being edited on the bottom row).
fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "COMMAND MODE",
        Mode::Insert => "SERIAL MODE",
        Mode::Command => "CMD",
    }
}

fn mode_color(theme: &Theme, mode: Mode) -> ratatui::style::Color {
    match mode {
        Mode::Normal => theme.normal,
        Mode::Insert => theme.insert,
        Mode::Command => theme.cmd,
    }
}

/// `port baud/D-P-S-flow` when connected, `disconnected` otherwise: the
/// top-bar centre shows `disconnected` when the port is down — the device path
/// stays visible in the DOWN status flag / `:r` context, and the last settings
/// are still in `App::port_cfg` for the reconnect).
pub fn connection_info(app: &App) -> String {
    match (&app.port_cfg, app.connected) {
        (Some(cfg), true) => format!(
            "{} {}/{}{}{}-{}",
            cfg.path,
            cfg.baud,
            cfg.data_bits,
            parity_letter(cfg.parity),
            stop_bits_letter(cfg.stop_bits),
            flow_name(cfg.flow),
        ),
        _ => "disconnected".to_string(),
    }
}

fn parity_letter(p: NcomParity) -> &'static str {
    match p {
        NcomParity::None => "N",
        NcomParity::Even => "E",
        NcomParity::Odd => "O",
        NcomParity::Mark => "M",
        NcomParity::Space => "S",
    }
}

fn stop_bits_letter(s: NcomStopBits) -> &'static str {
    match s {
        NcomStopBits::One => "1",
        NcomStopBits::OneAndHalf => "1.5",
        NcomStopBits::Two => "2",
    }
}

fn flow_name(f: NcomFlow) -> &'static str {
    match f {
        NcomFlow::None => "none",
        NcomFlow::XonXoff => "xonz",
        NcomFlow::RtsCts => "rtscts",
    }
}

// ---------------------------------------------------------------------------
// Scrollback body
// ---------------------------------------------------------------------------

fn render_body(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    // View-mode dispatch: same buffer, renderer swapped by `hex`.
    if app.hex_on {
        hex_body(frame, area, app, theme, base);
    } else {
        text_body(frame, area, app, theme, base);
    }
}

// ---------------------------------------------------------------------------
// Connection splash (`ncom` block-art wordmark)
// ---------------------------------------------------------------------------

/// The `ncom` wordmark as 5 text rows of Unicode block elements. Mirrors
/// docs/assets/logo.svg: letters on a 5-cell-tall grid — n (cols 3-7), c
/// (cols 9-13), o (cols 15-19), m (cols 21-27); every letter cell is a full
/// block, everything else a space, so each row is 25 cells wide. Kept as a
/// helper so tests can assert on the exact art.
fn splash_lines() -> [&'static str; 5] {
    [
        "█████ █████ █████ ███████",
        "█   █ █     █   █ █  █  █",
        "█   █ █     █   █ █  █  █",
        "█   █ █     █   █ █  █  █",
        "█   █ █████ █████ █  █  █",
    ]
}

/// Render the splash: the block-art wordmark centred in the body area, shown
/// only when the app has never seen serial traffic (`scrollback` empty and
/// not `connected`) — the condition stops holding the moment RX arrives or
/// the port comes up, so it disappears naturally. RENDER-ONLY: nothing is
/// pushed to the scrollback, so `:w`, logs and recordings are unaffected.
/// The letters use the theme's `normal` lavender (not bold) on the body base
/// background; the hex view never shows it (`render_body` dispatches before
/// this is reachable from `text_body` only).
fn render_splash(frame: &mut Frame, area: Rect, theme: &Theme, base: Style) {
    let lines = splash_lines();
    let art_width = lines[0].chars().count() as u16; // 25
    let art_height = lines.len() as u16;
    let fg = base.fg(theme.normal); // no BOLD — content, not an alert
    let top = area.height.saturating_sub(art_height) / 2;
    let mut rows: Vec<RtLine> = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        if y >= top && y < top + art_height {
            let art = lines[(y - top) as usize];
            let indent = (area.width.saturating_sub(art_width)) / 2;
            rows.push(RtLine::from(Span::styled(
                format!("{}{}", " ".repeat(indent as usize), art),
                fg,
            )));
        } else {
            rows.push(RtLine::from(Span::styled("", fg)));
        }
    }
    frame.render_widget(Paragraph::new(rows).style(base), area);
}

fn text_body(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    // Fresh, never-connected app: show the wordmark splash instead of an
    // empty screen (see [`render_splash`]).
    if app.scrollback.is_empty() && !app.connected {
        render_splash(frame, area, theme, base);
        return;
    }
    let visible = visible_lines(app, area.height as usize);
    let dim = base
        .fg(ratatui::style::Color::Rgb(0x9a, 0x8f, 0xb8))
        .add_modifier(Modifier::DIM);
    // Match highlight styles: `:hl` (whole-line) and `/` search matches have
    // DISTINCT theme keys now — `theme.hl` for the whole-line `:hl` highlight,
    // `theme.insert` for search matches (the defaults differ: comment purple
    // vs operator orange); customizing `hl` moves only the whole-line
    // highlight. Themed accents + BOLD keep the text readable on any theme.
    let hl_line = base.fg(theme.hl).add_modifier(Modifier::BOLD);
    let hl_search = base.fg(theme.insert).add_modifier(Modifier::BOLD);
    // Local echo (`:echo on`) lines: whole line (plus the render-only `> `
    // prefix) in `theme.tx`, its own theme key. No BOLD — TX lines are
    // content, not an alert.
    let tx_style = base.fg(theme.tx);
    let mut lines = Vec::with_capacity(area.height as usize);
    for line in visible {
        let mut spans = Vec::new();
        if app.ts_on {
            spans.push(Span::styled(
                format!("{} ", crate::app::format_ts(line.ts)),
                dim,
            ));
        }
        let text = decode_lossy_caret(&line.bytes);
        // Highlight precedence (documented choice): `:hl` highlights the
        // ENTIRE line when the line contains the pattern (whole-line style:
        // themed `hl` accent + BOLD — its own theme key, distinct from the
        // search accent) — per-match spans proved hard to spot in dense
        // serial output. On an
        // hl-matching line the whole-line hl style therefore WINS and `/`
        // search spans are suppressed there (they would be invisible inside
        // the all-accent line anyway); on NON-hl-matching lines search match
        // spans still render as before. The hl line check is one `find_ci`
        // pass per line — the same single-scan cost as filtering/searching,
        // no extra per-char work.
        //
        // TX-echo lines outrank BOTH: a `LineFlags::tx` line renders entirely
        // in `theme.tx` (prefix included) and skips the hl/search branches
        // (documented precedence — the TX accent marks provenance, which a
        // search/hl restyle would hide). The `> ` prefix is RENDER-ONLY:
        // filter/hl/search matching runs on the stored bytes, which do not
        // include it (see `LineFlags::tx`).
        if line.flags.tx {
            spans.push(Span::styled("> ", tx_style));
            spans.push(Span::styled(text, tx_style));
        } else {
            let hl_match = app
                .hl_pattern
                .as_deref()
                .is_some_and(|p| crate::app::find_ci(&text, p).is_some());
            if hl_match {
                spans.push(Span::styled(text, hl_line));
            } else if app.last_search.is_some() && app.last_match.is_some() {
                let pattern = app.last_search.as_deref().unwrap_or_default();
                spans.extend(highlight_spans(&text, pattern, base, hl_search));
            } else {
                spans.push(Span::styled(text, base));
            }
        }
        if line.flags.truncated {
            // Line cut by the 64 KiB partial cap (see MAX_PARTIAL_LINE):
            // dimmed ellipsis marks the truncation.
            spans.push(Span::styled("…", dim));
        }
        lines.push(RtLine::from(spans));
    }
    // Empty rows stay theme-coloured: pad with blank themed lines.
    for _ in lines.len()..area.height as usize {
        lines.push(RtLine::from(Span::styled("", base)));
    }
    frame.render_widget(Paragraph::new(lines).style(base), area);
}

/// Hex-view renderer (`:hex on`): the same scrollback buffer, rendered as a
/// canonical hexdump — one multi-row hexdump block per stored line (see
/// [`hexdump_parts`] for the exact format). `:ts on` prefixes the line's
/// arrival time exactly as in the text view, on the FIRST row of the block
/// only (kept: it is a per-line wall-clock stamp, so stamping every row would
/// repeat one timestamp down the block and break the hex columns' shared
/// alignment). Empty lines render blank (a zero-byte hexdump is nothing).
/// `:filter` applies here too (the filtered line list comes from the decoded
/// text, view-independent); `:hl` highlighting is SKIPPED in the hex view —
/// spans would have to cut into the fixed-width hex columns, and the hexdump
/// is for byte-exact reading, not prose. TX-echo lines (`:echo on`) render
/// like ANY other line here — byte-exact reading is the point of this view,
/// so no `> ` prefix and no `theme.tx` colour (documented choice: hex stays
/// neutral; the text view carries the TX styling).
///
/// Viewport sizing rule: [`visible_lines`] selects LINES, but a stored line
/// (up to 64 KiB) renders MANY rows. The height budget is therefore spent on
/// ROWS, newest-first: the newest line always renders IN FULL and ends at the
/// viewport bottom, whole older lines are dropped while they do not fit, and
/// the oldest included line contributes only its FINAL rows (its leading rows
/// are cut above the viewport). Trimming ourselves is required — a `Paragraph`
/// clips overflow at the BOTTOM, which would hide the tail bytes this view
/// exists to show. Short buffers pad with blank themed lines as in the text
/// view. Rendering API: ratatui 0.30 `Paragraph` + styled `Span`s
/// (docs.rs/ratatui/0.30.2/ratatui/widgets/struct.Paragraph.html).
fn hex_body(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    let dim = base
        .fg(ratatui::style::Color::Rgb(0x9a, 0x8f, 0xb8))
        .add_modifier(Modifier::DIM);
    // Three hex-column tones (each its own theme key, AA on `body` by
    // default): the offset is metadata → dimmest (`hex_offset`), the bytes
    // are the primary data → brightest (`hex_byte`), and the ASCII gutter is
    // a convenience rendering in a different hue family (`hex_ascii`). The
    // `:ts` stamp and the truncation `…` stay `dim` — metadata keeps the
    // existing style regardless of the column it sits next to.
    let offset_style = base.fg(theme.hex_offset);
    let byte_style = base.fg(theme.hex_byte);
    let ascii_style = base.fg(theme.hex_ascii);
    let height = area.height as usize;
    let visible = visible_lines(app, height);
    // Spend the row budget newest-first (see the sizing rule above): rows are
    // generated only while budget remains, whole older lines are skipped once
    // the budget is gone, and the oldest survivor contributes only its FINAL
    // rows (leading rows are never generated) — so a 64 KiB line costs at
    // most `height` generated rows, not all `ceil(len/16)` of them.
    let mut blocks: Vec<Vec<RtLine>> = Vec::with_capacity(height);
    let mut budget = height;
    for line in visible.iter().rev() {
        if budget == 0 {
            break;
        }
        if line.bytes.is_empty() {
            blocks.push(vec![RtLine::from(Span::styled("", base))]);
            budget -= 1;
            continue;
        }
        let total_rows = line.bytes.len().div_ceil(16);
        let (first_row, kept) = if total_rows <= budget {
            (0, total_rows)
        } else {
            // Oldest survivor: keep only its final `budget` rows.
            (total_rows - budget, budget)
        };
        // With `:ts on` the first row carries the 13-char wall-clock stamp;
        // continuation rows carry a BLANK stamp of the same width so the
        // offset/hex/ascii columns stay aligned across the whole block (the
        // stamp hangs outside the hexdump columns, as in the text view).
        let stamp_width = if app.ts_on {
            crate::app::format_ts(line.ts).len() + 1
        } else {
            0
        };
        let mut rows: Vec<RtLine> = Vec::with_capacity(kept);
        for row in first_row..total_rows {
            let mut spans = Vec::with_capacity(3);
            if row == 0 && app.ts_on {
                spans.push(Span::styled(
                    format!("{} ", crate::app::format_ts(line.ts)),
                    dim,
                ));
            } else if stamp_width > 0 {
                spans.push(Span::styled(" ".repeat(stamp_width), dim));
            }
            let (hex, ascii) = hexdump_row(&line.bytes, row);
            // Split the fixed-width hex column into offset span (the `0000  `
            // prefix, 6 chars) and byte span (the rest — including the
            // trailing padding spaces on a partial row, so column alignment
            // is unchanged; the row TEXT is byte-identical to before).
            // Owned splits: ratatui 0.30 `Span` stores text by value, not by
            // borrow (see the `visible` borrow error this avoids).
            spans.push(Span::styled(hex[..6].to_string(), offset_style));
            spans.push(Span::styled(hex[6..].to_string(), byte_style));
            // Truncation marker (text view appends `…` too; documented choice:
            // appended after the ASCII gutter so the hex columns stay aligned;
            // per-LINE flag, so every row of a truncated line carries it).
            if line.flags.truncated {
                spans.push(Span::styled(format!("{ascii}…"), dim));
            } else {
                spans.push(Span::styled(ascii, ascii_style));
            }
            rows.push(RtLine::from(spans));
        }
        blocks.push(rows);
        budget -= kept;
    }
    // Blocks were generated newest-first; display is oldest-first. Rows
    // WITHIN a block stay in ascending-offset order either way.
    blocks.reverse();
    let mut lines: Vec<RtLine> = blocks.into_iter().flatten().collect();
    for _ in lines.len()..height {
        lines.push(RtLine::from(Span::styled("", base)));
    }
    frame.render_widget(Paragraph::new(lines).style(base), area);
}

/// The rows of one canonical hexdump: per row `(hex column, ascii gutter)`.
/// Format decisions:
/// - **Per-line RELATIVE offset column** (`0000`, `0010`, … — 4 lowercase hex
///   digits + two spaces). Still **no absolute offsets**: the scrollback is a
///   ring buffer whose front is evicted at 10 000 lines, so a running file-
///   style absolute offset would go stale or wrong once lines are dropped.
///   The offset is relative to the start of the STORED line — always true,
///   never stale — which is what a per-line hexdump of a stream needs (each
///   stored line is a split boundary, not a byte position in a file).
/// - **16 bytes as two-digit lowercase hex pairs**, single-space separated;
///   a partial last row is padded with three spaces per missing byte so the
///   ASCII gutter stays column-aligned on every row (16 * 3 = 48 chars).
/// - **Two spaces (the ASCII gutter), then the ASCII rendering**: byte
///   `0x20..=0x7e` renders as the character itself, everything else as `.`.
///
/// A line longer than 16 bytes yields `ceil(len/16)` rows; an empty slice
/// yields no rows at all (the renderer draws a blank line instead).
pub fn hexdump_parts(bytes: &[u8]) -> Vec<(String, String)> {
    let mut rows = Vec::with_capacity(bytes.len().div_ceil(16));
    for row in 0..bytes.len().div_ceil(16) {
        rows.push(hexdump_row(bytes, row));
    }
    rows
}

/// Row `row` (0-based) of the canonical hexdump: same format as
/// [`hexdump_parts`], one row at a time — the hex view uses this to generate
/// only the rows the viewport actually shows instead of a whole block.
fn hexdump_row(bytes: &[u8], row: usize) -> (String, String) {
    let offset = row * 16;
    let chunk = &bytes[offset..(offset + 16).min(bytes.len())];
    let mut hex = String::with_capacity(48);
    let mut ascii = String::with_capacity(16);
    for i in 0..16 {
        match chunk.get(i) {
            Some(&b) => {
                hex.push_str(&format!("{b:02x} "));
                ascii.push(if (0x20..=0x7e).contains(&b) {
                    b as char
                } else {
                    '.'
                });
            }
            None => hex.push_str("   "),
        }
    }
    hex.pop(); // no trailing space after the 16th pair
    (format!("{offset:04x}  {hex}"), format!("  {ascii}"))
}

/// Single-string convenience form of [`hexdump_parts`] (rows joined by
/// newlines): `0000  68 65 6c 6c 6f                                  hello`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn hexdump(bytes: &[u8]) -> String {
    hexdump_parts(bytes)
        .into_iter()
        .map(|(h, a)| format!("{h}{a}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The slice of VIEW lines the viewport should show: the last `height` lines
/// when following the tail, otherwise the window ending `view_offset` lines
/// back. The view is the FILTERED line list while a `:filter` is active
/// (`App::filtered_indices` maps view positions back to scrollback lines) and
/// the full scrollback otherwise — motions and `view_offset` use the same
/// view-relative coordinates, so rendering and navigation stay consistent.
/// Follow-tail pause only freezes the anchor: RX keeps appending into the
/// buffer while `App::push_line` grows `view_offset` by every VISIBLE
/// appended line, so a paused viewport stays pinned to the same content.
fn visible_lines(app: &App, height: usize) -> Vec<&Line> {
    if height == 0 {
        return Vec::new();
    }
    let view = app.filtered_indices();
    let end = view
        .len()
        .saturating_sub(if app.follow_tail { 0 } else { app.view_offset });
    let start = end.saturating_sub(height);
    view[start..end]
        .iter()
        .map(|&i| &app.scrollback[i])
        .collect()
}

// ---------------------------------------------------------------------------
// Line-mode input bar (`:line on`, INSERT)
// ---------------------------------------------------------------------------

/// The local input line: `> ` prompt in the insert accent, pending text with
/// the cursor drawn as a reverse block at `line_cursor` (char-wise; pure
/// span math, no terminal cursor API needed, so it also renders headless).
fn render_input_bar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    let cursor_style = base.fg(theme.topbar).bg(theme.insert);
    let chars: Vec<char> = app.line_input.chars().collect();
    let cur = app.line_cursor.min(chars.len());
    let mut spans = vec![Span::styled("> ", base.fg(theme.insert))];
    let before: String = chars[..cur].iter().collect();
    if !before.is_empty() {
        spans.push(Span::styled(before, base));
    }
    if cur < chars.len() {
        spans.push(Span::styled(chars[cur].to_string(), cursor_style));
        let after: String = chars[cur + 1..].iter().collect();
        if !after.is_empty() {
            spans.push(Span::styled(after, base));
        }
    } else {
        // Cursor at end: block on a space keeps the position visible.
        spans.push(Span::styled(" ", cursor_style));
    }
    frame.render_widget(Paragraph::new(RtLine::from(spans)), area);
}

// ---------------------------------------------------------------------------
// Prompt line: COMMAND `:` line or `/` search line
// ---------------------------------------------------------------------------
fn render_prompt_line(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    // Only reached while searching (`render` gates on `search_prompt`); the
    // COMMAND `:` line is the bottom row, drawn by `render_command_line`.
    let Some(pattern) = &app.search_prompt else {
        return;
    };
    let line = RtLine::from(Span::styled(format!("/{pattern}"), base.fg(theme.insert)));
    frame.render_widget(Paragraph::new(line), area);
}

/// Split `text` into spans with every case-insensitive occurrence of
/// `pattern` styled `hl`, the rest `base`.
///
/// Single pass over the line's chars: one `Vec<char>` collect, then one scan
/// checking every match position allocation-free ([`crate::app::chars_match_ci`]
/// semantics — identical match behaviour to `find_ci`). The old shape rebuilt a
/// suffix `String` (plus two `Vec<char>`s inside `find_ci`) at EVERY position:
/// O(n²) allocations; this is O(n) allocations, O(n · p) comparisons (see the
/// complexity note on `find_ci`).
fn highlight_spans(text: &str, pattern: &str, base: Style, hl: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let mut spans = Vec::new();
    if p.is_empty() || p.len() > chars.len() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut plain = String::new();
    let mut i = 0;
    while i + p.len() <= chars.len() {
        if crate::app::chars_match_ci(&chars, &p, i) {
            if !plain.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut plain), base));
            }
            let m: String = chars[i..i + p.len()].iter().collect();
            spans.push(Span::styled(m, hl));
            i += p.len();
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    plain.extend(chars[i.min(chars.len())..].iter());
    if !plain.is_empty() {
        spans.push(Span::styled(plain, base));
    }
    spans
}

// ---------------------------------------------------------------------------
// Status line
// ---------------------------------------------------------------------------

/// Command feedback row (bottom row, non-COMMAND mode). The view flags used
/// to be here — they moved to the top bar (see [`render_top_bar`]). DOWN
/// choice: the red `DOWN` indicator STAYS on this row (it is command feedback
/// like the last error, and the top-bar centre already flips to
/// `disconnected` on the same state, so the connection state is visible at
/// all times without duplicating it on the bar).
fn render_status_line(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    let mut spans = Vec::new();
    if let Some(_cfg) = &app.port_cfg
        && !app.connected
    {
        spans.push(Span::styled("DOWN", base.fg(theme.error)));
    }
    if let Some(err) = &app.last_error {
        let sep = if spans.is_empty() { "" } else { " | " };
        spans.push(Span::styled(
            format!("{sep}{}", truncate(err, 40)),
            base.fg(theme.error),
        ));
    }
    if let Some(msg) = app.status_text() {
        let sep = if spans.is_empty() { "" } else { " | " };
        spans.push(Span::styled(format!("{sep}{}", truncate(msg, 40)), base));
    }
    // Multiple config warnings all matter (each names a key that was
    // ignored): join the first 3 with "; " into ONE status segment. The cap
    // is a wider 100 (not the 40 used for single errors/status messages) so
    // the joined text actually fits 3 short warnings; anything beyond is
    // clipped with the usual ellipsis. Later warnings stay in `app.warnings`
    // (the status line just stops showing them past the 3rd).
    if !app.warnings.is_empty() {
        let joined = app
            .warnings
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let sep = if spans.is_empty() { "" } else { " | " };
        spans.push(Span::styled(
            format!("{sep}{}", truncate(&joined, 100)),
            base.fg(theme.error),
        ));
    }
    if !app.follow_tail {
        let sep = if spans.is_empty() { "" } else { " | " };
        spans.push(Span::styled(format!("{sep}paused"), base));
    }
    frame.render_widget(Paragraph::new(RtLine::from(spans)), area);
}

/// The COMMAND-mode `:` line, rendered on the bottom (command) row.
fn render_command_line(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, base: Style) {
    let text = format!(":{}", app.command_line);
    let line = RtLine::from(Span::styled(text, base.fg(theme.cmd)));
    frame.render_widget(Paragraph::new(line), area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, LineFlags};
    use crate::config::Config;
    use std::time::SystemTime;

    // ------------------------------------------------------------------
    // `:help` popup (centred light-grey modal, two pages)
    // ------------------------------------------------------------------

    /// Render `cols`x`rows` (TestBackend width x height) with `app` and return
    /// the frame buffer, for the help-popup tests below (extract rows with
    /// [`row_of`]).
    fn help_render(a: &mut App, cols: u16, rows: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(cols, rows);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, a)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Whole row `y` of `buf` (a `cols`-wide frame) as a string.
    fn row_of(buf: &ratatui::buffer::Buffer, cols: u16, y: u16) -> String {
        (0..cols)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn help_popup_page1_renders_commands_centered_grey() {
        let mut a = App::new(&Config::default());
        a.push_line(
            b"some serial output".to_vec(),
            SystemTime::now(),
            LineFlags::default(),
        );
        a.help_visible = true; // help_page starts at 0 (page 1)
        let buf = help_render(&mut a, 80, 60);
        let row = |y: u16| row_of(&buf, 80, y);

        // Shared box: the taller page (1: 27 content lines) sizes BOTH pages.
        let height = (0..crate::app::HELP_PAGE_COUNT)
            .map(|p| crate::app::help_page_lines(p).count())
            .max()
            .unwrap() as u16
            + 2; // 29
        let width = (80u16 - 4).clamp(10, crate::ui::HELP_POPUP_MAX_WIDTH); // 64

        // Locate the popup by its top border row/column (no assumption about
        // which side of the odd remaining space Flex::Center rounds into).
        let top = (0..60u16)
            .find(|&y| row(y).contains('┌'))
            .expect("top border row");
        let left = row(top).find('┌').expect("top border col") as u16; // ASCII prefix: byte idx == char idx
        let right = row(top)
            .chars()
            .rev()
            .position(|c| c == '┐')
            .expect("top border right col");
        let right = (row(top).chars().count() - 1 - right) as u16;

        // Card of the documented size, centred: equal margins on left/right
        // (even split) and the vertical margins within one row of each other
        // (odd remainder splits unevenly).
        assert_eq!(right - left + 1, width);
        assert_eq!((80 - width) / 2, left);
        let bottom = top + height - 1;
        assert!(row(bottom).contains('└'));
        let bottom_margin = 60i32 - height as i32 - top as i32;
        assert!(
            (top as i32 - bottom_margin).abs() <= 1,
            "vertical margins uneven: top {top}, bottom {bottom_margin}"
        );

        // Title carries the 1-based page indicator AND the navigation hint
        // (hint on both pages); rows above the popup stay free of it.
        assert!(
            row(top).contains(" help [1/2]  j: next  k: prev  esc: close "),
            "{}",
            row(top)
        );
        assert!(!row(top.saturating_sub(1)).contains(" help ["));

        // First rendered line is the ` commands:` section header, and the
        // whole commands section is present.
        let first_content: String = row(top + 1).chars().skip((left + 1) as usize).collect();
        assert!(first_content.starts_with(" commands:"), "{}", row(top + 1));
        let all: String = ((top + 1)..bottom).map(row).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("quit (q! forces)"),
            "command list present:\n{all}"
        );
        assert!(all.contains("q!"), "command list present");
        // Page 1 only: the keys/serial sections must NOT be here.
        assert!(!all.contains(" command mode keys:"), "page 1 only:\n{all}");
        assert!(!all.contains(" serial mode:"), "page 1 only:\n{all}");

        // Grey background inside the box, not outside it.
        let grey = ratatui::style::Color::Rgb(0xd3, 0xd3, 0xd3);
        assert_eq!(buf[(left + 1, top + 1)].bg, grey);
        assert_ne!(buf[(0, top + 1)].bg, grey);
        // Dark text on the grey card.
        assert_eq!(
            buf[(left + 1, top + 1)].fg,
            ratatui::style::Color::Rgb(0x30, 0x30, 0x30)
        );
    }

    #[test]
    fn help_popup_page2_renders_keys_serial_and_hint() {
        let mut a = App::new(&Config::default());
        a.help_visible = true;
        a.help_page = 1;
        let buf = help_render(&mut a, 80, 60);
        let row = |y: u16| row_of(&buf, 80, y);

        // Same shared box as page 1 (taller page sizes both).
        let height = (0..crate::app::HELP_PAGE_COUNT)
            .map(|p| crate::app::help_page_lines(p).count())
            .max()
            .unwrap() as u16
            + 2; // 29
        let top = (0..60u16)
            .find(|&y| row(y).contains('┌'))
            .expect("top border row");
        let bottom = top + height - 1;

        // Page indicator shows page 2 of 2, with the navigation hint in the
        // title (present on both pages).
        assert!(
            row(top).contains(" help [2/2]  j: next  k: prev  esc: close "),
            "{}",
            row(top)
        );

        let all: String = ((top + 1)..bottom).map(row).collect::<Vec<_>>().join("\n");
        // Two EMPTY spacer lines sit between the keys section and the
        // ` serial mode:` header (HELP_SECTION_SPACERS, chained into
        // help_page_lines so the height math counts them): both interior
        // rows directly above the header render blank.
        let left = row(top)
            .chars()
            .position(|c| c == '┌')
            .expect("left border col");
        let serial_row = ((top + 1)..bottom)
            .find(|&y| row(y).contains(" serial mode:"))
            .expect("serial header row");
        for y in (serial_row - 2)..serial_row {
            let interior: String = row(y)
                .chars()
                .skip(left + 1)
                .take(crate::ui::HELP_POPUP_MAX_WIDTH as usize - 2)
                .collect();
            assert!(
                interior.chars().all(|c| c == ' '),
                "row {y} above ` serial mode:` must be empty: {interior:?}"
            );
        }
        // Both sections of page 2 render with representative entries; the
        // page-1 commands section must NOT be here.
        assert!(all.contains(" command mode keys:"), "keys section:\n{all}");
        assert!(all.contains("half page down / up"), "ctrl-d/u entry");
        assert!(all.contains(" serial mode:"), "serial section");
        assert!(all.contains("back to command mode"), "esc entry");
        assert!(!all.contains(" commands:"), "page 2 only:\n{all}");
        assert!(!all.contains("quit (q! forces)"), "page 2 only:\n{all}");

        // The bottom border is plain on every page (hint lives in the title).
        assert!(
            !row(bottom).contains("esc: close"),
            "no hint on bottom border: {}",
            row(bottom)
        );
        a.help_page = 0;
        let buf1 = help_render(&mut a, 80, 60);
        let row1 = |y: u16| row_of(&buf1, 80, y);
        let bottom1 = (top..60u16).find(|&y| row1(y).contains('└')).unwrap();
        assert!(
            !row1(bottom1).contains("esc: close"),
            "no hint on page 1 bottom border: {}",
            row1(bottom1)
        );
    }

    #[test]
    fn help_popup_names_and_headers_render_bold() {
        let mut a = App::new(&Config::default());
        a.help_visible = true;
        let buf = help_render(&mut a, 80, 60);
        let row = |y: u16| row_of(&buf, 80, y);
        let bold = ratatui::style::Modifier::BOLD;

        let top = (0..60u16)
            .find(|&y| row(y).contains('┌'))
            .expect("top border row");
        let left = row(top).find('┌').expect("top border col") as u16;

        // Cell style helper: modifier at (x, y) inside the popup.
        let mods = |x: u16, y: u16| buf[(x, y)].modifier;

        // Section header ` commands:` is entirely bold (popup interior starts
        // at left + 1; the header line is the first content row).
        let header: Vec<(u16, char)> = (left + 1..left + 1 + crate::ui::HELP_POPUP_MAX_WIDTH - 2)
            .map(|x| (x, buf[(x, top + 1)].symbol().chars().next().unwrap()))
            .collect();
        let header_text: String = header.iter().map(|(_, c)| c).collect();
        let header_len = header_text.trim_end().chars().count() as u16;
        assert!(header_text.starts_with(" commands:"), "{header_text:?}");
        for x in (left + 1)..(left + 1 + header_len) {
            assert!(mods(x, top + 1).contains(bold), "header not bold at x={x}");
        }

        // First command entry `q  ...  quit (q! forces)` on the next row:
        // the name `q` (1 char) is bold, the description is not.
        assert!(mods(left + 1, top + 2).contains(bold), "name `q` not bold");
        // Description starts at the page-1 common name column, 24 chars in
        // (longest name `connect <dev> [<baud>]` is 22 + 2 separator spaces).
        assert!(
            !mods(left + 1 + 24, top + 2).contains(bold),
            "description must not be bold"
        );

        // `filter <pat>|off  show only matching` used to carry a single-space
        // separator, which missed the 2+-space split and rendered plain; its
        // name (16 chars) must now be bold, the description not.
        let f_row = ((top + 1)..60u16)
            .find(|&y| row(y).contains("filter <pat>|off"))
            .expect("filter entry row");
        let f_col = row(f_row)
            .find("filter <pat>|off")
            .map(|b| row(f_row)[..b].chars().count())
            .unwrap() as u16;
        assert!(
            mods(f_col, f_row).contains(bold),
            "`filter <pat>|off` name not bold"
        );
        assert!(
            mods(f_col + 15, f_row).contains(bold),
            "`filter <pat>|off` tail not bold"
        );
        let f_desc_col = row(f_row)
            .find("show only matching")
            .map(|b| row(f_row)[..b].chars().count())
            .unwrap() as u16;
        assert!(
            !mods(f_desc_col, f_row).contains(bold),
            "description of `filter <pat>|off` must not be bold"
        );

        // Page 2: ` command mode keys:` header bold, entry `j / k` name bold,
        // its description not.
        a.help_page = 1;
        let buf2 = help_render(&mut a, 80, 60);
        let row2 = |y: u16| row_of(&buf2, 80, y);
        let top2 = (0..60u16)
            .find(|&y| row2(y).contains('┌'))
            .expect("top border row");
        let left2 = row2(top2)
            .char_indices()
            .find(|(_, c)| *c == '┌')
            .expect("top border col")
            .0 as u16;
        let mods2 = |x: u16, y: u16| buf2[(x, y)].modifier;

        assert!(
            mods2(left2 + 1, top2 + 1).contains(bold),
            "page 2 header not bold"
        );
        // Find the `j / k` entry row inside page 2.
        let jk_row = ((top2 + 1)..60u16)
            .find(|&y| row2(y).contains("j / k"))
            .expect("j / k entry row");
        let jk_col = row2(jk_row)
            .find("j / k")
            .map(|b| row2(jk_row)[..b].chars().count())
            .unwrap() as u16;
        // Name span is `j / k` (5 chars): first and last char bold.
        assert!(mods2(jk_col, jk_row).contains(bold), "`j / k` not bold");
        assert!(
            mods2(jk_col + 4, jk_row).contains(bold),
            "`j / k` tail not bold"
        );
        // Description starts after the run of spaces following the name.
        let after_name = row2(jk_row)[jk_col as usize + 5..]
            .char_indices()
            .find(|(_, c)| *c != ' ')
            .map(|(i, _)| i)
            .expect("description text after `j / k`");
        let desc_col = jk_col + 5 + after_name as u16;
        assert!(
            !mods2(desc_col, jk_row).contains(bold),
            "description of `j / k` must not be bold"
        );

        // Serial-mode prose line (`keys are sent to the port as typed`) has
        // no 2+ space separator: it renders plain (not bold).
        let prose_row = ((top2 + 1)..60u16)
            .find(|&y| row2(y).contains("keys are sent to the port as typed"))
            .expect("serial prose row");
        let prose_col = row2(prose_row)
            .find("keys are sent")
            .map(|b| row2(prose_row)[..b].chars().count())
            .unwrap() as u16;
        assert!(
            !mods2(prose_col, prose_row).contains(bold),
            "prose line must not be bold"
        );
    }

    #[test]
    fn help_popup_clamps_to_tiny_terminals() {
        let mut a = App::new(&Config::default());
        a.help_visible = true;
        let backend = ratatui::backend::TestBackend::new(20, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Width clamps to the terminal (20), height to 10 rows; must not panic
        // and the border must stay inside the frame.
        let row =
            |y: u16| -> String { (0..20).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(row(0).contains(" help "), "{}", row(0));
        // The full title (42 chars) overflows the clamped 20-col popup: it is
        // clipped at the right border — the hint tail must not leak outside.
        assert!(!row(0).contains("esc: close"), "{}", row(0));
        assert!(row(9).trim().starts_with('└'), "{}", row(9));
    }

    #[test]
    fn help_popup_both_pages_render_the_same_box() {
        let mut a = App::new(&Config::default());
        a.help_visible = true;

        // Popup box geometry (left, top, right, bottom) for a page, located
        // by its border corners.
        let mut box_of = |page: usize| -> (u16, u16, u16, u16) {
            a.help_page = page;
            let buf = help_render(&mut a, 80, 60);
            let row = |y: u16| row_of(&buf, 80, y);
            let top = (0..60u16)
                .find(|&y| row(y).contains('┌'))
                .expect("top border row");
            let left = row(top).find('┌').expect("left border col") as u16;
            let right = row(top)
                .chars()
                .rev()
                .position(|c| c == '┐')
                .expect("right border col");
            let right = (row(top).chars().count() - 1 - right) as u16;
            let bottom = (top..60u16)
                .find(|&y| row(y).contains('└'))
                .expect("bottom border row");
            (left, top, right, bottom)
        };

        let p1 = box_of(0);
        let p2 = box_of(1);
        // Identical geometry on both pages: paging (j/k) never resizes the
        // popup.
        assert_eq!(p1, p2, "both pages must render the identical box");
        let expected_w = (80u16 - 4).clamp(10, crate::ui::HELP_POPUP_MAX_WIDTH); // 64
        let expected_h = (0..crate::app::HELP_PAGE_COUNT)
            .map(|p| crate::app::help_page_lines(p).count())
            .max()
            .unwrap() as u16
            + 2; // 29 — sized by the taller page (page 1)
        assert_eq!(p1.2 - p1.0 + 1, expected_w);
        assert_eq!(p1.3 - p1.1 + 1, expected_h);
    }

    #[test]
    fn help_line_spans_bold_every_name_and_header() {
        // Exhaustive audit over ALL help entries on BOTH pages: every section
        // header is one fully-bold span; every entry with a 2+ space
        // separator has its whole leading name bold; the only plain lines are
        // the serial-mode lead-in prose and its continuation lines (flush
        // left, or indented to the description column with no name before
        // the indent run); and spans never mangle the text.
        let style = Style::new().fg(HELP_FG).bg(HELP_BG);
        let bold = Modifier::BOLD;
        let sep_end = |l: &str| -> Option<usize> {
            l.as_bytes()
                .windows(2)
                .position(|w| w == b"  ")
                .map(|i| i + 2)
        };
        let all = crate::app::HELP_LINES
            .iter()
            .copied()
            .chain(crate::app::HELP_KEY_LINES.iter().copied())
            .chain(crate::app::HELP_SERIAL_LINES.iter().copied())
            .chain(crate::app::HELP_SECTION_SPACERS.iter().copied());
        for l in all {
            let spans = help_line_spans(l, style);
            let joined: String = spans.iter().map(|s| &*s.content).collect();
            assert_eq!(joined, l, "spans must not mangle {l:?}");
            if l.ends_with(':') {
                assert_eq!(spans.len(), 1, "header {l:?} must be one bold span");
                assert!(
                    spans[0].style.add_modifier.contains(bold),
                    "header {l:?} not bold"
                );
            } else if l.is_empty() {
                // Spacer lines: exactly one plain EMPTY span (renders as a
                // blank row, no bold anywhere).
                assert_eq!(spans.len(), 1, "spacer line must be one span");
                assert_eq!(&*spans[0].content, "", "spacer span must be empty");
                assert!(
                    !spans[0].style.add_modifier.contains(bold),
                    "spacer line must not be bold"
                );
            } else if let Some(end) = sep_end(l) {
                let name = l[..end].trim_end();
                if name.is_empty() {
                    // Continuation line indented to the description column:
                    // the run has no name before it, so like prose the whole
                    // line renders plain.
                    for s in &spans {
                        assert!(
                            !s.style.add_modifier.contains(bold),
                            "continuation {l:?} must not be bold"
                        );
                    }
                } else {
                    assert!(!name.contains("  "), "split landed early in {l:?}");
                    assert!(
                        spans[0].style.add_modifier.contains(bold),
                        "name of {l:?} not bold"
                    );
                    assert_eq!(&*spans[0].content, name, "name span of {l:?}");
                }
            } else {
                for s in &spans {
                    assert!(
                        !s.style.add_modifier.contains(bold),
                        "prose {l:?} must not be bold"
                    );
                }
            }
        }
        // Exactly the serial-mode lead-in prose and the two `:line on`
        // description continuation lines render plain (no name separator, or
        // an indent run with no name before it); everything else (both
        // headers, every command, every key) bolds its name per the loop
        // above, and the empty spacer lines are excluded (they render as one
        // plain empty span).
        let plain: Vec<&str> = crate::app::HELP_LINES
            .iter()
            .copied()
            .chain(crate::app::HELP_KEY_LINES.iter().copied())
            .chain(crate::app::HELP_SERIAL_LINES.iter().copied())
            .chain(crate::app::HELP_SECTION_SPACERS.iter().copied())
            .filter(|l| {
                !l.ends_with(':')
                    && !l.is_empty()
                    && match sep_end(l) {
                        None => true,
                        Some(end) => l[..end].trim_end().is_empty(),
                    }
            })
            .collect();
        assert_eq!(
            plain,
            [
                "keys are sent to the port as typed",
                "                 allows full line editing with history",
                "                 sending full line when enter is pressed",
            ]
        );
    }

    #[test]
    fn help_lines_fit_popup_interior_width() {
        // Interior width at the 64-col cap (2 border columns); terminals
        // wide enough (>=68 cols) give the popup the full cap. Every rendered
        // line on both pages is checked: each section, its header, the empty
        // HELP_SECTION_SPACERS lines (which pass trivially), and the
        // width-normalised entries (name padded to the page's common column,
        // so the widest line is column + longest description).
        assert_eq!(crate::ui::HELP_POPUP_MAX_WIDTH, 64);
        let interior = crate::ui::HELP_POPUP_MAX_WIDTH - 2; // 62
        for page in 0..crate::app::HELP_PAGE_COUNT {
            for line in crate::app::help_page_lines(page) {
                assert!(
                    line.chars().count() <= interior as usize,
                    "help line too long ({}/{}): {line:?}",
                    line.chars().count(),
                    interior
                );
            }
        }
        // And on a typical 64-column terminal the popup (width formula as in
        // render_help_popup) still leaves room for every line inside it.
        let popup_w = (64u16.saturating_sub(4))
            .clamp(10, crate::ui::HELP_POPUP_MAX_WIDTH)
            .min(64);
        let longest = (0..crate::app::HELP_PAGE_COUNT)
            .flat_map(crate::app::help_page_lines)
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        assert!(
            (longest as u16) <= popup_w - 2,
            "longest help line {longest} exceeds interior of {popup_w}-col popup"
        );
    }

    #[test]
    fn help_descriptions_align_to_one_common_column_per_page() {
        // Width normalisation invariant: on EACH page every entry that
        // carries a 2+ space name separator starts its description at the
        // SAME char column, and that column is exactly the page's longest
        // name + the 2-space separator (computed from the data, not
        // hardcoded). Headers and the plain serial-mode lines (flush-left
        // lead-in prose, and continuation lines indented to the description
        // column with no name before the indent run) have no name column and
        // are skipped, as are the empty HELP_SECTION_SPACERS lines.
        for page in 0..crate::app::HELP_PAGE_COUNT {
            let mut desc_cols: Vec<usize> = Vec::new();
            let mut longest_name = 0usize;
            for line in crate::app::help_page_lines(page) {
                let sep_start = line.as_bytes().windows(2).position(|w| w == b"  ");
                if let Some(start) = sep_start {
                    // Name is everything before the separator run (never
                    // contains "  "; audited in the spans test); the
                    // description starts at the first non-space AFTER the
                    // whole run (the run can be longer than 2 spaces).
                    let name = line[..start].trim_end();
                    if name.is_empty() {
                        // Continuation line: an indent run, not a name
                        // separator — no name column.
                        continue;
                    }
                    let desc_col = line[start..]
                        .char_indices()
                        .find(|(_, c)| *c != ' ')
                        .map(|(i, _)| start + i)
                        .unwrap_or(start);
                    longest_name = longest_name.max(name.chars().count());
                    desc_cols.push(desc_col);
                }
            }
            assert!(!desc_cols.is_empty(), "page {page} has no entries");
            let col = desc_cols[0];
            assert!(
                desc_cols.iter().all(|&c| c == col),
                "page {page}: description columns vary: {desc_cols:?}"
            );
            assert_eq!(
                col,
                longest_name + 2,
                "page {page}: column must be the longest name + 2 spaces"
            );
        }
    }

    #[test]
    fn visible_lines_follow_tail_shows_last() {
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        for i in 0..10u8 {
            a.push_line(vec![b'0' + i], t, LineFlags::default());
        }
        let v = visible_lines(&a, 4);
        let firsts: Vec<u8> = v.iter().map(|l| l.bytes[0]).collect();
        assert_eq!(firsts, vec![b'6', b'7', b'8', b'9']);
    }

    #[test]
    fn visible_lines_respects_offset_when_not_following() {
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        for i in 0..10u8 {
            a.push_line(vec![b'0' + i], t, LineFlags::default());
        }
        a.follow_tail = false;
        a.view_offset = 3;
        let v = visible_lines(&a, 4);
        let firsts: Vec<u8> = v.iter().map(|l| l.bytes[0]).collect();
        // window ends at 10-3=7 (exclusive), so lines 3..7
        assert_eq!(firsts, vec![b'3', b'4', b'5', b'6']);
    }

    #[test]
    fn visible_lines_short_buffer() {
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        a.push_line(b"only".to_vec(), t, LineFlags::default());
        let v = visible_lines(&a, 5);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].bytes, b"only".to_vec());
    }

    #[test]
    fn connection_info_strings() {
        use crate::serial::{NcomFlow, NcomParity, NcomStopBits, SerialConfig};
        let mut a = App::new(&Config::default());
        assert_eq!(connection_info(&a), "disconnected");
        a.port_cfg = Some(SerialConfig {
            path: "/dev/ttyUSB0".into(),
            baud: 115_200,
            data_bits: 8,
            parity: NcomParity::None,
            stop_bits: NcomStopBits::One,
            flow: NcomFlow::None,
        });
        a.connected = true;
        assert_eq!(connection_info(&a), "/dev/ttyUSB0 115200/8N1-none");
        a.connected = false;
        assert_eq!(connection_info(&a), "disconnected");
    }

    #[test]
    fn truncate_works() {
        assert_eq!(truncate("short", 40), "short");
        assert_eq!(truncate(&"a".repeat(50), 10).chars().count(), 10);
    }

    #[test]
    fn highlight_spans_case_insensitive() {
        let base = Style::new();
        let hl = Style::new().bold();
        let spans = highlight_spans("aBc and ABC and x", "abc", base, hl);
        let styled: Vec<(String, bool)> = spans
            .into_iter()
            .map(|s| (s.content.to_string(), s.style != Style::new()))
            .collect();
        assert_eq!(
            styled,
            vec![
                ("aBc".to_string(), true),
                (" and ".to_string(), false),
                ("ABC".to_string(), true),
                (" and x".to_string(), false),
            ]
        );
        // No occurrence: single plain span.
        let spans = highlight_spans("hello", "zz", base, hl);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.to_string(), "hello");
    }

    #[test]
    fn highlight_spans_long_line_match_at_end() {
        // 100k chars with the only match at the very end: the single-pass
        // scanner must find it without O(n²) suffix rebuilds (completes in
        // bounded time; the old per-position find_ci did not).
        let text = format!("{}{}", "x".repeat(100_000 - 3), "AbC");
        let base = Style::new();
        let spans = highlight_spans(&text, "abc", base, base.bold());
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content.to_string(), "AbC");
        assert_ne!(spans[1].style, base, "the match is highlighted");
        assert_eq!(spans[0].content.chars().count(), 100_000 - 3);
    }

    #[test]
    fn truncated_line_renders_dimmed_ellipsis_in_text_view() {
        use crate::serial::MAX_PARTIAL_LINE;
        // Ingestion side: a >cap run without `\n` yields bounded lines that
        // carry the flag (tail inherits it).
        let mut a = App::new(&Config::default());
        let t0 = std::time::Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(crate::serial::FromWorker::Data(vec![
            b'z';
            MAX_PARTIAL_LINE + 3
        ]))
        .unwrap();
        a.connected = true;
        a.drain_worker_at(&rx, t0);
        a.drain_worker_at(&rx, t0 + std::time::Duration::from_millis(150));
        assert_eq!(a.scrollback.len(), 2);
        assert!(a.scrollback[0].flags.truncated);
        assert!(a.scrollback[1].flags.truncated, "tail inherits the flag");

        // Rendering side: short flagged lines keep the `…` on screen.
        let mut a = App::new(&Config::default());
        let t = std::time::SystemTime::now();
        for text in ["head", "tail"] {
            a.push_line(
                text.as_bytes().to_vec(),
                t,
                crate::app::LineFlags {
                    truncated: true,
                    tx: false,
                },
            );
        }
        a.push_line(b"plain".to_vec(), t, crate::app::LineFlags::default());
        let backend = ratatui::backend::TestBackend::new(80, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(row(1).trim_end().ends_with("head…"), "{}", row(1));
        assert!(row(2).trim_end().ends_with("tail…"), "{}", row(2));
        assert_eq!(row(3).trim_end(), "plain", "unflagged line has no marker");
    }

    // ------------------------------------------------------------------
    // Hex view (hexdump formatting + headless render)
    // ------------------------------------------------------------------

    #[test]
    fn hexdump_full_row_format() {
        // 16 printable bytes: relative offset (4 digits + 2 spaces), 16 hex
        // pairs (16*3-1 = 47 chars), two-space ASCII gutter, 16-char ASCII
        // rendering. The offset is per-line RELATIVE (see `hexdump_parts`
        // docs: a ring buffer makes any absolute offset stale/fake).
        let rows = hexdump_parts(b"0123456789abcdef");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].0,
            "0000  30 31 32 33 34 35 36 37 38 39 61 62 63 64 65 66"
        );
        assert_eq!(rows[0].1, "  0123456789abcdef");
        assert!(hexdump(b"x").starts_with("0000  78"));
    }

    #[test]
    fn hexdump_non_printables_are_dots() {
        // Controls, high bytes, DEL: all '.' in the gutter, real hex in bytes.
        let rows = hexdump_parts(&[0x00, 0x07, 0x1b, 0x7f, 0x80, 0xff]);
        assert_eq!(rows[0].1, "  ......");
        assert!(rows[0].0.starts_with("0000  00 07 1b 7f 80 ff"));
        // 0x20 (space) and 0x7e (tilde) are the printable bounds: kept.
        let rows = hexdump_parts(&[0x20, 0x7e]);
        assert_eq!(rows[0].0.trim_end(), "0000  20 7e");
    }

    #[test]
    fn hexdump_partial_last_line_stays_aligned() {
        // 5 bytes: the hex column is padded to the full 47 chars (plus the
        // 6-char offset prefix) so the ASCII gutter starts at the same column
        // as a full row.
        let rows = hexdump_parts(b"hello");
        assert_eq!(rows.len(), 1);
        let full_rows = hexdump_parts(b"The quick brown f");
        assert_eq!(rows[0].0.len(), full_rows[0].0.len());
        assert_eq!(rows[0].1, "  hello");
        assert!(rows[0].0.starts_with("0000  68 65 6c 6c 6f")); // the 5 real pairs
        assert!(rows[0].0.ends_with("   "), "padding uses 3 spaces per slot");
        // Exactly 16 bytes: no padding at all.
        let full_rows = hexdump_parts(&[0xaa; 16]);
        assert_eq!(full_rows[0].1, format!("  {}", ".".repeat(16)));
        assert_eq!(full_rows[0].0.matches("aa").count(), 16);
    }

    #[test]
    fn hexdump_line_longer_than_16_bytes_spans_multiple_rows() {
        // 17 bytes -> 2 rows with relative offsets 0000/0010; the second row
        // carries the 17th byte and stays column-aligned via padding.
        let rows = hexdump_parts(b"0123456789abcdefX");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].0,
            "0000  30 31 32 33 34 35 36 37 38 39 61 62 63 64 65 66"
        );
        // Partial row: the 17th byte, padded to the same 53-char column.
        assert_eq!(rows[1].0.trim_end(), "0010  58");
        assert_eq!(rows[0].0.len(), rows[1].0.len(), "gutter stays aligned");
        assert_eq!(rows[1].1, "  X");
        // 64 bytes -> exactly 4 rows, offsets 0000/0010/0020/0030, every byte
        // present exactly once across the rows.
        let rows = hexdump_parts(&[0x41; 64]);
        assert_eq!(rows.len(), 4);
        for (r, off) in rows.iter().zip(["0000", "0010", "0020", "0030"]) {
            assert!(r.0.starts_with(off), "{}", r.0);
            assert_eq!(r.0.len(), 53, "full row: 6 offset + 47 hex chars");
        }
        assert_eq!(
            rows.iter()
                .map(|(h, _)| h.matches("41").count())
                .sum::<usize>(),
            64,
            "all 64 bytes render across the 4 rows"
        );
        // Empty slice: no rows (the renderer draws a blank line instead).
        assert!(hexdump_parts(&[]).is_empty());
    }

    #[test]
    fn hex_long_line_renders_all_rows_ts_on_first_row_only() {
        use crate::app::{DEFAULT_SCROLLBACK, LineFlags};
        use std::time::{Duration, SystemTime};

        let mut a = App::new(&Config::default());
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_791_824_523_456);
        // One 17-byte line: 2 hexdump rows, ts prefix on the first row only.
        a.push_line(b"0123456789abcdefX".to_vec(), t, LineFlags::default());
        a.hex_on = true;
        a.ts_on = true;

        // chrono's Local reads the process-global TZ; pin it so the fixed
        // instant renders as UTC regardless of the host timezone (and of
        // tests that mutate TZ, e.g. app.rs `ts_formatting`). Guarded by the
        // shared test env lock (std::env is process-global).
        let _guard = crate::config::tests::lock_env_for_test();
        let prev_tz = std::env::var("TZ").ok();
        // SAFETY: single-threaded w.r.t. env mutation (guarded by the shared
        // test env lock); test harness never reads TZ concurrently.
        unsafe { std::env::set_var("TZ", "UTC0") };

        let backend = ratatui::backend::TestBackend::new(90, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Line 0 = body top (after the 1-row top bar). First row: ts, then the
        // relative offset and the full 16-byte hex column + ASCII gutter.
        assert!(
            row(1)
                .starts_with("17:02:03.456 0000  30 31 32 33 34 35 36 37 38 39 61 62 63 64 65 66"),
            "{}",
            row(1)
        );
        // Second row: blank stamp, then offset 0010 and the 17th byte — no
        // timestamp prefix, same column alignment as the first row.
        assert!(row(2)[13..].starts_with("0010  58"), "{}", row(2));
        assert!(!row(2).contains("17:02:03"), "{}", row(2));
        // ASCII gutter alignment: the gutter starts at the same column on
        // every row (13-char ts prefix + 6 offset + 47 hex + 2 gutter = 68).
        assert!(row(1)[68..].starts_with("0123456789abcdef"), "{}", row(1));
        assert_eq!(&row(2)[68..68 + 1], "X", "{}", row(2));
        // Padding row below the block stays blank.
        assert!(row(3).trim().is_empty(), "{}", row(3));
        assert_eq!(DEFAULT_SCROLLBACK, 10_000);

        // Restore the host's TZ before releasing the env lock.
        match prev_tz {
            Some(v) => unsafe { std::env::set_var("TZ", v) },
            None => unsafe { std::env::remove_var("TZ") },
        }
    }

    #[test]
    fn hex_body_renders_only_budget_tail_rows_for_oversized_line() {
        use crate::app::{DEFAULT_SCROLLBACK, LineFlags};
        use std::time::SystemTime;

        let mut a = App::new(&Config::default());
        // One 17 KiB line: 1088 hexdump rows — far more than any viewport.
        // Byte i is (i % 256) so every row's content is predictable.
        let bytes: Vec<u8> = (0..17 * 1024).map(|i| (i % 256) as u8).collect();
        a.push_line(bytes, SystemTime::UNIX_EPOCH, LineFlags::default());
        a.hex_on = true;
        assert_eq!(DEFAULT_SCROLLBACK, 10_000);

        // Viewport height 6: top bar (1) + body (4) + bottom bar (1). The
        // whole 1088-row block must be trimmed to the FINAL 4 body rows
        // (rows 1084..1088, i.e. offsets 0x43c0..0x43f0) — never the leading
        // rows.
        let backend = ratatui::backend::TestBackend::new(90, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let offsets = ["43c0", "43d0", "43e0", "43f0"];
        for (y, off) in offsets.iter().enumerate() {
            let r = row(1 + y as u16);
            assert!(r.starts_with(off), "body row {y}: {}", r);
        }
        // The final row covers bytes 17392..=17407 (= 0xf0..=0xff: 17392 %
        // 256 == 240) — every one non-printable, so the ASCII gutter is 16
        // dots.
        assert!(row(4)[55..].starts_with(&".".repeat(16)), "{}", row(4));
    }

    // ------------------------------------------------------------------
    // Mode chip display labels (COMMAND MODE / SERIAL MODE)
    // ------------------------------------------------------------------

    #[test]
    fn mode_chip_renders_display_labels_even_on_narrow_terminal() {
        // Display labels only: the chip shows COMMAND MODE / SERIAL MODE (the
        // Mode enum keeps its vi identifiers). "COMMAND MODE" is 12 chars, so
        // render at a width where left flags, centred info and the chip
        // overlap — must not panic, and the chip stays right-aligned.
        let mut a = App::new(&Config::default());
        a.hex_on = true;
        a.ts_on = true;
        let backend = ratatui::backend::TestBackend::new(24, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row = |b: &ratatui::buffer::Buffer, y: u16| -> String {
            (0..24).map(|x| b[(x, y)].symbol().to_string()).collect()
        };
        let top = row(&buf, 0);
        assert!(top.ends_with("COMMAND MODE"), "{}", top);
        // Insert mode: same position, SERIAL MODE.
        a.mode = Mode::Insert;
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(row(&buf, 0).ends_with("SERIAL MODE"), "{}", row(&buf, 0));
    }

    #[test]
    fn flags_render_on_top_bar_only_when_active() {
        let mut a = App::new(&Config::default());
        let backend = ratatui::backend::TestBackend::new(60, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let row = |b: &ratatui::buffer::Buffer, y: u16| -> String {
            (0..60).map(|x| b[(x, y)].symbol().to_string()).collect()
        };
        // All flags off: bare `ncom`, and nothing flag-like on the bottom row.
        let buf = terminal.backend().buffer().clone();
        assert!(row(&buf, 0).starts_with("ncom "), "{}", row(&buf, 0));
        assert!(!row(&buf, 0).contains("hex"), "{}", row(&buf, 0));
        assert_eq!(row(&buf, 2).trim(), "");
        // All flags on: `ncom  line hex ts log` on the top bar only.
        a.insert_style = InsertStyle::Line;
        a.hex_on = true;
        a.ts_on = true;
        a.log_on = true;
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(
            row(&buf, 0).starts_with("ncom  line hex ts log "),
            "{}",
            row(&buf, 0)
        );
        assert_eq!(row(&buf, 2).trim(), "");
    }

    #[test]
    fn bottom_row_is_command_line_in_command_mode_feedback_otherwise() {
        let mut a = App::new(&Config::default());
        let backend = ratatui::backend::TestBackend::new(60, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        // COMMAND mode: the bottom row shows the `:` command line.
        a.mode = Mode::Command;
        a.command_line = "hex on".to_string();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row = |b: &ratatui::buffer::Buffer, y: u16| -> String {
            (0..60).map(|x| b[(x, y)].symbol().to_string()).collect()
        };
        assert_eq!(row(&buf, 2).trim_end(), ":hex on");
        // Back to NORMAL: bottom row shows command feedback (status message).
        a.mode = Mode::Normal;
        a.set_status("log: off");
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(row(&buf, 2).trim_end(), "log: off");
        // Port DOWN (configured but not connected): red DOWN on the bottom row.
        use crate::serial::SerialConfig;
        a.port_cfg = Some(SerialConfig {
            path: "/dev/ttyUSB0".into(),
            baud: 115_200,
            data_bits: 8,
            parity: crate::serial::NcomParity::None,
            stop_bits: crate::serial::NcomStopBits::One,
            flow: crate::serial::NcomFlow::None,
        });
        a.status_msg = None;
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(row(&buf, 2).trim_end(), "DOWN");
        // ...while the top-bar centre shows `disconnected` for the same state.
        assert!(row(&buf, 0).contains("disconnected"));
    }

    #[test]
    fn status_line_joins_up_to_three_config_warnings() {
        let mut a = App::new(&Config::default());
        let backend = ratatui::backend::TestBackend::new(120, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let row = |b: &ratatui::buffer::Buffer, y: u16| -> String {
            (0..120).map(|x| b[(x, y)].symbol().to_string()).collect()
        };

        // Two warnings render as one "; "-joined segment, not just the first.
        a.warnings = vec![
            "config: unknown key `theme.a` ignored".into(),
            "config: unreadable (denied); using defaults".into(),
        ];
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(
            row(&buf, 2).contains("theme.a") && row(&buf, 2).contains("unreadable"),
            "both warnings must show: {}",
            row(&buf, 2)
        );
        assert!(
            row(&buf, 2).contains("ignored; config: unreadable"),
            "joined with \"; \": {}",
            row(&buf, 2)
        );

        // Three join; a fourth is dropped from the status line (cap 3).
        a.warnings.push("config: third".into());
        a.warnings.push("config: fourth".into());
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let bottom = row(&buf, 2);
        assert!(bottom.contains("theme.a"), "{}", bottom);
        assert!(bottom.contains("third"), "{}", bottom);
        assert!(!bottom.contains("fourth"), "{}", bottom);
        // The full vec is untouched — only the status line caps at 3.
        assert_eq!(a.warnings.len(), 4);
    }

    // ------------------------------------------------------------------
    // Body darker than top bar (explicit `body` theme colour)
    // ------------------------------------------------------------------

    #[test]
    fn top_bar_keeps_theme_bg_body_is_darker() {
        let mut a = App::new(&Config::default());
        let backend = ratatui::backend::TestBackend::new(40, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = Config::default().theme;
        // Row 0 = top bar: exact theme `topbar` colour.
        assert_eq!(buf[(0, 0)].bg, theme.topbar);
        // Body row and status row: the configured body colour, different
        // from (and darker than) the bar.
        assert_eq!(buf[(0, 1)].bg, theme.body);
        assert_eq!(buf[(0, 4)].bg, theme.body);
        assert_ne!(theme.body, theme.topbar);
    }

    #[test]
    fn hex_body_renders_headless_with_ts() {
        use crate::app::{DEFAULT_SCROLLBACK, LineFlags};
        use std::time::{Duration, SystemTime};

        let mut a = App::new(&Config::default());
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_791_824_523_456);
        a.push_line(b"hello".to_vec(), t, LineFlags::default());
        a.push_line(Vec::new(), t, LineFlags::default());
        a.hex_on = true;
        a.ts_on = true;

        // chrono's Local reads the process-global TZ; pin it so the fixed
        // instant renders as UTC regardless of the host timezone (and of
        // tests that mutate TZ, e.g. app.rs `ts_formatting`). Guarded by the
        // shared test env lock (std::env is process-global).
        let _guard = crate::config::tests::lock_env_for_test();
        let prev_tz = std::env::var("TZ").ok();
        // SAFETY: single-threaded w.r.t. env mutation (guarded by the shared
        // test env lock); test harness never reads TZ concurrently.
        unsafe { std::env::set_var("TZ", "UTC0") };

        let backend = ratatui::backend::TestBackend::new(90, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Line 0 = body top (line 0 of the buffer after the 1-row top bar).
        // Offset column is per-line RELATIVE (see `hexdump_parts` docs).
        assert!(
            row(1).starts_with("17:02:03.456 0000  68 65 6c 6c 6f"),
            "{}",
            row(1)
        );
        assert!(row(1).trim_end().ends_with("hello"));
        // Empty line renders blank (zero-byte hexdump), not an offset row.
        assert!(row(2).trim().is_empty(), "{}", row(2));
        assert_eq!(DEFAULT_SCROLLBACK, 10_000);

        // Restore the host's TZ before releasing the env lock.
        match prev_tz {
            Some(v) => unsafe { std::env::set_var("TZ", v) },
            None => unsafe { std::env::remove_var("TZ") },
        }
    }

    // ------------------------------------------------------------------
    // :filter view + :hl highlight (headless renders)
    // ------------------------------------------------------------------

    fn filter_app() -> App {
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        for text in ["alpha err", "beta ok", "gamma err"] {
            a.push_line(text.as_bytes().to_vec(), t, LineFlags::default());
        }
        a
    }

    #[test]
    fn filter_view_shows_only_matching_lines_and_off_restores_full() {
        let mut a = filter_app();
        a.filter_pattern = Some("err".into());
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let body: String = (1..6).map(row).collect::<Vec<_>>().join("\n");
        assert!(body.contains("alpha err"), "{}", body);
        assert!(body.contains("gamma err"), "{}", body);
        assert!(
            !body.contains("beta ok"),
            "filtered-out line must not render: {body}"
        );

        // `filter off` restores the full scrollback view.
        a.filter_pattern = None;
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let body: String = (1..6).map(row).collect::<Vec<_>>().join("\n");
        assert!(body.contains("beta ok"), "{}", body);
        assert!(body.contains("alpha err") && body.contains("gamma err"));
    }

    #[test]
    fn filter_respects_view_offset_motions_see_filtered_list() {
        let mut a = filter_app();
        a.filter_pattern = Some("err".into());
        a.viewport_rows = 2;
        a.motion_up(1);
        assert_eq!(a.view_offset, 1, "clamped to filtered len - 1");
        assert!(!a.follow_tail);
        let backend = ratatui::backend::TestBackend::new(40, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Offset 1 over a 2-line filtered list: the window is [alpha err] only.
        let body: String = (1..5).map(row).collect::<Vec<_>>().join("\n");
        assert!(body.contains("alpha err"), "{}", body);
        assert!(!body.contains("gamma err"), "{}", body);
    }

    #[test]
    fn hl_renders_whole_line_and_off_clears() {
        let mut a = App::new(&Config::default());
        a.push_line(
            b"hello world".to_vec(),
            SystemTime::now(),
            LineFlags::default(),
        );
        a.hl_pattern = Some("world".into());
        let backend = ratatui::backend::TestBackend::new(40, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = Config::default().theme;
        // Row 1 is the first body row: "hello world". The line CONTAINS the
        // pattern, so the ENTIRE line carries the themed accent + BOLD —
        // including the "hello " prefix that does not itself match.
        let cell = |x: u16| buf[(x, 1)].clone();
        assert_eq!(cell(0).symbol(), "h");
        assert_eq!(
            cell(0).fg,
            theme.hl,
            "whole line highlighted, not just the match"
        );
        assert!(cell(0).modifier.contains(ratatui::style::Modifier::BOLD));
        assert_eq!(cell(6).symbol(), "w");
        assert_eq!(cell(6).fg, theme.hl);
        assert!(cell(6).modifier.contains(ratatui::style::Modifier::BOLD));

        // `hl off` clears the highlighting: plain text, no accent anywhere.
        a.hl_pattern = None;
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        for x in 0..11u16 {
            assert_ne!(buf[(x, 1)].fg, theme.hl, "no accent after hl off");
        }
    }

    #[test]
    fn hl_whole_line_wins_over_search_on_matching_lines_only() {
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        // Both lines contain the search pattern "hit"; only the first also
        // matches the hl pattern.
        a.push_line(b"hit hl here".to_vec(), t, LineFlags::default());
        a.push_line(b"hit only search".to_vec(), t, LineFlags::default());
        a.hl_pattern = Some("hl".into());
        a.last_search = Some("hit".into());
        a.last_match = Some(0);
        let backend = ratatui::backend::TestBackend::new(40, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = Config::default().theme;
        let row =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(row(1).contains("hit hl here"), "{}", row(1));
        assert!(row(2).contains("hit only search"), "{}", row(2));
        // Precedence (documented in text_body): on the hl-matching line the
        // whole-line style wins — every text cell, even the non-matching
        // prefix "hit ", carries the accent.
        assert_eq!(buf[(0, 1)].symbol(), "h");
        assert_eq!(buf[(0, 1)].fg, theme.hl);
        assert_eq!(
            buf[(8, 1)].fg,
            theme.hl,
            "whole line accented; search spans suppressed on hl lines"
        );
        // On the NON-hl-matching line the search spans still render: the
        // "hit" match (cols 0-2) is accented, the rest stays plain.
        assert_eq!(buf[(0, 2)].symbol(), "h");
        assert_eq!(
            buf[(0, 2)].fg,
            theme.insert,
            "search spans show on non-hl lines"
        );
        assert_eq!(buf[(4, 2)].symbol(), "o");
        assert_ne!(
            buf[(4, 2)].fg,
            theme.insert,
            "non-match text stays plain off-hl lines"
        );
    }

    #[test]
    fn hl_whole_line_uses_theme_hl_key_when_customized() {
        // Custom `hl` colour: the whole-line `:hl` highlight switches to
        // theme.hl, while `/` search matches stay on theme.insert — the two
        // highlights have distinct theme keys (and distinct default accents).
        let cfg = Config {
            theme: crate::config::Theme {
                hl: ratatui::style::Color::Rgb(0x12, 0x34, 0x56),
                ..crate::config::Theme::default()
            },
            warnings: Vec::new(),
        };
        let mut a = App::new(&cfg);
        let t = SystemTime::now();
        a.push_line(b"hit hl here".to_vec(), t, LineFlags::default());
        a.push_line(b"hit only search".to_vec(), t, LineFlags::default());
        a.hl_pattern = Some("hl".into());
        a.last_search = Some("hit".into());
        a.last_match = Some(0);
        let backend = ratatui::backend::TestBackend::new(40, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = cfg.theme;
        // hl-matching line: whole line in the custom hl colour.
        assert_eq!(buf[(0, 1)].fg, theme.hl, "whole-line hl uses theme.hl");
        assert_eq!(buf[(8, 1)].fg, theme.hl);
        // Non-hl line: search match keeps theme.insert (NOT the custom hl).
        assert_eq!(
            buf[(0, 2)].fg,
            theme.insert,
            "search matches keep theme.insert"
        );
        assert_ne!(theme.hl, theme.insert);
    }

    #[test]
    fn hl_and_filter_combine_and_hex_view_skips_hl() {
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        a.push_line(b"warn hot here".to_vec(), t, LineFlags::default());
        a.push_line(b"cold line".to_vec(), t, LineFlags::default());
        // Filter one pattern, highlight another — both active at once.
        a.filter_pattern = Some("warn".into());
        a.hl_pattern = Some("hot".into());
        // 90 cols so the hex view's 47-char hex column + ASCII gutter fits.
        let backend = ratatui::backend::TestBackend::new(90, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = Config::default().theme;
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(row(1).contains("warn hot here"), "{}", row(1));
        assert!(!row(2).contains("cold"), "{}", row(2));
        assert_eq!(buf[(5, 1)].symbol(), "h", "'hot' starts at col 5");
        assert_eq!(
            buf[(5, 1)].fg,
            theme.hl,
            "hl spans styled in the filtered text view"
        );

        // Hex view: the filter still applies (decoded text), hl is skipped.
        a.hex_on = true;
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(
            row(1).contains("warn hot here"),
            "filter applies in hex view: {}",
            row(1)
        );
        assert!(!row(2).contains("cold"), "{}", row(2));
        assert_ne!(buf[(5, 1)].fg, theme.hl, "no hl accent in hex view");
    }

    #[test]
    fn tx_echo_line_renders_prefix_in_tx_color_over_hl_and_search() {
        // Local echo (`:echo on`) line: the render-only `> ` prefix plus the
        // whole line render in `theme.tx` — and the TX colour WINS over both
        // the whole-line `:hl` accent and `/` search spans (documented
        // precedence). No BOLD: TX lines are content, not an alert.
        let mut a = App::new(&Config::default());
        let t = SystemTime::now();
        a.push_line(
            b"hello from tx".to_vec(),
            t,
            LineFlags {
                truncated: false,
                tx: true,
            },
        );
        // Both highlight mechanisms match this line; TX styling must win.
        a.hl_pattern = Some("hello".into());
        a.last_search = Some("from".into());
        a.last_match = Some(0);
        let backend = ratatui::backend::TestBackend::new(40, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let tx = Config::default().theme.tx;
        let row =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(
            row(1).trim_end().starts_with("> hello from tx"),
            "render-only `> ` prefix: {}",
            row(1)
        );
        for x in 0..15u16 {
            assert_eq!(
                buf[(x, 1)].fg,
                tx,
                "col {x} (`{}`) renders in theme.tx",
                buf[(x, 1)].symbol()
            );
            assert!(
                !buf[(x, 1)].modifier.contains(Modifier::BOLD),
                "TX style carries no BOLD"
            );
        }
        // Control: a plain RX line right below keeps base styling (no prefix).
        a.push_line(b"plain rx".to_vec(), t, LineFlags::default());
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(
            row(2).trim_end().starts_with("plain rx"),
            "RX line has no `> ` prefix: {}",
            row(2)
        );
        assert_ne!(buf[(0, 2)].fg, tx, "RX line not in the TX accent");
    }

    #[test]
    fn tx_lines_render_neutral_in_hex_view() {
        // Hex view stays neutral: TX lines render like any line (byte-exact
        // hexdump), no `> ` prefix and no `theme.tx` colour (documented).
        let mut a = App::new(&Config::default());
        a.hex_on = true;
        let t = SystemTime::now();
        a.push_line(
            b"abc".to_vec(),
            t,
            LineFlags {
                truncated: false,
                tx: true,
            },
        );
        let backend = ratatui::backend::TestBackend::new(90, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let body = row(1);
        assert!(body.contains("0000"), "hexdump row: {body}");
        assert!(body.contains("61 62 63"), "hexdump row: {body}");
        assert!(!body.contains('>'), "no `> ` prefix in hex view: {body}");
        // Neutral means neutral: neither the offset column (col 0) nor the
        // ASCII gutter (col 55) carries the TX accent — the gutter has its
        // own `hex_ascii` theme key now, the offset `hex_offset`.
        assert_ne!(
            buf[(0, 1)].fg,
            Config::default().theme.tx,
            "hex offset column stays un-TX"
        );
        assert_ne!(
            buf[(55, 1)].fg,
            Config::default().theme.tx,
            "hex gutter stays un-TX"
        );
    }

    #[test]
    fn hex_columns_render_three_distinct_theme_colors() {
        // The `:hex on` row splits into three themed spans: the offset
        // column (`0000  `, cols 0..6) in `hex_offset`, the hex byte column
        // (cols 6..53, padding included) in `hex_byte`, and the ASCII gutter
        // (cols 53..) in `hex_ascii`. Row text is unchanged — only styles.
        let mut a = App::new(&Config::default());
        a.hex_on = true;
        let t = SystemTime::now();
        a.push_line(b"hi".to_vec(), t, LineFlags::default());
        let backend = ratatui::backend::TestBackend::new(90, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = Config::default().theme;
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(row(1).starts_with("0000  68 69"), "{}", row(1));
        for x in 0..6u16 {
            assert_eq!(
                buf[(x, 1)].fg,
                theme.hex_offset,
                "offset col {x} (`{}`) in theme.hex_offset",
                buf[(x, 1)].symbol()
            );
        }
        for x in 6..53u16 {
            assert_eq!(
                buf[(x, 1)].fg,
                theme.hex_byte,
                "hex col {x} (`{}`) in theme.hex_byte (padding included)",
                buf[(x, 1)].symbol()
            );
        }
        for x in 53..55u16 {
            assert_eq!(
                buf[(x, 1)].fg,
                theme.hex_ascii,
                "gutter pad col {x} in theme.hex_ascii (`{}`)",
                buf[(x, 1)].symbol()
            );
        }
        // The two printable gutter chars (`hi`).
        for x in 55..57u16 {
            assert_eq!(
                buf[(x, 1)].fg,
                theme.hex_ascii,
                "gutter col {x} (`{}`) in theme.hex_ascii",
                buf[(x, 1)].symbol()
            );
        }
        // Three DISTINCT tones — no two columns share a colour by default.
        assert_ne!(theme.hex_offset, theme.hex_byte);
        assert_ne!(theme.hex_byte, theme.hex_ascii);
        assert_ne!(theme.hex_offset, theme.hex_ascii);
    }

    #[test]
    fn hex_partial_row_padding_stays_in_the_hex_byte_span() {
        // A partial (sub-16-byte) final row pads the hex column with spaces;
        // those spaces belong to the `hex_byte` span so the ASCII gutter
        // stays column-aligned across rows (text unchanged, only styles).
        let mut a = App::new(&Config::default());
        a.hex_on = true;
        let t = SystemTime::now();
        a.push_line(b"abc".to_vec(), t, LineFlags::default());
        let backend = ratatui::backend::TestBackend::new(90, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let theme = Config::default().theme;
        let row =
            |y: u16| -> String { (0..90).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(row(1).starts_with("0000  61 62 63"), "{}", row(1));
        // Gutter alignment (ts off): ASCII starts at col 55 on every row
        // (6 offset + 47 hex + 2 gutter spaces); unchanged by the split.
        assert_eq!(&row(1)[55..58], "abc", "{}", row(1));
        // Padding spaces at the tail of the hex column (cols 14..53) carry
        // the hex_byte style, not base.
        for x in [14u16, 30, 52] {
            assert_eq!(
                buf[(x, 1)].fg,
                theme.hex_byte,
                "pad col {x} stays inside the hex_byte span",
            );
        }
        // The two gutter spaces before `abc` are hex_ascii, as on full rows.
        for x in 53..55u16 {
            assert_eq!(buf[(x, 1)].fg, theme.hex_ascii, "gutter pad col {x}");
        }
    }

    #[test]
    fn top_bar_shows_hl_and_filter_flags_when_active() {
        let mut a = App::new(&Config::default());
        a.hl_pattern = Some("x".into());
        a.filter_pattern = Some("y".into());
        let backend = ratatui::backend::TestBackend::new(60, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row: String = (0..60).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(row.starts_with("ncom  hl filter "), "{}", row);
    }

    // ------------------------------------------------------------------
    // Connection splash (block-art `ncom` wordmark, fresh app only)
    // ------------------------------------------------------------------

    #[test]
    fn splash_renders_centred_on_fresh_disconnected_app() {
        let mut a = App::new(&Config::default()); // connected: false, no scrollback
        let (cols, rows) = (80u16, 24u16);
        let backend = ratatui::backend::TestBackend::new(cols, rows);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row = |y: u16| -> String {
            (0..cols)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        };

        let lines = splash_lines();
        let art_width = lines[0].chars().count() as u16; // 25
        // Body area: 1-row top bar above, 1-row command row below -> 22 rows
        // starting at y=1. The 5 art rows are vertically centred in those.
        let body_top = 1u16;
        let body_height = rows - 2;
        let top = body_top + (body_height - lines.len() as u16) / 2;
        let expected_indent = (cols - art_width) / 2; // (80-25)/2 = 27
        for (i, art) in lines.iter().enumerate() {
            let y = top + i as u16;
            let r = row(y);
            // Leading indent spaces, then the art row, then trailing blanks.
            let indent = r.find('█').expect("art row present") as u16;
            assert_eq!(indent, expected_indent, "row {i} centred: {r:?}");
            let rendered: String = r
                .chars()
                .skip(indent as usize)
                .take(art_width as usize)
                .collect();
            assert_eq!(rendered, *art, "row {i} content: {r:?}");
        }
        // Nothing outside the art block on those rows.
        let first = row(top);
        assert!(first[..expected_indent as usize].chars().all(|c| c == ' '));
    }

    #[test]
    fn no_splash_when_connected() {
        let mut a = App::new(&Config::default());
        a.connected = true; // `--port` up, but no traffic yet
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        for y in 1..23u16 {
            let r: String = (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect();
            assert!(!r.contains('█'), "splash leaked at y={y}: {r:?}");
        }
    }

    #[test]
    fn no_splash_when_scrollback_non_empty() {
        let mut a = App::new(&Config::default());
        a.push_line(b"hello".to_vec(), SystemTime::now(), LineFlags::default());
        a.connected = false; // traffic seen, port now down: traffic view wins
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        for y in 1..23u16 {
            let r: String = (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect();
            assert!(!r.contains('█'), "splash leaked at y={y}: {r:?}");
        }
    }

    #[test]
    fn splash_uses_theme_normal_colour() {
        let mut a = App::new(&Config::default());
        let theme = Config::default().theme;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let top = 1 + (22 - splash_lines().len() as u16) / 2;
        let indent = (80u16 - splash_lines()[0].chars().count() as u16) / 2;
        // First cell of the wordmark and a mid-art space cell share the art
        // row; the ink cell must be theme.normal foreground, not bold.
        let cell = buf[(indent, top)].clone();
        assert_eq!(cell.fg, theme.normal);
        assert!(!cell.modifier.contains(ratatui::style::Modifier::BOLD));
    }
}
