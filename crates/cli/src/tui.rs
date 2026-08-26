//! TUI render (ratatui) and event loop (crossterm). Rendering consumes a
//! `DomainSnapshot` (the same data a socket client receives); only the live
//! chat pane needs the local `App` (the PTY screen is not on the wire yet).

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, BoardFocus, Tab};
use crate::keymap;
use crate::protocol::{
    CardView, CheckVerdict, CheckView, DocsView, DomainSnapshot, EnforceId, FindingView, FocusId,
    InputKind, ParallelMark, ReviewProgressId, ReviewView, SessionKind, SeverityId, StatusId,
    TabId, TaskTypeId, TaskView,
};
use crate::snapshot::build_snapshot;

// --- theme (Charm/Lipgloss aesthetic) -------------------------------------
const ACCENT: Color = Color::Rgb(180, 142, 255); // lavender (Charm signature)
const PINK: Color = Color::Rgb(255, 121, 198); // accent pink
const BORDER: Color = Color::Rgb(82, 80, 112); // subtle border
const SUBTLE: Color = Color::Rgb(128, 128, 150); // dimmed text
const SEL_BG: Color = Color::Rgb(54, 48, 92); // selection background
const BG_TITLE_FG: Color = Color::Rgb(20, 20, 28);

/// Charm-style base panel: rounded border, chip title, and padding.
fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .padding(Padding::new(2, 2, 1, 0))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(BG_TITLE_FG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ))
}

/// Panel without padding (so the PTY fills the exact area).
fn panel_tight(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(BG_TITLE_FG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ))
}

/// Selected-line style (background bar + bold text).
fn sel_style() -> Style {
    Style::default()
        .bg(SEL_BG)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

const MARGIN_X: u16 = 2;
const MARGIN_Y: u16 = 1;

/// Central geometry: outer gutter + header/body/footer with spacing.
/// Used by render AND `sync_pty_size` so they agree on the PTY size.
fn root_layout(area: Rect) -> (Rect, Rect, Rect) {
    let inner = area.inner(Margin {
        vertical: MARGIN_Y,
        horizontal: MARGIN_X,
    });
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // body
        Constraint::Length(1), // spacer
        Constraint::Length(1), // footer
    ])
    .split(inner);
    (rows[0], rows[2], rows[4])
}

pub fn run(mut app: App) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let res = run_loop(&mut terminal, &mut app);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    res
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.drain_pty();
        app.drain_sidecar();
        app.tick_permissions();
        app.tick_sidecar_health();
        app.poll_job();
        app.tick_reload();
        app.tick_pr_sync();
        app.tick_toast();
        // session events feed connected clients; the local TUI reads the PTY
        // parser directly, so the buffer is just discarded here.
        let _ = app.take_session_events();
        sync_pty_size(terminal, app);
        let snap = build_snapshot(app);
        terminal.draw(|f| render(f, &snap, Some(app)))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => handle_key(app, k),
                Event::Mouse(m) => {
                    if let Ok(size) = terminal.size() {
                        handle_mouse(app, m, size.width, size.height);
                    }
                }
                _ => {}
            }
        }
        if app.edit_request {
            app.edit_request = false;
            edit_conventions(terminal, app);
        }
        if app.should_quit {
            app.stop_all_sessions();
            break;
        }
    }
    Ok(())
}

/// Suspends the TUI, opens `conventions.md` in `$EDITOR`, then resumes and reloads.
fn edit_conventions(terminal: &mut DefaultTerminal, app: &mut App) {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    ratatui::restore();
    let _ = std::process::Command::new(editor)
        .arg(&app.conventions_path)
        .status();
    *terminal = ratatui::init();
    let _ = terminal.clear();
    app.reload_conventions();
    app.status_msg = "conventions.md updated".into();
}

/// Keeps the PTY and vt100 parser at the EXACT size of the Session pane (body
/// minus borders). Otherwise `claude` wraps at a different width than rendered
/// and the text comes out corrupted.
fn sync_pty_size(terminal: &DefaultTerminal, app: &mut App) {
    let Ok(size) = terminal.size() else { return };
    sync_pty_to(app, size.width, size.height);
}

/// Like `sync_pty_size`, but from an explicit size. Syncs only the SELECTED
/// (displayed) session with the master-detail terminal pane.
pub(crate) fn sync_pty_to(app: &mut App, width: u16, height: u16) {
    let (_h, body, _f) = root_layout(Rect::new(0, 0, width, height));
    // the chat is the 3rd Board column (or the whole area in fullscreen).
    let term = if app.chat_fullscreen {
        body
    } else {
        board_layout(body)[2]
    };
    // the terminal pane has a border (-2 on each axis)
    let cols = term.width.saturating_sub(2);
    let rows = term.height.saturating_sub(2);
    if cols == 0 || rows == 0 {
        return;
    }
    let Some(sel) = app.current_session_idx() else {
        return;
    };
    if let Some(e) = app.sessions.get_mut(sel)
        && e.parser.screen().size() != (rows, cols)
    {
        if let Some(s) = &e.session {
            let _ = s.resize(rows, cols);
        }
        e.parser.screen_mut().set_size(rows, cols);
    }
}

/// Interior area (borderless) of the chat pane, in absolute coords.
pub(crate) fn session_term_area(app: &App, width: u16, height: u16) -> Rect {
    let (_h, body, _f) = root_layout(Rect::new(0, 0, width, height));
    let term = if app.chat_fullscreen {
        body
    } else {
        board_layout(body)[2]
    };
    Rect {
        x: term.x + 1,
        y: term.y + 1,
        width: term.width.saturating_sub(2),
        height: term.height.saturating_sub(2),
    }
}

/// Handles a mouse event over the chat pane: if claude is in mouse mode
/// (SGR), forwards the event to the PTY; otherwise scrolls the embedded
/// terminal's scrollback (vt100) with the wheel.
pub(crate) fn handle_mouse(
    app: &mut App,
    ev: crossterm::event::MouseEvent,
    width: u16,
    height: u16,
) {
    use crossterm::event::MouseEventKind;
    if app.tab != Tab::Board || app.board_focus != BoardFocus::Chat {
        return;
    }
    let area = session_term_area(app, width, height);
    let inside = ev.column >= area.x
        && ev.column < area.x.saturating_add(area.width)
        && ev.row >= area.y
        && ev.row < area.y.saturating_add(area.height);
    if !inside {
        return;
    }
    let Some(sel) = app.current_session_idx() else {
        return;
    };
    let Some(e) = app.sessions.get_mut(sel) else {
        return;
    };

    let screen = e.parser.screen();
    let mouse_on = screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
        && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr;

    if mouse_on {
        let col = ev.column - area.x + 1; // 1-based, relative to the PTY
        let row = ev.row - area.y + 1;
        if let Some(bytes) = encode_mouse_sgr(&ev, col, row)
            && let Some(s) = &mut e.session
        {
            let _ = s.write_input(&bytes);
        }
        return;
    }

    // claude didn't capture the mouse: scroll the embedded terminal's scrollback.
    let back = e.parser.screen().scrollback();
    match ev.kind {
        MouseEventKind::ScrollUp => e.parser.screen_mut().set_scrollback(back + 3),
        MouseEventKind::ScrollDown => e.parser.screen_mut().set_scrollback(back.saturating_sub(3)),
        _ => {}
    }
}

/// Encodes a mouse event in the SGR protocol (1006): `ESC [ < cb ; col ; row (M|m)`.
fn encode_mouse_sgr(ev: &crossterm::event::MouseEvent, col: u16, row: u16) -> Option<Vec<u8>> {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let btn = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut cb, release) = match ev.kind {
        MouseEventKind::Down(b) => (btn(b), false),
        MouseEventKind::Up(b) => (btn(b), true),
        MouseEventKind::Drag(b) => (btn(b) + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        MouseEventKind::Moved => return None,
    };
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    let suffix = if release { 'm' } else { 'M' };
    Some(format!("\x1b[<{cb};{col};{row}{suffix}").into_bytes())
}

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    let overlay_active = app.job_overlay
        || app.project_picker
        || app.detail_open
        || app.doc_open
        || app.input.is_some();

    // Chat focus (Board): ALL keys go to `claude` through the local PTY. jaum
    // commands via the Ctrl+G prefix: Ctrl+G 1-2 tab · q quit · f finish · x
    // remove · z fullscreen · n/p card · h back to cards · g sends literal Ctrl+G.
    if !overlay_active
        && app.tab == Tab::Board
        && app.board_focus == BoardFocus::Chat
        && app.selected_card_is_live()
    {
        if app.pending_prefix {
            app.pending_prefix = false;
            match key.code {
                KeyCode::Char(c @ '1'..='2') => {
                    app.tab = Tab::from_index(c as usize - '1' as usize)
                }
                KeyCode::Char('q') => app.should_quit = true,
                KeyCode::Char('f') => app.finish_selected_session(),
                KeyCode::Char('x') => app.close_selected_session(),
                KeyCode::Char('z') => app.chat_fullscreen = !app.chat_fullscreen,
                KeyCode::Char('h') | KeyCode::Left => app.board_focus = BoardFocus::Cards,
                KeyCode::Char('n') | KeyCode::Char('j') => {
                    app.card_next();
                    if !app.selected_card_is_live() {
                        app.board_focus = BoardFocus::Cards;
                    }
                }
                KeyCode::Char('p') | KeyCode::Char('k') => {
                    app.card_prev();
                    if !app.selected_card_is_live() {
                        app.board_focus = BoardFocus::Cards;
                    }
                }
                KeyCode::Char('g') => {
                    if let Some(i) = app.current_session_idx()
                        && let Some(e) = app.sessions.get_mut(i)
                        && let Some(s) = &mut e.session
                    {
                        let _ = s.write_input(&[0x07]); // Ctrl+G literal (BEL)
                    }
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            app.pending_prefix = true;
            return;
        }
        // Esc leaves the chat back to the cards (not forwarded to the PTY).
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            app.board_focus = BoardFocus::Cards;
            return;
        }
        if let Some(i) = app.current_session_idx()
            && let Some(e) = app.sessions.get_mut(i)
        {
            let bytes = key_to_bytes(key);
            if !bytes.is_empty()
                && let Some(s) = &mut e.session
            {
                let _ = s.write_input(&bytes);
            }
        }
        return;
    }

    let ctx = keymap::KeyCtx::from_app(app);
    if let Some(intent) = keymap::map_key(&ctx, key) {
        app.apply_intent(keymap::with_local_prefill(intent));
    }
}

/// Translates a key into PTY bytes (basic coverage).
fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl-A..Ctrl-Z -> 0x01..0x1a
                let lc = c.to_ascii_lowercase();
                if lc.is_ascii_lowercase() {
                    return vec![(lc as u8) - b'a' + 1];
                }
            }
            c.to_string().into_bytes()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => Vec::new(),
    }
}

// --- render ----------------------------------------------------------------

/// Renders a snapshot. `app` gives access to the local PTY screens (chat
/// pane); socket clients pass `None` and get a placeholder until sessions
/// stream over the wire.
pub(crate) fn render(f: &mut Frame, snap: &DomainSnapshot, app: Option<&App>) {
    let (header, body, footer) = root_layout(f.area());

    render_header(f, snap, header);
    match snap.tab {
        TabId::Board => render_board(f, snap, app, body),
        TabId::Docs => render_docs(f, &snap.docs, body),
    }
    render_statusline(f, snap, footer);

    if snap.board.detail_open {
        render_detail(f, snap);
    }
    if snap.docs.doc_open {
        render_doc_view(f, &snap.docs);
    }
    if snap.picker.is_some() {
        render_picker(f, snap);
    }
    if snap.job_overlay {
        render_job(f, snap);
    }
    // snackbar on top of everything
    if let Some(msg) = &snap.toast {
        render_toast(f, msg);
    }
}

/// Live log overlay for a job (ingest/capture/init).
fn render_job(f: &mut Frame, snap: &DomainSnapshot) {
    use ratatui::widgets::Clear;
    let Some(job) = &snap.job else { return };

    let area = centered_rect(82, 72, f.area());
    f.render_widget(Clear, area);

    let (state_txt, color) = if job.finished {
        ("done", Color::Green)
    } else {
        ("running…", ACCENT)
    };
    // nav hint: changes depending on whether it's live or paused reading.
    let nav = if job.follow {
        "j/k scroll · Esc close"
    } else {
        "j/k scroll · G live · Esc close"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .padding(Padding::new(2, 2, 1, 0))
        .title(Span::styled(
            format!(" {} · {state_txt} · {nav} ", job.title),
            Style::default()
                .fg(BG_TITLE_FG)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ));

    // inner area: borders (-2) + horizontal padding (2+2) and top padding (1).
    let inner_w = area.width.saturating_sub(6).max(1) as usize;
    let visible_rows = area.height.saturating_sub(3).max(1) as usize;

    let lines: Vec<Line> = job.logs.iter().map(|l| job_log_line(l)).collect();

    // estimate wrapped height to clamp the scroll and drive follow.
    let wrapped: usize = job
        .logs
        .iter()
        .map(|l| l.chars().count().div_ceil(inner_w).max(1))
        .sum();
    let max_scroll = wrapped.saturating_sub(visible_rows) as u16;
    let scroll = if job.follow {
        max_scroll
    } else {
        job.scroll.min(max_scroll)
    };

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: true })
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

/// Styles a job log line based on its prefix.
fn job_log_line(l: &str) -> Line<'static> {
    if let Some(rest) = l.strip_prefix("→ ") {
        // "Tool argument…" — highlights the tool name.
        let (tool, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        let mut spans = vec![
            Span::styled("→ ", Style::default().fg(PINK)),
            Span::styled(
                tool.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ];
        if !arg.is_empty() {
            spans.push(Span::styled(
                format!(" {arg}"),
                Style::default().fg(Color::White),
            ));
        }
        Line::from(spans)
    } else if let Some(rest) = l.strip_prefix("∴ ") {
        Line::from(Span::styled(
            format!("∴ {rest}"),
            Style::default()
                .fg(Color::Rgb(150, 150, 170))
                .add_modifier(Modifier::ITALIC),
        ))
    } else if let Some(rest) = l.strip_prefix("— ") {
        Line::from(Span::styled(
            format!("— {rest}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else if let Some(rest) = l.strip_prefix('[') {
        // "[model] text…" — dimmed model, bright text.
        match rest.split_once("] ") {
            Some((model, text)) => Line::from(vec![
                Span::styled(format!("[{model}] "), Style::default().fg(ACCENT)),
                Span::styled(text.to_string(), Style::default().fg(Color::White)),
            ]),
            None => Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(Color::White),
            )),
        }
    } else {
        Line::from(Span::styled(l.to_string(), Style::default().fg(SUBTLE)))
    }
}

fn status_name(s: StatusId) -> &'static str {
    match s {
        StatusId::Backlog => "backlog",
        StatusId::Ready => "ready",
        StatusId::Wip => "wip",
        StatusId::Review => "review",
        StatusId::Merged => "merged",
    }
}

/// Color style per status (board and badges).
fn status_color(s: StatusId) -> Color {
    match s {
        StatusId::Wip => Color::Green,
        StatusId::Review => Color::Yellow,
        StatusId::Ready => Color::Blue,
        StatusId::Backlog => Color::Gray,
        StatusId::Merged => Color::DarkGray,
    }
}

fn task_type_name(t: TaskTypeId) -> &'static str {
    match t {
        TaskTypeId::Impl => "Impl",
        TaskTypeId::Spike => "Spike",
    }
}

fn session_kind_label(k: SessionKind) -> &'static str {
    match k {
        SessionKind::Play => "play",
        SessionKind::Review => "review",
        SessionKind::Setup => "setup",
    }
}

/// termimad skin matched to the app's Charm theme (lavender/pink/dimmed).
fn md_skin() -> termimad::MadSkin {
    use termimad::crossterm::style::Color::Rgb;
    let mut skin = termimad::MadSkin::default();
    skin.set_fg(Rgb {
        r: 200,
        g: 200,
        b: 210,
    });
    skin.set_headers_fg(Rgb {
        r: 180,
        g: 142,
        b: 255,
    }); // ACCENT
    skin.bold.set_fg(Rgb {
        r: 255,
        g: 255,
        b: 255,
    });
    skin.italic.set_fg(Rgb {
        r: 150,
        g: 150,
        b: 170,
    });
    skin.inline_code.set_fg(Rgb {
        r: 255,
        g: 121,
        b: 198,
    }); // PINK
    skin
}

/// Markdown render -> ratatui lines. Prose goes to termimad (themes, wrap,
/// code, lists) via ANSI + `ansi-to-tui`; tables are rendered here
/// (`render_table`) with a full border and per-cell wrap, because termimad's
/// table has no top/bottom border and inconsistent padding.
/// `width` is the inner width of the target panel.
fn markdown_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prose = String::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        // GFM table: a row with `|` followed by a `|---|` separator line.
        if is_table_row(raw) && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            flush_prose(&mut prose, &mut out, width);
            let mut rows = vec![raw];
            i += 2; // skip header + separator
            while i < lines.len() && is_table_row(lines[i]) && !is_table_separator(lines[i]) {
                rows.push(lines[i]);
                i += 1;
            }
            render_table(&mut out, &rows, width);
            continue;
        }
        prose.push_str(raw);
        prose.push('\n');
        i += 1;
    }
    flush_prose(&mut prose, &mut out, width);
    out
}

/// Renders accumulated prose via termimad (ANSI -> Text) and clears the buffer.
fn flush_prose(prose: &mut String, out: &mut Vec<Line<'static>>, width: u16) {
    use ansi_to_tui::IntoText;
    if !prose.trim().is_empty() {
        let w = width.max(8) as usize;
        let ansi = format!("{}", md_skin().text(prose, Some(w)));
        match ansi.into_text() {
            Ok(t) => out.extend(t.lines),
            Err(_) => out.extend(prose.lines().map(|l| Line::from(l.to_string()))),
        }
    }
    prose.clear();
}

/// Plain text of a cell (no inline markers) — used to measure and wrap.
fn strip_inline(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}

fn is_table_row(r: &str) -> bool {
    let t = r.trim();
    t.contains('|') && !t.starts_with("```")
}

fn is_table_separator(r: &str) -> bool {
    let t = r.trim();
    if !t.contains('|') {
        return false;
    }
    let cells = split_row(r);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':'))
}

/// Splits a table row into cells (respects escaped `\|`).
fn split_row(r: &str) -> Vec<String> {
    let t = r.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.replace("\\|", "\u{0}")
        .split('|')
        .map(|c| c.trim().replace('\u{0}', "|"))
        .collect()
}

/// Wraps a cell's text into lines of up to `w` columns (word-wrap; a word
/// longer than `w` is hard-split).
fn wrap_cell(text: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if word.chars().count() > w {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == w {
                    out.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            line = chunk;
            continue;
        }
        let extra = usize::from(!line.is_empty());
        if line.chars().count() + extra + word.chars().count() > w {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Renders a GFM table with a full rounded border, highlighted header, and
/// per-cell wrap to fit `width`. `rows[0]` is the header.
fn render_table(out: &mut Vec<Line<'static>>, rows: &[&str], width: u16) {
    let bs = Style::default().fg(BORDER);
    let cells: Vec<Vec<String>> = rows.iter().map(|r| split_row(r)).collect();
    let ncol = cells.iter().map(|c| c.len()).max().unwrap_or(0);
    if ncol == 0 {
        return;
    }
    // natural (max) width of each column.
    let mut widths = vec![0usize; ncol];
    for row in &cells {
        for (ci, c) in row.iter().enumerate() {
            let w = strip_inline(c).chars().count();
            if w > widths[ci] {
                widths[ci] = w;
            }
        }
    }
    // shrink the widest columns until the frame fits (each column costs w+3:
    // 1 border + 2 spaces; +1 for the final border).
    const MIN_COL: usize = 3;
    let budget = (width as usize).saturating_sub(3 * ncol + 1);
    let floor = MIN_COL * ncol;
    let mut sum: usize = widths.iter().sum();
    while sum > budget.max(floor) {
        let Some(idx) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > MIN_COL)
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)
        else {
            break;
        };
        widths[idx] -= 1;
        sum -= 1;
    }

    // draws a horizontal rule (top/middle/bottom).
    let rule = |left: char, mid: char, right: char| -> Line<'static> {
        let mut s = String::new();
        s.push(left);
        for (ci, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push(if ci + 1 < ncol { mid } else { right });
        }
        Line::from(Span::styled(s, bs))
    };

    out.push(rule('╭', '┬', '╮'));
    for (ri, row) in cells.iter().enumerate() {
        let is_header = ri == 0;
        let base = if is_header {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(200, 200, 210))
        };
        let wrapped: Vec<Vec<String>> = (0..ncol)
            .map(|ci| {
                let cell = row.get(ci).map(String::as_str).unwrap_or("");
                wrap_cell(&strip_inline(cell), widths[ci])
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for li in 0..height {
            let mut spans: Vec<Span<'static>> = vec![Span::styled("│", bs)];
            for (ci, w) in widths.iter().enumerate() {
                let piece = wrapped[ci].get(li).cloned().unwrap_or_default();
                let pad = w.saturating_sub(piece.chars().count());
                spans.push(Span::raw(" "));
                spans.push(Span::styled(piece, base));
                spans.push(Span::raw(" ".repeat(pad + 1)));
                spans.push(Span::styled("│", bs));
            }
            out.push(Line::from(spans));
        }
        if is_header {
            out.push(rule('├', '┼', '┤'));
        }
    }
    out.push(rule('╰', '┴', '╯'));
}

/// Doc view overlay (rendered markdown).
fn render_doc_view(f: &mut Frame, docs: &DocsView) {
    use ratatui::widgets::Clear;

    let area = centered_rect(85, 85, f.area());
    f.render_widget(Clear, area);

    let title = docs.list.get(docs.selected).cloned().unwrap_or_default();
    let p = Paragraph::new(markdown_lines(&docs.preview, area.width.saturating_sub(7)))
        .block(overlay_panel(&format!("{title} · j/k scroll · Esc close")))
        .wrap(Wrap { trim: false })
        .scroll((docs.scroll, 0));
    f.render_widget(p, area);
}

/// Overlay panel: like `panel`, but with an accent border (focus).
fn overlay_panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .padding(Padding::new(2, 2, 1, 0))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(BG_TITLE_FG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ))
}

/// Short (7-char) commit hash, git-style.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Current Unix time in seconds (0 if the clock predates the epoch).
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Human "when" for a review taken at `reviewed_at` (Unix seconds): relative
/// ("5min ago") until a week out, then the absolute local date/time.
fn review_when(reviewed_at: u64) -> String {
    fmt_review_when(now_unix_secs().saturating_sub(reviewed_at), reviewed_at)
}

/// Pure formatter split out for tests: `elapsed` drives the relative buckets;
/// `reviewed_at` is only used for the absolute fallback beyond a week.
fn fmt_review_when(elapsed: u64, reviewed_at: u64) -> String {
    match elapsed {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}min ago", elapsed / 60),
        3600..=86399 => format!("{}h ago", elapsed / 3600),
        86400..=604_799 => format!("{}d ago", elapsed / 86400),
        _ => local_datetime(reviewed_at),
    }
}

/// Absolute local date/time `YYYY-MM-DD HH:MM` for a Unix timestamp, via libc's
/// `localtime_r` (no date-library dependency).
fn local_datetime(secs: u64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r fills a caller-owned `tm`; we zero it first and only
    // read scalar fields it populates.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return format!("@{secs}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// Joins distinct short shas (multi-PR tasks) so a tag says which commits a
/// review covers. `None` when the list is empty.
fn join_shas(shas: &[String]) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    for s in shas {
        let short = short_sha(s);
        if !seen.contains(&short) {
            seen.push(short);
        }
    }
    (!seen.is_empty()).then(|| seen.join(", "))
}

/// Short hash(es) the task's review was taken against, from the per-PR
/// `reviewed_sha` carried in the snapshot. `None` when nothing was reviewed yet.
fn reviewed_tag(t: &TaskView) -> Option<String> {
    let shas: Vec<String> = t
        .prs
        .iter()
        .filter_map(|p| p.reviewed_sha.clone())
        .collect();
    join_shas(&shas)
}

/// Content lines of a task (metadata + markdown body). Reused by the Board
/// preview and the detail overlay.
fn task_detail_lines(t: &TaskView, width: u16) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            task_type_name(t.task_type),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            status_name(t.status).to_uppercase(),
            Style::default().fg(status_color(t.status)),
        ),
    ]));

    if !t.rfcs.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("RFCs ", Style::default().fg(SUBTLE)),
            Span::raw(t.rfcs.join(", ")),
        ]));
    }
    if !t.adrs.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("ADRs ", Style::default().fg(SUBTLE)),
            Span::raw(t.adrs.join(", ")),
        ]));
    }
    for pr in &t.prs {
        let n = if pr.pr == 0 {
            "PR not created".to_string()
        } else {
            format!("PR #{}", pr.pr)
        };
        lines.push(Line::from(format!("{} @ {} ({n})", pr.repo, pr.branch)));
    }
    if !t.constraints.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Constraints", bold)));
        for c in &t.constraints {
            let (tag, color) = match c.enforce {
                EnforceId::Hook => ("hook", Color::Red),
                EnforceId::Review => ("review", PINK),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  [{tag}] "), Style::default().fg(color)),
                Span::raw(c.text.clone()),
            ]));
        }
    }
    if !t.deferred.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Deferred", bold)));
        for d in &t.deferred {
            lines.push(Line::from(format!("  - {d}")));
        }
    }

    lines.push(Line::from(""));
    lines.extend(markdown_lines(&t.body, width));
    lines
}

/// Selected task view (None while the `· project` row is selected).
fn selected_task(snap: &DomainSnapshot) -> Option<&TaskView> {
    if snap.board.project_selected {
        return None;
    }
    snap.board.tasks.get(snap.board.selected)
}

/// Overlay with the full content of the selected task (Enter/`o`).
fn render_detail(f: &mut Frame, snap: &DomainSnapshot) {
    use ratatui::widgets::Clear;
    let Some(t) = selected_task(snap) else { return };
    let area = centered_rect(80, 80, f.area());
    f.render_widget(Clear, area);
    let p = Paragraph::new(task_detail_lines(t, area.width.saturating_sub(7)))
        .block(overlay_panel(&format!("{} · j/k scroll · Esc close", t.id)))
        .wrap(Wrap { trim: false })
        .scroll((snap.board.detail_scroll, 0));
    f.render_widget(p, area);
}

/// Header: logo + tab pills (left) and project name (right).
fn render_header(f: &mut Frame, snap: &DomainSnapshot, area: Rect) {
    let cols = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(snap.project.chars().count() as u16 + 4),
    ])
    .split(area);

    let mut spans = vec![
        Span::styled(
            "◆ jaum",
            Style::default().fg(PINK).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
    ];
    for (i, (tab, title)) in [(TabId::Board, "Board"), (TabId::Docs, "Docs")]
        .iter()
        .enumerate()
    {
        let label = format!(" {} {} ", i + 1, title);
        if *tab == snap.tab {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(BG_TITLE_FG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(label, Style::default().fg(SUBTLE)));
        }
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);

    let right = Line::from(vec![
        Span::styled("", Style::default().fg(BORDER)),
        Span::styled(
            format!(" {} ", snap.project),
            Style::default().fg(SUBTLE).add_modifier(Modifier::BOLD),
        ),
    ])
    .right_aligned();
    f.render_widget(Paragraph::new(right), cols[1]);
}

/// Central overlay with the project list (`P` key).
fn render_picker(f: &mut Frame, snap: &DomainSnapshot) {
    use ratatui::widgets::Clear;

    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = snap
        .projects
        .iter()
        .map(|p| ListItem::new(format!("{}  {}", p.name, p.backlog)))
        .collect();
    let mut state = ListState::default();
    state.select(snap.picker.as_ref().map(|p| p.selected));
    let list = List::new(items)
        .block(overlay_panel("Projects · Enter switch · Esc close"))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Centered rectangle (percentages of the area).
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

/// Board split: tasks | task items (cards) | chat/content.
pub(crate) fn board_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::horizontal([
        Constraint::Percentage(24),
        Constraint::Percentage(28),
        Constraint::Percentage(48),
    ])
    .spacing(1)
    .split(area)
}

/// Block with a highlighted border when the panel is focused.
fn panel_focus(title: &str, focused: bool) -> Block<'static> {
    let b = panel(title);
    if focused {
        b.border_style(Style::default().fg(ACCENT))
    } else {
        b
    }
}

fn render_board(f: &mut Frame, snap: &DomainSnapshot, app: Option<&App>, area: Rect) {
    // chat fullscreen: the card content takes the whole area.
    if snap.board.chat_fullscreen {
        render_card_content(f, snap, app, area);
        return;
    }
    let cols = board_layout(area);
    render_board_list(f, snap, cols[0]);
    render_task_cards(f, snap, cols[1]);
    render_card_content(f, snap, app, cols[2]);
}

fn render_board_list(f: &mut Frame, snap: &DomainSnapshot, area: Rect) {
    let board = &snap.board;
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_task: Vec<Option<usize>> = Vec::new();
    let mut last_status: Option<StatusId> = None;

    // synthetic "· project" row (top): holds the setup sessions.
    let mut proj = vec![Span::styled(
        "· project",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if board.setup_live {
        proj.push(Span::styled(" ●", Style::default().fg(Color::Green)));
    }
    if board.setup_needed {
        proj.push(Span::styled(
            " setup (S)",
            Style::default().fg(Color::Yellow),
        ));
    }
    items.push(ListItem::new(Line::from(proj)));
    row_to_task.push(None);

    for (i, t) in board.tasks.iter().enumerate() {
        if last_status != Some(t.status) {
            let count = board.tasks.iter().filter(|x| x.status == t.status).count();
            let color = status_color(t.status);
            items.push(ListItem::new(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(color)),
                Span::styled(
                    status_name(t.status).to_uppercase(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {count}"), Style::default().fg(Color::DarkGray)),
            ])));
            row_to_task.push(None);
            last_status = Some(t.status);
        }

        let badge = match t.status {
            StatusId::Wip => ("▶", Color::Green),
            StatusId::Merged => ("✔", Color::DarkGray),
            _ => ("·", SUBTLE),
        };
        let mut spans = vec![
            Span::styled(format!("{} ", badge.0), Style::default().fg(badge.1)),
            Span::styled(t.id.clone(), Style::default().add_modifier(Modifier::BOLD)),
        ];
        // live session open on this task (there's a chat to enter).
        if t.live_session {
            spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
        }
        // review progress: running now, or a newer commit still owing a review.
        if let Some(p) = t.review_progress {
            let (g, c) = match p {
                ReviewProgressId::Running => ("⟳", Color::Yellow),
                ReviewProgressId::AwaitingCi => ("◷", Color::Yellow),
                ReviewProgressId::CiFailed => ("✗", Color::Red),
            };
            spans.push(Span::styled(format!(" {g}"), Style::default().fg(c)));
        }
        // review verdict (if there's a `.review.md`).
        if let Some(r) = &t.review {
            let (g, c) = if r.badge == 0 {
                ("✓", Color::Green)
            } else {
                ("⚑", Color::Red)
            };
            spans.push(Span::styled(format!(" {g}"), Style::default().fg(c)));
        }
        // parallelism glyph (only when there are active tasks and this isn't one).
        match t.parallel {
            Some(ParallelMark::Conflict) => {
                spans.push(Span::styled(" ⚠", Style::default().fg(Color::Yellow)));
            }
            Some(ParallelMark::Safe) => {
                spans.push(Span::styled(" ‖", Style::default().fg(Color::Green)));
            }
            None => {}
        }
        items.push(ListItem::new(Line::from(spans)));
        row_to_task.push(Some(i));
    }

    let selected_row = if board.project_selected {
        Some(0) // the · project row is the first item
    } else {
        row_to_task.iter().position(|r| *r == Some(board.selected))
    };
    let mut state = ListState::default();
    state.select(selected_row);

    let title = format!("Board · {}", board.tasks.len());
    let list = List::new(items)
        .block(panel_focus(&title, board.focus == FocusId::Tasks))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Middle column: compact task detail at the top + the card list (sessions
/// + verdict). Cards are the navigable rows (`card_selected`).
fn render_task_cards(f: &mut Frame, snap: &DomainSnapshot, area: Rect) {
    let board = &snap.board;
    let focused = board.focus == FocusId::Cards;
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_card: Vec<Option<usize>> = Vec::new();
    // task body (goal/description), rendered below the Items.
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    let mut detail = |items: &mut Vec<ListItem>, l: Line<'static>| {
        items.push(ListItem::new(l));
        row_to_card.push(None);
    };

    // compact detail (· project row or task)
    let title = if board.project_selected {
        detail(
            &mut items,
            Line::from(Span::styled(
                "project config (setup)",
                Style::default().fg(SUBTLE),
            )),
        );
        if board.setup_needed {
            detail(
                &mut items,
                Line::from(Span::styled(
                    "setup pending — S opens the chat",
                    Style::default().fg(Color::Yellow),
                )),
            );
        }
        "· project".to_string()
    } else if let Some(t) = selected_task(snap) {
        detail(
            &mut items,
            Line::from(Span::styled(
                format!(
                    "{} · {}",
                    task_type_name(t.task_type),
                    status_name(t.status)
                ),
                Style::default().fg(SUBTLE),
            )),
        );
        if let Some(pr) = t.prs.first() {
            let pr_txt = if pr.pr != 0 {
                format!("PR #{} · {}", pr.pr, pr.branch)
            } else {
                format!("no PR · {}", pr.branch)
            };
            detail(
                &mut items,
                Line::from(Span::styled(pr_txt, Style::default().fg(SUBTLE))),
            );
        }
        if let Some(r) = &t.review {
            let (mut txt, c) = if r.clean {
                ("review CLEAN".to_string(), Color::Green)
            } else {
                (format!("review DIRTY · {} pending", r.unmet), Color::Red)
            };
            // tag the commit the verdict was taken against (short reviewed SHA).
            if let Some(tag) = reviewed_tag(t) {
                txt.push_str(&format!(" · @{tag}"));
            }
            // and when the capture ran.
            if let Some(at) = r.reviewed_at {
                txt.push_str(&format!(" · {}", review_when(at)));
            }
            detail(
                &mut items,
                Line::from(Span::styled(txt, Style::default().fg(c))),
            );
        }
        // pending/in-flight review state (new commit not yet reviewed).
        if let Some(p) = t.review_progress {
            let (txt, c) = match p {
                ReviewProgressId::Running => ("⟳ review running", Color::Yellow),
                ReviewProgressId::AwaitingCi => ("◷ re-review pending · CI running", Color::Yellow),
                ReviewProgressId::CiFailed => ("✗ review blocked · CI red", Color::Red),
            };
            detail(
                &mut items,
                Line::from(Span::styled(txt, Style::default().fg(c))),
            );
        }
        match t.parallel {
            Some(ParallelMark::Conflict) => detail(
                &mut items,
                Line::from(Span::styled(
                    "⚠ parallel conflict",
                    Style::default().fg(Color::Yellow),
                )),
            ),
            Some(ParallelMark::Safe) => detail(
                &mut items,
                Line::from(Span::styled(
                    "‖ parallel ok",
                    Style::default().fg(Color::Green),
                )),
            ),
            None => {}
        }
        if !t.body.trim().is_empty() {
            // List inner width: panel chrome (6) + highlight_symbol (2) + 1 col
            // of margin, so termimad doesn't overflow and re-wrap.
            body_lines = markdown_lines(&t.body, area.width.saturating_sub(9));
        }
        t.id.clone()
    } else {
        let p = Paragraph::new("select a task (j/k)")
            .style(Style::default().fg(SUBTLE))
            .block(panel_focus("Task", focused));
        f.render_widget(p, area);
        return;
    };

    // separator + cards
    detail(&mut items, Line::from(""));
    detail(
        &mut items,
        Line::from(Span::styled(
            "Items",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    );
    if board.cards.is_empty() {
        detail(
            &mut items,
            Line::from(Span::styled(
                "  (none) — p play · R review chat",
                Style::default().fg(SUBTLE),
            )),
        );
    } else {
        for (ci, card) in board.cards.iter().enumerate() {
            items.push(card_item(card));
            row_to_card.push(Some(ci));
        }
    }

    // task body (goal/description) below the Items.
    if !body_lines.is_empty() {
        items.push(ListItem::new(Line::from("")));
        row_to_card.push(None);
        for l in body_lines {
            items.push(ListItem::new(l));
            row_to_card.push(None);
        }
    }

    let selected_row = row_to_card
        .iter()
        .position(|r| *r == Some(board.card_selected));
    let mut state = ListState::default();
    state.select(selected_row);
    let hl = if focused {
        sel_style()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let list = List::new(items)
        .block(panel_focus(&title, focused))
        .highlight_style(hl)
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// A single card row in the middle column.
fn card_item(card: &CardView) -> ListItem<'static> {
    match card {
        CardView::Session {
            kind,
            live,
            last_activity_ms,
        } => {
            let (dot, color) = if *live {
                ("●", Color::Green)
            } else {
                ("✓", Color::DarkGray)
            };
            let age = if *live {
                format!("active · {}", fmt_dur(age_of_ms(*last_activity_ms)))
            } else {
                "closed".to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(
                    session_kind_label(*kind).to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {age}"), Style::default().fg(SUBTLE)),
            ]))
        }
        CardView::Verdict { clean } => {
            let (dot, color, txt) = if *clean {
                ("✓", Color::Green, "verdict (CLEAN)")
            } else {
                ("⚑", Color::Red, "verdict (DIRTY)")
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(
                    txt.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]))
        }
    }
}

/// Right panel: content of the selected card (session chat, verdict, or the
/// task detail if there's no card).
fn render_card_content(f: &mut Frame, snap: &DomainSnapshot, app: Option<&App>, area: Rect) {
    let board = &snap.board;
    let focused = board.focus == FocusId::Chat || board.chat_fullscreen;
    let idx = board.card_selected.min(board.cards.len().saturating_sub(1));
    match board.cards.get(idx) {
        Some(CardView::Session { .. }) => render_session_pane(f, app, area, focused),
        Some(CardView::Verdict { .. }) => {
            let lines = verdict_lines(board.review.as_ref());
            let title = match selected_task(snap) {
                Some(t) => format!("Verdict · {}", t.id),
                None => "Verdict".to_string(),
            };
            let p = Paragraph::new(lines)
                .block(panel_focus(&title, focused))
                .wrap(Wrap { trim: true });
            f.render_widget(p, area);
        }
        None => {
            // no card: task detail (or hint).
            let (title, lines) = match selected_task(snap) {
                Some(t) => (
                    format!("{} · Enter expand", t.id),
                    task_detail_lines(t, area.width.saturating_sub(7)),
                ),
                None => (
                    "Details".to_string(),
                    vec![Line::from(Span::styled(
                        "select a task (j/k)",
                        Style::default().fg(SUBTLE),
                    ))],
                ),
            };
            let p = Paragraph::new(lines)
                .block(panel_focus(&title, focused))
                .wrap(Wrap { trim: false });
            f.render_widget(p, area);
        }
    }
}

/// Renders the selected session: the local PTY when the `App` is at hand
/// (local mode), or a placeholder for socket clients (no session stream to
/// draw from on this side of the wire).
fn render_session_pane(f: &mut Frame, app: Option<&App>, area: Rect, focused: bool) {
    let Some(app) = app else {
        let block = if focused {
            panel_tight("chat — Esc back to cards").border_style(Style::default().fg(ACCENT))
        } else {
            panel_tight("chat")
        };
        let msg = "Session panel not available on this client.\nThe board stays fully usable; the chat runs where the daemon lives.";
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(SUBTLE))
                .block(block),
            area,
        );
        return;
    };
    let hint = if app.pending_prefix {
        "Ctrl+G… (n/p card · f finish · x remove · z zoom · g=Ctrl+G)"
    } else if focused {
        "typing in claude · Ctrl+G = jaum command"
    } else {
        "l/→ enter chat"
    };
    let block = if focused {
        panel_tight(&format!("chat — {hint}")).border_style(Style::default().fg(ACCENT))
    } else {
        panel_tight(&format!("chat — {hint}"))
    };
    match app.current_session_idx().and_then(|i| app.sessions.get(i)) {
        Some(e) if !e.is_live() => {
            let msg = "Session closed (history). No live terminal.\nContext is saved in claude; open a new play/review to continue.";
            f.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(SUBTLE))
                    .block(block),
                area,
            );
        }
        Some(e) => {
            let term = PseudoTerminal::new(e.parser.screen()).block(block);
            f.render_widget(term, area);
        }
        None => f.render_widget(Paragraph::new("").block(block), area),
    }
}

/// Verdict body (findings + constraints + criteria) for the right panel.
fn verdict_lines(review: Option<&ReviewView>) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let Some(r) = review else {
        lines.push(Line::from(
            "No review yet. The verdict runs by itself when every PR check turns green; `R` opens the review chat.",
        ));
        return lines;
    };
    lines.push(Line::from(vec![
        Span::raw("is_clean: "),
        if r.clean {
            Span::styled(
                "CLEAN",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                "DIRTY",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        },
    ]));
    lines.push(Line::from(Span::styled(
        format!(
            "{} blocking · {} finding(s) · minor/nit don't fail",
            r.blocking,
            r.findings.len()
        ),
        Style::default().fg(SUBTLE),
    )));
    // which commit this verdict was taken against, and when it ran.
    if let Some(tag) = join_shas(&r.reviewed_shas) {
        let mut line = format!("reviewed @{tag}");
        if let Some(at) = r.reviewed_at {
            line.push_str(&format!(" · {}", review_when(at)));
        }
        lines.push(Line::from(Span::styled(line, Style::default().fg(SUBTLE))));
    }
    if !r.clean {
        lines.push(Line::from(Span::styled(
            "`H` sends the open items to the play session to fix",
            Style::default().fg(SUBTLE),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Findings",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if r.findings.is_empty() {
        lines.push(Line::from("  (none)"));
    } else {
        for finding in &r.findings {
            lines.push(Line::from(format!("  {}", finding_line(finding))));
        }
    }
    push_verdict_section(&mut lines, "Constraints (enforce: review)", &r.constraints);
    push_verdict_section(&mut lines, "Acceptance criteria", &r.criteria);
    lines
}

fn severity_tag(s: SeverityId) -> &'static str {
    match s {
        SeverityId::Blocker => "BLOCKER",
        SeverityId::Major => "MAJOR",
        SeverityId::Minor => "MINOR",
        SeverityId::Nit => "NIT",
    }
}

/// Terminal rendering of a structured finding: `[SEV] file:line - message
/// (violates ref)`.
fn finding_line(f: &FindingView) -> String {
    let loc = match f.line {
        Some(l) => format!("{}:{}", f.file, l),
        None => f.file.clone(),
    };
    let tag = severity_tag(f.severity);
    match &f.reference {
        Some(r) => format!("[{tag}] {loc} - {} (violates {r})", f.message),
        None => format!("[{tag}] {loc} - {}", f.message),
    }
}

/// Age of an epoch-milliseconds instant. Tolerant of a clock that went
/// backwards (clamped to zero).
fn age_of_ms(ms: u64) -> std::time::Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    std::time::Duration::from_millis(now.saturating_sub(ms))
}

/// Short, readable duration: `8s`, `3m`, `1h2m`.
fn fmt_dur(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Pushes a header + the items of a checklist (constraint/criterion) with the
/// colored verdict.
fn push_verdict_section(lines: &mut Vec<Line>, title: &str, items: &[CheckView]) {
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        title.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(SUBTLE),
        )));
        return;
    }
    for c in items {
        let (tag, color) = match c.verdict {
            CheckVerdict::Ok => ("OK", Color::Green),
            CheckVerdict::Failed => ("FAILED", Color::Red),
            CheckVerdict::Pending => ("PENDING", Color::Yellow),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  [{tag}] "), Style::default().fg(color)),
            Span::raw(c.text.clone()),
        ]));
    }
}

fn render_docs(f: &mut Frame, docs: &DocsView, area: Rect) {
    if docs.list.is_empty() {
        let p = Paragraph::new(format!(
            "(no .md in {})\n\nWrite the design docs here; then press `i` to ingest.",
            docs.dir
        ))
        .block(panel("Docs"))
        .wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    }
    let cols = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)])
        .spacing(1)
        .split(area);
    render_docs_list(f, docs, cols[0]);
    render_docs_preview(f, docs, cols[1]);
}

fn render_docs_list(f: &mut Frame, docs: &DocsView, area: Rect) {
    let group_of = |rel: &str| -> String {
        rel.split_once('/')
            .map(|(g, _)| g.to_string())
            .unwrap_or_default()
    };
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_doc: Vec<Option<usize>> = Vec::new();
    let mut last_group: Option<String> = None;

    for (i, rel) in docs.list.iter().enumerate() {
        let group = group_of(rel);
        if last_group.as_deref() != Some(group.as_str()) {
            let label = if group.is_empty() {
                "(root)".to_string()
            } else {
                group.to_uppercase()
            };
            let count = docs.list.iter().filter(|d| group_of(d) == group).count();
            let color = doc_group_color(&group);
            items.push(ListItem::new(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(color)),
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {count}"), Style::default().fg(Color::DarkGray)),
            ])));
            row_to_doc.push(None);
            last_group = Some(group);
        }
        let file = rel.rsplit('/').next().unwrap_or(rel);
        items.push(ListItem::new(Line::from(format!("  {file}"))));
        row_to_doc.push(Some(i));
    }

    let selected_row = row_to_doc.iter().position(|r| *r == Some(docs.selected));
    let mut state = ListState::default();
    state.select(selected_row);

    let title = format!("Docs · {}", docs.list.len());
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Live preview (rendered markdown) of the selected doc.
fn render_docs_preview(f: &mut Frame, docs: &DocsView, area: Rect) {
    let (title, lines) = match docs.list.get(docs.selected) {
        Some(rel) => {
            let file = rel.rsplit('/').next().unwrap_or(rel).to_string();
            (
                format!("{file} · J/K scroll · Enter expand"),
                markdown_lines(&docs.preview, area.width.saturating_sub(7)),
            )
        }
        None => ("preview".to_string(), Vec::new()),
    };
    let p = Paragraph::new(lines)
        .block(panel(&title))
        .wrap(Wrap { trim: false })
        .scroll((docs.scroll, 0));
    f.render_widget(p, area);
}

/// Color per doc category (purely aesthetic).
fn doc_group_color(group: &str) -> Color {
    match group.to_lowercase().as_str() {
        "rfcs" | "rfc" => Color::Cyan,
        "adr" | "adrs" => Color::Magenta,
        "prd" | "prds" => Color::Green,
        "design" => Color::Yellow,
        _ => Color::Blue,
    }
}

/// Footer status text built from the domain snapshot: tab, selected row,
/// branch, overlap warning and this terminal's navigation hints. Presentation
/// belongs here, not on the wire.
fn statusline_text(snap: &DomainSnapshot) -> String {
    let mut s = format!(
        "[{}]",
        match snap.tab {
            TabId::Board => "Board",
            TabId::Docs => "Docs",
        }
    );
    // in-flight review first, so the narrow footer never truncates it away.
    if let Some(t) = snap
        .board
        .tasks
        .iter()
        .find(|t| t.review_progress == Some(ReviewProgressId::Running))
    {
        s.push_str(&format!(" ⟳ review {}", t.id));
    }
    if snap.board.project_selected {
        s.push_str(" · project");
    } else if let Some(t) = selected_task(snap) {
        s.push_str(&format!(" {}", t.id));
        if let Some(pr) = t.prs.first() {
            s.push_str(&format!(" {}", pr.branch));
        }
    }
    if let Some(o) = snap.board.overlaps.first() {
        s.push_str(&format!(" · ⚠ overlap {} ({}↔{})", o.repo, o.a, o.b));
    }
    // navigation hint depending on the focused panel (Board only).
    if snap.tab == TabId::Board {
        s.push_str(match snap.board.focus {
            FocusId::Tasks => "   h/l focus · l items · z zoom",
            FocusId::Cards => "   Enter chat · h back · z zoom",
            FocusId::Chat => "   Ctrl+G cmd · Ctrl+G z zoom",
        });
    }
    s
}

fn render_statusline(f: &mut Frame, snap: &DomainSnapshot, area: Rect) {
    // input mode: active prompt
    if let Some(input) = &snap.input {
        let label = match input.kind {
            InputKind::Defer => "defer",
            InputKind::Convention => "convention",
            InputKind::NewTask => "new task",
            InputKind::NewTaskClaude => "task (claude investigates)",
            InputKind::InitPath => "init (project path)",
        };
        let line = Line::from(vec![
            Span::styled(
                format!(" {label} ▸ "),
                Style::default()
                    .fg(BG_TITLE_FG)
                    .bg(PINK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {}", input.buffer)),
            Span::styled("█", Style::default().fg(PINK)),
            Span::styled("   Enter confirm · Esc cancel", Style::default().fg(SUBTLE)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    // statusline (left) + key caps (right)
    let keys = [
        ("p", "play"),
        ("R", "review"),
        ("f", "finish"),
        ("a", "parallel"),
        ("S", "setup"),
        ("i", "ingest"),
        ("I", "init"),
        ("N", "claude"),
        ("q", "quit"),
    ];
    let mut hint: Vec<Span> = Vec::new();
    for (k, label) in keys {
        hint.push(Span::styled(
            format!(" {k}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        hint.push(Span::styled(
            format!(" {label} "),
            Style::default().fg(SUBTLE),
        ));
    }

    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(84)]).split(area);
    f.render_widget(
        Paragraph::new(Span::styled(
            statusline_text(snap),
            Style::default().fg(SUBTLE),
        )),
        cols[0],
    );
    f.render_widget(Paragraph::new(Line::from(hint)).right_aligned(), cols[1]);
}

/// Temporary snackbar in the top-right corner (interaction feedback).
fn render_toast(f: &mut Frame, msg: &str) {
    use ratatui::widgets::Clear;
    let area = f.area();
    let w = (msg.chars().count() as u16 + 7)
        .min(area.width.saturating_sub(2))
        .max(14);
    let x = area.right().saturating_sub(w + 1);
    let y = area.top() + 4; // below the tab bar
    let rect = Rect {
        x,
        y,
        width: w,
        height: 3,
    };
    f.render_widget(Clear, rect);
    let p = Paragraph::new(Line::from(vec![
        Span::styled("● ", Style::default().fg(PINK)),
        Span::raw(msg.to_string()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .padding(Padding::horizontal(1)),
    )
    .style(Style::default().fg(Color::White).bg(Color::Rgb(34, 30, 52)))
    .wrap(Wrap { trim: true });
    f.render_widget(p, rect);
}

// Behavior tests live in-crate (not under tests/) so llvm-cov attributes the
// exercised lines to this file; the coverage tooling drops any path containing
// a `tests/` segment, which discards `#[path]` includes from integration tests.
#[cfg(test)]
#[path = "tui_tests.rs"]
mod tui_tests;

#[cfg(test)]
mod tests {
    use super::{encode_mouse_sgr, markdown_lines};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::text::Line;

    fn plain(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn markdown_strips_bold_markers() {
        // termimad renders bold and discards the `**`.
        let ls = markdown_lines("a **bold** word", 80);
        let s: String = ls.iter().map(plain).collect();
        assert!(s.contains("bold"));
        assert!(!s.contains("**"));
    }

    #[test]
    fn markdown_table_full_border_and_content() {
        let md = "| a | bb |\n|---|----|\n| 1 | 22 |";
        let ls = markdown_lines(md, 80);
        let s: String = ls.iter().map(plain).collect();
        assert!(s.contains("bb"));
        assert!(s.contains("22"));
        // border: top ╭ + header + separator ├ + row + base ╰.
        assert_eq!(ls.len(), 5);
        assert!(plain(&ls[0]).starts_with('╭'));
        assert!(plain(&ls[0]).contains('┬'));
        assert!(plain(&ls[2]).starts_with('├'));
        assert!(plain(&ls[4]).starts_with('╰'));
    }

    #[test]
    fn markdown_wide_table_wraps_per_cell() {
        let md = "| col |\n|-----|\n| a very long phrase that does not fit |";
        let ls = markdown_lines(md, 20);
        // top + header + separator + several cell lines + base.
        assert!(ls.len() > 5);
        assert!(plain(ls.first().unwrap()).starts_with('╭'));
        assert!(plain(ls.last().unwrap()).starts_with('╰'));
    }

    fn ev(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn encode_sgr_scroll_and_click() {
        // scroll up: button 64, suffix M
        assert_eq!(
            encode_mouse_sgr(&ev(MouseEventKind::ScrollUp), 5, 9).unwrap(),
            b"\x1b[<64;5;9M".to_vec()
        );
        // left press: cb 0, M
        assert_eq!(
            encode_mouse_sgr(&ev(MouseEventKind::Down(MouseButton::Left)), 2, 3).unwrap(),
            b"\x1b[<0;2;3M".to_vec()
        );
        // release: same cb, suffix m
        assert_eq!(
            encode_mouse_sgr(&ev(MouseEventKind::Up(MouseButton::Left)), 2, 3).unwrap(),
            b"\x1b[<0;2;3m".to_vec()
        );
        // pure movement encodes to nothing
        assert!(encode_mouse_sgr(&ev(MouseEventKind::Moved), 1, 1).is_none());
    }
}
